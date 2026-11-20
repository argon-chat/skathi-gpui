use std::{path::PathBuf, sync::Arc};

/// A system drag that originates in an embedded document. Native backends may
/// request generated file bytes after the source event has returned.
#[derive(Clone)]
pub struct PlatformDragData {
    /// Public MIME values and an optional opaque in-process session identifier.
    pub strings: Vec<(String, String)>,
    /// Files offered to a target in source order.
    pub files: Vec<PlatformDragFile>,
    /// Bitmask of allowed DropOperation values.
    pub allowed: u8,
    /// Optional pixels for the native drag preview.
    pub image: Option<Arc<PlatformDragImage>>,
}

/// A disk path or a lazily readable generated file in a drag.
#[derive(Clone)]
pub struct PlatformDragFile {
    /// Suggested leaf filename.
    pub name: String,
    /// Original path when the entire on-disk entry can be shared.
    pub path: Option<PathBuf>,
    /// Whether a path refers to a directory.
    pub directory: bool,
    /// Length of the promised contents.
    pub size: u64,
    /// Reads bytes beginning at an arbitrary file offset.
    pub read: Arc<dyn Fn(u64, &mut [u8]) -> std::io::Result<usize> + Send + Sync>,
}

impl PlatformDragFile {
    /// Write a generated file in bounded chunks when a native target requests it.
    pub fn write_to(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        let mut buffer = vec![0; 64 * 1024];
        let mut offset = 0u64;
        while offset < self.size {
            let want = (self.size - offset).min(buffer.len() as u64) as usize;
            let count = (self.read)(offset, &mut buffer[..want])?;
            if count == 0 || count > want {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            writer.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        Ok(())
    }
}

/// Straight-alpha RGBA bytes in device pixels, with the pointer's hotspot.
pub struct PlatformDragImage {
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
    /// RGBA pixels, one scanline after another.
    pub rgba: Vec<u8>,
    /// Offset of the pointer from the preview's top-left corner.
    pub hotspot: (i32, i32),
}
