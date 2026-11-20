//! OLE data for a document drag. File contents stay behind IStream until a target asks.
use gpui::{DropOperation, PlatformDragData, PlatformDragFile, PlatformDragImage};
use std::{
    ffi::c_void,
    mem::{ManuallyDrop, offset_of, size_of},
    os::windows::ffi::OsStrExt,
    sync::Mutex,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            Com::*, DataExchange::RegisterClipboardFormatW, Memory::*, Ole::*, SystemServices::*,
        },
        UI::Shell::*,
    },
    core::*,
};

#[implement(IDropSource)]
struct Source;
impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !keys.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

#[implement(IStream)]
struct FileStream {
    file: PlatformDragFile,
    position: Mutex<u64>,
}

impl FileStream {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut pos = self.position.lock().unwrap_or_else(|e| e.into_inner());
        let want = buf
            .len()
            .min(self.file.size.saturating_sub(*pos).min(usize::MAX as u64) as usize);
        let mut used = 0;
        while used < want {
            let end = (used + 65536).min(want);
            let n = (self.file.read)(*pos, &mut buf[used..end])?;
            if n == 0 || n > end - used {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            *pos += n as u64;
            used += n;
        }
        Ok(used)
    }
}
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, data: *mut c_void, len: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe { *read = 0 };
        }
        if len == 0 {
            return S_OK;
        }
        if data.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        match self.read(unsafe { std::slice::from_raw_parts_mut(data.cast(), len as usize) }) {
            Ok(n) => {
                if !read.is_null() {
                    unsafe { *read = n as u32 }
                };
                if n == len as usize { S_OK } else { S_FALSE }
            }
            Err(_) => STG_E_READFAULT,
        }
    }
    fn Write(&self, _: *const c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe { *written = 0 }
        };
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, by: i64, origin: STREAM_SEEK, new: *mut u64) -> Result<()> {
        let mut pos = self.position.lock().unwrap_or_else(|e| e.into_inner());
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *pos,
            STREAM_SEEK_END => self.file.size,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        *pos = base
            .checked_add_signed(by)
            .ok_or_else(|| Error::from_hresult(STG_E_INVALIDFUNCTION))?;
        if !new.is_null() {
            unsafe { *new = *pos }
        };
        Ok(())
    }
    fn SetSize(&self, _: u64) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        target: Ref<IStream>,
        amount: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> Result<()> {
        let mut consumed = 0u64;
        let mut sent = 0u64;
        let result = (|| -> Result<()> {
            let target = target.ok()?;
            let mut buffer = vec![0u8; 65536];
            while consumed < amount {
                let want = (amount - consumed).min(buffer.len() as u64) as usize;
                let n = self
                    .read(&mut buffer[..want])
                    .map_err(|_| Error::from_hresult(STG_E_READFAULT))?;
                if n == 0 {
                    break;
                }
                consumed += n as u64;
                let mut out = 0u32;
                unsafe {
                    target
                        .Write(buffer.as_ptr().cast(), n as u32, Some(&mut out))
                        .ok()?
                };
                sent += u64::from(out);
                if out != n as u32 {
                    return Err(STG_E_WRITEFAULT.into());
                }
            }
            Ok(())
        })();
        if !read.is_null() {
            unsafe { *read = consumed }
        };
        if !written.is_null() {
            unsafe { *written = sent }
        };
        result
    }
    fn Commit(&self, _: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, stat: *mut STATSTG, _: &STATFLAG) -> Result<()> {
        if stat.is_null() {
            return Err(STG_E_INVALIDPOINTER.into());
        }
        unsafe {
            *stat = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.file.size,
                ..Default::default()
            }
        };
        Ok(())
    }
    fn Clone(&self) -> Result<IStream> {
        Ok(FileStream {
            file: self.file.clone(),
            position: Mutex::new(*self.position.lock().unwrap_or_else(|e| e.into_inner())),
        }
        .into())
    }
}

// The shell data object keys by clipboard format and ignores lindex. Keep
// the shell's enumerator and drag-image formats, but serve each promised
// file from its own fresh seekable stream.
#[implement(IDataObject)]
struct IndexedData {
    shell: IDataObject,
    contents: u16,
    files: Vec<PlatformDragFile>,
}

impl IndexedData {
    fn file(&self, format: &FORMATETC) -> Result<&PlatformDragFile> {
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT.into());
        }
        if format.tymed & TYMED_ISTREAM.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        let index = if self.files.len() == 1 && format.lindex == -1 {
            0
        } else {
            format.lindex
        };
        usize::try_from(index)
            .ok()
            .and_then(|i| self.files.get(i))
            .filter(|file| !file.directory)
            .ok_or_else(|| DV_E_LINDEX.into())
    }
}

fn content_medium(file: &PlatformDragFile) -> STGMEDIUM {
    let stream: IStream = FileStream {
        file: file.clone(),
        position: Mutex::new(0),
    }
    .into();
    STGMEDIUM {
        tymed: TYMED_ISTREAM.0 as u32,
        u: STGMEDIUM_0 {
            pstm: ManuallyDrop::new(Some(stream)),
        },
        ..Default::default()
    }
}

impl IDataObject_Impl for IndexedData_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        let f = unsafe { format.as_ref() }.ok_or_else(|| Error::from_hresult(E_POINTER))?;
        if f.cfFormat == self.contents {
            return self.file(f).map(content_medium);
        }
        unsafe { self.shell.GetData(format) }
    }
    fn GetDataHere(&self, format: *const FORMATETC, medium: *mut STGMEDIUM) -> Result<()> {
        if unsafe { format.as_ref() }.is_some_and(|f| f.cfFormat == self.contents) {
            return Err(DV_E_FORMATETC.into());
        }
        unsafe { self.shell.GetDataHere(format, medium) }
    }
    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(f) = (unsafe { format.as_ref() }) else {
            return E_POINTER;
        };
        if f.cfFormat == self.contents {
            if f.lindex == -1
                && f.tymed & TYMED_ISTREAM.0 as u32 != 0
                && f.dwAspect == DVASPECT_CONTENT.0
            {
                return S_OK;
            }
            return self.file(f).map_or_else(|e| e.code(), |_| S_OK);
        }
        unsafe { self.shell.QueryGetData(format) }
    }
    fn GetCanonicalFormatEtc(&self, input: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        unsafe { self.shell.GetCanonicalFormatEtc(input, output) }
    }
    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> Result<()> {
        unsafe { self.shell.SetData(format, medium, release.as_bool()) }
    }
    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        unsafe { self.shell.EnumFormatEtc(direction) }
    }
    fn DAdvise(&self, format: *const FORMATETC, flags: u32, sink: Ref<IAdviseSink>) -> Result<u32> {
        unsafe { self.shell.DAdvise(format, flags, sink.ok()?) }
    }
    fn DUnadvise(&self, connection: u32) -> Result<()> {
        unsafe { self.shell.DUnadvise(connection) }
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        unsafe { self.shell.EnumDAdvise() }
    }
}

fn fmt(id: u16, index: i32, medium: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: medium.0 as u32,
        ..Default::default()
    }
}

unsafe fn register(name: &str) -> Result<u16> {
    unsafe {
        let wide = HSTRING::from(name);
        let id = RegisterClipboardFormatW(&wide);
        if id == 0 {
            Err(Error::from_thread())
        } else {
            Ok(id as u16)
        }
    }
}

unsafe fn set_medium(object: &IDataObject, format: FORMATETC, mut medium: STGMEDIUM) -> Result<()> {
    unsafe {
        if let Err(e) = object.SetData(&format, &medium, true) {
            ReleaseStgMedium(&mut medium);
            return Err(e);
        }
        Ok(())
    }
}

unsafe fn set_bytes(object: &IDataObject, id: u16, bytes: &[u8]) -> Result<()> {
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let raw = GlobalLock(global);
        if raw.is_null() {
            let _ = GlobalFree(Some(global));
            return Err(Error::from_thread());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), raw.cast(), bytes.len());
        let _ = GlobalUnlock(global);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
            ..Default::default()
        };
        set_medium(object, fmt(id, -1, TYMED_HGLOBAL), medium)
    }
}

fn unicode(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn descriptors(files: &[PlatformDragFile]) -> Vec<u8> {
    let mut out = vec![0u8; 4 + files.len() * size_of::<FILEDESCRIPTORW>()];
    out[..4].copy_from_slice(&(files.len() as u32).to_le_bytes());
    for (i, f) in files.iter().enumerate() {
        let d = &mut out[4 + i * size_of::<FILEDESCRIPTORW>()..];
        let put = |d: &mut [u8], at: usize, n: u32| d[at..at + 4].copy_from_slice(&n.to_le_bytes());
        put(
            d,
            offset_of!(FILEDESCRIPTORW, dwFlags),
            FD_FILESIZE.0 as u32 | FD_ATTRIBUTES.0 as u32 | FD_PROGRESSUI.0 as u32,
        );
        put(
            d,
            offset_of!(FILEDESCRIPTORW, dwFileAttributes),
            if f.directory { 0x10 } else { 0x80 },
        );
        put(
            d,
            offset_of!(FILEDESCRIPTORW, nFileSizeHigh),
            (f.size >> 32) as u32,
        );
        put(d, offset_of!(FILEDESCRIPTORW, nFileSizeLow), f.size as u32);
        let mut at = offset_of!(FILEDESCRIPTORW, cFileName);
        let mut name: String = f
            .name
            .chars()
            .map(|c| {
                if c.is_control() || "/\\:*?\"<>|".contains(c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let clean = name.trim_end_matches([' ', '.']);
        let stem = clean
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if clean.is_empty()
            || ["CON", "NUL", "AUX", "PRN", "CONIN$", "CONOUT$"].contains(&stem.as_str())
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
        {
            name = format!("file-{i}");
        } else {
            name.truncate(clean.len());
        }
        for c in name.chars() {
            let mut buf = [0u16; 2];
            let unit = c.encode_utf16(&mut buf);
            if at + 2 * unit.len() > size_of::<FILEDESCRIPTORW>() - 2 {
                break;
            }
            for n in unit {
                d[at..at + 2].copy_from_slice(&n.to_le_bytes());
                at += 2;
            }
        }
    }
    out
}

fn html_bytes(fragment: &str) -> Vec<u8> {
    let head = |a: usize, b: usize, c: usize, d: usize| {
        format!(
            "Version:1.0\r\nStartHTML:{a:010}\r\nEndHTML:{b:010}\r\nStartFragment:{c:010}\r\nEndFragment:{d:010}\r\n"
        )
    };
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let a = head(0, 0, 0, 0).len();
    let c = a + prefix.len();
    let d = c + fragment.len();
    let b = d + suffix.len();
    format!("{}{prefix}{fragment}{suffix}\0", head(a, b, c, d)).into_bytes()
}

unsafe fn object_for(data: &PlatformDragData) -> Result<IDataObject> {
    unsafe {
        let object: IDataObject = SHCreateDataObject(None, None, None::<&IDataObject>)?;
        for (mime, text) in &data.strings {
            if mime == "text/plain" {
                set_bytes(&object, CF_UNICODETEXT.0, &unicode(text))?
            }
            if mime == "text/uri-list" {
                if let Some(url) = text.lines().find(|s| !s.is_empty() && !s.starts_with('#')) {
                    set_bytes(&object, register("UniformResourceLocatorW")?, &unicode(url))?;
                }
            }
            if mime == "text/html" {
                set_bytes(&object, register("HTML Format")?, &html_bytes(text))?
            }
            if mime.contains('/') && mime.len() <= 200 && !mime.contains('\0') {
                let mut bytes = text.as_bytes().to_vec();
                bytes.push(0);
                set_bytes(&object, register(mime)?, &bytes)?;
            }
        }
        if data.files.is_empty() {
            return Ok(object);
        }
        if data.files.iter().all(|f| f.path.is_some()) {
            let base = size_of::<DROPFILES>();
            let mut list = vec![0u8; base];
            list[..4].copy_from_slice(&(base as u32).to_le_bytes());
            list[16..20].copy_from_slice(&1u32.to_le_bytes());
            for file in &data.files {
                list.extend(
                    file.path
                        .as_ref()
                        .unwrap()
                        .as_os_str()
                        .encode_wide()
                        .chain([0])
                        .flat_map(u16::to_le_bytes),
                );
            }
            list.extend([0u8; 2]);
            set_bytes(&object, CF_HDROP.0, &list)?;
        } else {
            set_bytes(
                &object,
                register("FileGroupDescriptorW")?,
                &descriptors(&data.files),
            )?;
            let id = register("FileContents")?;
            if let Some(file) = data.files.iter().find(|file| !file.directory) {
                set_medium(&object, fmt(id, -1, TYMED_ISTREAM), content_medium(file))?;
                return Ok(IndexedData {
                    shell: object,
                    contents: id,
                    files: data.files.clone(),
                }
                .into());
            }
        }
        Ok(object)
    }
}

unsafe fn image_for(object: &IDataObject, image: &PlatformDragImage) -> Result<()> {
    unsafe {
        let (w, h) = (image.width, image.height);
        if w == 0
            || h == 0
            || w > 4096
            || h > 4096
            || image.rgba.len() != w as usize * h as usize * 4
        {
            return Ok(());
        }
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let mut raw = std::ptr::null_mut();
        let bmp = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut raw, None, 0)?;
        if raw.is_null() {
            let _ = DeleteObject(bmp.into());
            return Err(E_OUTOFMEMORY.into());
        }
        let out = std::slice::from_raw_parts_mut(raw.cast::<u8>(), image.rgba.len());
        for (i, o) in image.rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
            o.copy_from_slice(&[i[2], i[1], i[0], i[3]]);
        }
        let helper: Result<IDragSourceHelper> =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER);
        let result = helper.and_then(|helper| {
            helper.InitializeFromBitmap(
                &SHDRAGIMAGE {
                    sizeDragImage: SIZE {
                        cx: w as i32,
                        cy: h as i32,
                    },
                    ptOffset: POINT {
                        x: image.hotspot.0,
                        y: image.hotspot.1,
                    },
                    hbmpDragImage: bmp,
                    crColorKey: COLORREF(0xffff_ffff),
                },
                object,
            )
        });
        if result.is_err() {
            let _ = DeleteObject(bmp.into());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Ole;
    impl Drop for Ole {
        fn drop(&mut self) {
            unsafe { OleUninitialize() }
        }
    }
    #[test]
    fn ole_serves_virtual_items_by_index_only_when_the_target_reads() {
        unsafe { OleInitialize(None).unwrap() };
        let _ole = Ole;
        let reads = Arc::new(AtomicUsize::new(0));
        let files = [
            ("a.txt", b"first".as_slice()),
            ("b.txt", b"second".as_slice()),
        ]
        .into_iter()
        .map(|(name, bytes)| {
            let count = reads.clone();
            PlatformDragFile {
                name: name.into(),
                path: None,
                directory: false,
                size: bytes.len() as u64,
                read: Arc::new(move |at, out| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let source = bytes.get(at as usize..).unwrap_or_default();
                    let n = source.len().min(out.len());
                    out[..n].copy_from_slice(&source[..n]);
                    Ok(n)
                }),
            }
        })
        .collect();
        let data = PlatformDragData {
            files,
            strings: vec![("text/plain".into(), "hello".into())],
            allowed: 3,
            image: None,
        };
        unsafe {
            let object = object_for(&data).unwrap();
            let mut group = object
                .GetData(&fmt(
                    register("FileGroupDescriptorW").unwrap(),
                    -1,
                    TYMED_HGLOBAL,
                ))
                .unwrap();
            let p = GlobalLock(group.u.hGlobal).cast::<u32>();
            assert!(!p.is_null());
            assert_eq!(*p, 2);
            let _ = GlobalUnlock(group.u.hGlobal);
            ReleaseStgMedium(&mut group);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            for (i, expected) in [(1, "second"), (0, "first")] {
                let mut medium = object
                    .GetData(&fmt(register("FileContents").unwrap(), i, TYMED_ISTREAM))
                    .unwrap();
                let stream = (*medium.u.pstm).as_ref().unwrap();
                let mut out = [0u8; 16];
                let mut n = 0u32;
                stream
                    .Read(out.as_mut_ptr().cast(), out.len() as u32, Some(&mut n))
                    .ok()
                    .unwrap();
                assert_eq!(&out[..n as usize], expected.as_bytes());
                ReleaseStgMedium(&mut medium);
            }
            assert!(reads.load(Ordering::SeqCst) >= 2);
        }
    }
}

pub(crate) fn perform(data: PlatformDragData) -> DropOperation {
    unsafe {
        let Ok(object) = object_for(&data) else {
            return DropOperation::None;
        };
        if let Some(i) = &data.image {
            let _ = image_for(&object, i);
        }
        let mut effect = DROPEFFECT_NONE;
        let source: IDropSource = Source.into();
        if DoDragDrop(
            &object,
            &source,
            DROPEFFECT(data.allowed as u32),
            &mut effect,
        ) != DRAGDROP_S_DROP
        {
            return DropOperation::None;
        }
        match effect {
            DROPEFFECT_COPY => DropOperation::Copy,
            DROPEFFECT_MOVE => DropOperation::Move,
            DROPEFFECT_LINK => DropOperation::Link,
            _ => DropOperation::None,
        }
    }
}
