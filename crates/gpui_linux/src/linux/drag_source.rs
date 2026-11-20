//! MIME streams offered through both Linux desktop drag protocols.
use gpui::{PlatformDragData, PlatformDragFile};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use url::Url;

pub(super) const MARKER: &str = "application/x-skathi-drag-session";
pub(super) const URI: &str = "text/uri-list";
const UTF8: &str = "text/plain;charset=utf-8";

pub(super) struct Offer {
    strings: BTreeMap<String, String>,
    files: Vec<PlatformDragFile>,
    uris: OnceLock<Result<Vec<u8>, String>>,
}

impl Offer {
    pub fn document(data: &PlatformDragData) -> Arc<Self> {
        let mut strings: BTreeMap<_, _> = data.strings.iter().cloned().collect();
        if let Some(text) = strings.get("text/plain").cloned() {
            strings.insert(UTF8.into(), text);
        }
        Arc::new(Self {
            strings,
            files: data.files.clone(),
            uris: OnceLock::new(),
        })
    }
    pub fn paths(list: String) -> Arc<Self> {
        Arc::new(Self {
            strings: BTreeMap::from([(URI.into(), list)]),
            files: vec![],
            uris: OnceLock::new(),
        })
    }
    pub fn types(&self) -> Vec<&str> {
        let mut types: Vec<_> = self.strings.keys().map(String::as_str).collect();
        if !self.files.is_empty() && !self.strings.contains_key(URI) {
            types.push(URI);
        }
        types
    }
    pub fn bytes(&self, mime: &str) -> io::Result<Vec<u8>> {
        if mime == URI && !self.files.is_empty() {
            self.uris
                .get_or_init(|| self.file_uris().map_err(|e| e.to_string()))
                .as_ref()
                .cloned()
                .map_err(|e| io::Error::other(e.clone()))
        } else {
            self.strings
                .get(mime)
                .map(|s| s.as_bytes().to_vec())
                .ok_or(io::ErrorKind::NotFound.into())
        }
    }
    fn file_uris(&self) -> io::Result<Vec<u8>> {
        let mut dir = None;
        let mut uris = self.strings.get(URI).cloned().unwrap_or_default();
        if !uris.is_empty() && !uris.ends_with('\n') {
            uris.push_str("\r\n");
        }
        for (index, file) in self.files.iter().enumerate() {
            let path = if let Some(path) = &file.path {
                path.clone()
            } else {
                if dir.is_none() {
                    let mut builder = tempfile::Builder::new();
                    builder.prefix("skathi-drag-");
                    dir = Some(match std::env::var_os("XDG_RUNTIME_DIR") {
                        Some(runtime) => builder.tempdir_in(runtime)?,
                        None => builder.tempdir()?,
                    });
                }
                let sub = dir.as_ref().unwrap().path().join(index.to_string());
                std::fs::create_dir(&sub)?;
                let mut name = file.name.replace(['/', '\\', '\0'], "_");
                if name.trim_matches([' ', '.']).is_empty() {
                    name = "file".into();
                }
                let path = sub.join(name);
                let mut writer = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_to(&mut writer)?;
                path
            };
            let url = Url::from_file_path(&path).map_err(|_| io::ErrorKind::InvalidInput)?;
            uris.push_str(url.as_str());
            uris.push_str("\r\n");
        }
        if let Some(dir) = dir {
            retain_files(dir)?;
        }
        Ok(uris.into_bytes())
    }
}

// A file manager reads the URI list before it opens the paths, sometimes
// after DndFinished. Keep private directories alive independently of sessions.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn document_files_are_materialized_once_when_a_target_requests_uris() {
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let offer = Offer::document(&PlatformDragData {
            allowed: 3,
            image: None,
            strings: vec![("text/plain".into(), "label".into())],
            files: vec![PlatformDragFile {
                name: "../drawing.png".into(),
                path: None,
                directory: false,
                size: 3,
                read: Arc::new(move |pos, out| {
                    count.fetch_add(1, Ordering::SeqCst);
                    let bytes = &b"abc"[pos as usize..];
                    let n = out.len().min(bytes.len());
                    out[..n].copy_from_slice(&bytes[..n]);
                    Ok(n)
                }),
            }],
        });
        assert!(offer.types().contains(&URI));
        assert_eq!(offer.bytes(UTF8).unwrap(), b"label");
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        let uris = offer.bytes(URI).unwrap();
        let path = Url::parse(std::str::from_utf8(&uris).unwrap().trim())
            .unwrap()
            .to_file_path()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("drawing.png")
        );
        assert_eq!(offer.bytes(URI).unwrap(), uris);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}

fn retain_files(dir: TempDir) -> io::Result<()> {
    static LEASES: Mutex<Vec<(Instant, PathBuf)>> = Mutex::new(Vec::new());
    static CLEANER: OnceLock<bool> = OnceLock::new();
    let started = CLEANER.get_or_init(|| {
        std::thread::Builder::new()
            .name("drag file cleanup".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                    let now = Instant::now();
                    let expired: Vec<_> = {
                        let mut leases = LEASES.lock().unwrap_or_else(|e| e.into_inner());
                        leases
                            .extract_if(.., |(until, _)| *until <= now)
                            .map(|(_, dir)| dir)
                            .collect()
                    };
                    for path in expired {
                        let _ = std::fs::remove_dir_all(path);
                    }
                }
            })
            .is_ok()
    });
    if !started {
        return Err(io::Error::other("could not retain generated drag files"));
    }
    LEASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((Instant::now() + Duration::from_secs(3600), dir.keep()));
    Ok(())
}
