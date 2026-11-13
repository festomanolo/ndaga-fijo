//! Block device abstraction. Read-only by default; writes need explicit opt-in
//! and are bounds-checked against the device (or partition window) size.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("access out of bounds: offset {offset} + len {len} > size {size}")]
    OutOfBounds { offset: u64, len: u64, size: u64 },
    #[error("device is read-only")]
    ReadOnly,
    #[error("invalid window: offset {offset} + len {len} exceeds parent size {size}")]
    InvalidWindow { offset: u64, len: u64, size: u64 },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, BlockError>;

pub trait BlockDevice: Send + Sync {
    /// Total size in bytes.
    fn size(&self) -> u64;
    fn is_read_only(&self) -> bool;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()>;
}

fn check(offset: u64, len: usize, size: u64) -> Result<()> {
    let len = len as u64;
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(BlockError::OutOfBounds { offset, len, size }),
    }
}

/// Disk image (raw file) backend.
pub struct ImageDevice {
    file: File,
    size: u64,
    read_only: bool,
}

/// Size of a regular file, or of a block/character device (whose `metadata().len()` is 0).
fn device_size(file: &File) -> io::Result<u64> {
    use std::os::unix::io::AsRawFd;
    let len = file.metadata()?.len();
    if len > 0 { return Ok(len); }
    #[cfg(target_os = "macos")]
    {
        // DKIOCGETBLOCKSIZE / DKIOCGETBLOCKCOUNT
        let (mut bs, mut count) = (0u32, 0u64);
        let fd = file.as_raw_fd();
        let ok = unsafe { libc::ioctl(fd, 0x4004_6418, &mut bs) == 0 && libc::ioctl(fd, 0x4008_6419, &mut count) == 0 };
        if ok && bs > 0 { return Ok(bs as u64 * count); }
    }
    #[cfg(target_os = "linux")]
    {
        // BLKGETSIZE64 (0x80081272 on x86_64/aarch64 Linux) returns size in bytes as u64
        let mut bytes = 0u64;
        let fd = file.as_raw_fd();
        let ok = unsafe { libc::ioctl(fd, 0x8008_1272u32 as libc::Ioctl, &mut bytes) == 0 };
        if ok && bytes > 0 { return Ok(bytes); }
        let end = unsafe { libc::lseek(fd, 0, libc::SEEK_END) };
        if end > 0 {
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET); }
            return Ok(end as u64);
        }
    }
    let _ = file.as_raw_fd();
    Ok(0)
}

impl ImageDevice {
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path)?;
        let size = device_size(&file)?;
        Ok(Self { file, size, read_only: true })
    }

    /// Explicit write authorization: the caller must opt in by name.
    pub fn open_read_write(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = device_size(&file)?;
        Ok(Self { file, size, read_only: false })
    }
}

impl BlockDevice for ImageDevice {
    fn size(&self) -> u64 { self.size }
    fn is_read_only(&self) -> bool { self.read_only }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check(offset, buf.len(), self.size)?;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        if self.read_only { return Err(BlockError::ReadOnly); }
        check(offset, data.len(), self.size)?;
        self.file.write_all_at(data, offset)?;
        Ok(())
    }
}

/// A bounded window onto a parent device (a partition). Drivers receive this,
/// so they physically cannot touch bytes outside their partition.
pub struct PartitionWindow {
    parent: Arc<dyn BlockDevice>,
    offset: u64,
    len: u64,
}

impl PartitionWindow {
    pub fn new(parent: Arc<dyn BlockDevice>, offset: u64, len: u64) -> Result<Self> {
        match offset.checked_add(len) {
            Some(end) if end <= parent.size() => Ok(Self { parent, offset, len }),
            _ => Err(BlockError::InvalidWindow { offset, len, size: parent.size() }),
        }
    }
}

impl BlockDevice for PartitionWindow {
    fn size(&self) -> u64 { self.len }
    fn is_read_only(&self) -> bool { self.parent.is_read_only() }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check(offset, buf.len(), self.len)?;
        self.parent.read_at(self.offset + offset, buf)
    }
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        check(offset, data.len(), self.len)?;
        self.parent.write_at(self.offset + offset, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn image(size: usize) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&vec![0u8; size]).unwrap();
        f
    }

    #[test]
    fn read_only_rejects_writes() {
        let f = image(4096);
        let d = ImageDevice::open_read_only(f.path()).unwrap();
        assert!(matches!(d.write_at(0, &[1]), Err(BlockError::ReadOnly)));
    }

    #[test]
    fn bounds_checked() {
        let f = image(1024);
        let d = ImageDevice::open_read_only(f.path()).unwrap();
        let mut b = [0u8; 16];
        assert!(d.read_at(1008, &mut b).is_ok());
        assert!(matches!(d.read_at(1009, &mut b), Err(BlockError::OutOfBounds { .. })));
        assert!(matches!(d.read_at(u64::MAX, &mut b), Err(BlockError::OutOfBounds { .. })));
    }

    #[test]
    fn window_confines_writes() {
        let f = image(4096);
        let d: Arc<dyn BlockDevice> = Arc::new(ImageDevice::open_read_write(f.path()).unwrap());
        let w = PartitionWindow::new(d.clone(), 1024, 512).unwrap();
        w.write_at(0, &[7; 4]).unwrap();
        assert!(w.write_at(510, &[1; 4]).is_err());
        let mut b = [0u8; 4];
        d.read_at(1024, &mut b).unwrap();
        assert_eq!(b, [7; 4]);
        d.read_at(1020, &mut b).unwrap();
        assert_eq!(b, [0; 4]);
        assert!(PartitionWindow::new(d, 4000, 200).is_err());
    }
}
