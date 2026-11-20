use std::{
    ffi::{CStr, CString, OsStr},
    fs::OpenOptions,
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr::NonNull,
};

use gpui::{DropOperation, PlatformDragData, PlatformDragFile, PlatformDragImage};
use image::ImageEncoder;
use objc2::{
    AnyThread, DefinedClass, Message, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSDragOperation, NSDraggingItem, NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSImage,
    NSPasteboardItem, NSPasteboardWriting, NSWorkspace,
};
use objc2_foundation::{
    NSData, NSError, NSMutableArray, NSObject, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect,
    NSSize, NSString, NSURL,
};
use objc2_uniform_type_identifiers::{UTType, UTTypeData};

pub(super) const MARKER: &str = "application/x-skathi-drag-session";
pub(super) const NATIVE_MARKER: &str = "org.skathi.document-drag";

pub(super) struct Session {
    pub allowed: u8,
    pub token: Option<String>,
    pub done: Box<dyn FnOnce(DropOperation)>,
}

pub(super) fn effect(operation: NSDragOperation) -> DropOperation {
    if operation.contains(NSDragOperation::Copy) {
        DropOperation::Copy
    } else if operation.contains(NSDragOperation::Move) {
        DropOperation::Move
    } else if operation.contains(NSDragOperation::Link) {
        DropOperation::Link
    } else {
        DropOperation::None
    }
}

struct PromiseIvars {
    file: PlatformDragFile,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "GPUIDocumentFilePromiseDelegate"]
    #[ivars = PromiseIvars]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _: &NSFilePromiseProvider, _: &NSString) -> Retained<NSString> {
            NSString::from_str(&self.ivars().file.name)
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &block2::DynBlock<dyn Fn(*mut NSError)>,
        ) {
            // AppKit supplies the complete destination URL, including the chosen filename.
            let path = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
            let path = Path::new(OsStr::from_bytes(path.to_bytes()));
            let result = write_promise(&self.ivars().file, path);
            let error = result.err().map(|e| unsafe {
                NSError::errorWithDomain_code_userInfo(
                    &NSString::from_str("NSPOSIXErrorDomain"),
                    e.raw_os_error().unwrap_or(5) as isize,
                    None,
                )
            });
            completion.call((error.as_ref().map_or(std::ptr::null_mut(), |error| {
                Retained::as_ptr(error).cast_mut()
            }),));
        }
    }
);

fn write_promise(file: &PlatformDragFile, path: &Path) -> io::Result<()> {
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = file.write_to(&mut output);
    drop(output);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn promise(file: &PlatformDragFile) -> Retained<NSFilePromiseProvider> {
    let mut file = file.clone();
    file.name = file.name.replace(['/', ':', '\0'], "_");
    if file.name.trim_matches([' ', '.']).is_empty() {
        file.name = "file".into();
    }
    let kind = Path::new(&file.name)
        .extension()
        .and_then(OsStr::to_str)
        .and_then(|ext| UTType::typeWithFilenameExtension(&NSString::from_str(ext)))
        .unwrap_or_else(|| unsafe { UTTypeData.retain() });
    let queue = NSOperationQueue::new();
    queue.setMaxConcurrentOperationCount(1);
    let delegate = PromiseDelegate::alloc().set_ivars(PromiseIvars { file, queue });
    let delegate: Retained<PromiseDelegate> = unsafe { msg_send![super(delegate), init] };
    let provider = NSFilePromiseProvider::initWithFileType_delegate(
        NSFilePromiseProvider::alloc(),
        &kind.identifier(),
        ProtocolObject::from_ref(&*delegate),
    );
    // delegate is weak. AppKit retains the provider for as long as Finder
    // may request the promise, including after the drag session has ended.
    unsafe { provider.setUserInfo(Some(&delegate)) };
    provider
}

fn preview(image: &PlatformDragImage) -> Option<Retained<NSImage>> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &image.rgba,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    NSImage::initWithData(NSImage::alloc(), &NSData::from_vec(png))
}

pub(super) fn items(
    data: &PlatformDragData,
    at: NSPoint,
    scale: f64,
) -> Retained<NSMutableArray<NSDraggingItem>> {
    let list = NSMutableArray::<NSDraggingItem>::new();
    let custom = data.image.as_deref().and_then(|image| {
        let size = NSSize::new(image.width as f64 / scale, image.height as f64 / scale);
        let origin = NSPoint::new(
            at.x - image.hotspot.0 as f64 / scale,
            at.y - size.height + image.hotspot.1 as f64 / scale,
        );
        Some((preview(image)?, NSRect::new(origin, size)))
    });
    let add = |writer: &ProtocolObject<dyn NSPasteboardWriting>| {
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        let (image, frame) = match custom.as_ref() {
            Some((image, frame)) => (image.clone(), *frame),
            None => (
                NSWorkspace::sharedWorkspace().iconForContentType(unsafe { UTTypeData }),
                NSRect::new(NSPoint::new(at.x - 16., at.y - 16.), NSSize::new(32., 32.)),
            ),
        };
        unsafe { item.setDraggingFrame_contents(frame, Some(&image)) };
        list.addObject(&item);
    };
    let strings = NSPasteboardItem::new();
    let mut public_text = false;
    for (mime, text) in &data.strings {
        let ty = match mime.as_str() {
            "text/plain" => "public.utf8-plain-text",
            "text/uri-list" => "public.url",
            "text/html" => "public.html",
            MARKER if data.files.is_empty() => NATIVE_MARKER,
            _ => continue,
        };
        public_text |=
            strings.setString_forType(&NSString::from_str(text), &NSString::from_str(ty));
    }
    let mut added_strings = false;
    for file in &data.files {
        if let Some(path) = &file.path {
            let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
                continue;
            };
            let url = unsafe {
                NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                    NonNull::new_unchecked(path.as_ptr().cast_mut()),
                    file.directory,
                    None,
                )
            };
            if public_text
                && !added_strings
                && let Some(url) = url.absoluteString()
            {
                strings.setString_forType(&url, &NSString::from_str("public.file-url"));
                add(ProtocolObject::from_ref(&*strings));
                added_strings = true;
            } else {
                add(ProtocolObject::from_ref(&*url));
            }
        } else {
            add(ProtocolObject::from_ref(&*promise(file)));
        }
    }
    if public_text && !added_strings {
        add(ProtocolObject::from_ref(&*strings));
    }
    list
}
