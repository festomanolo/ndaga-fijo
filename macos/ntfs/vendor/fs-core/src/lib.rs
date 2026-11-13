//! Common filesystem interface. Drivers report exactly what they support via
//! `Capabilities`; unsupported operations return `FsError::Unsupported`
//! rather than being silently emulated.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("is a directory: {0}")]
    IsADirectory(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid name: {0}")]
    InvalidName(String),
    #[error("directory not empty: {0}")]
    NotEmpty(String),
    #[error("no space left on volume")]
    NoSpace,
    #[error("write refused: {0}")]
    Refused(&'static str),
    #[error("source read failed: {0}")]
    Source(String),
    #[error("corrupt filesystem: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Block(#[from] block_device::BlockError),
}
pub type Result<T> = std::result::Result<T, FsError>;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Capabilities {
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub delete: bool,
    pub rename: bool,
    pub permissions: bool,
    pub symbolic_links: bool,
    pub extended_attributes: bool,
}

impl Capabilities {
    pub const READ_ONLY: Self = Self {
        read: true, write: false, create: false, delete: false, rename: false,
        permissions: false, symbolic_links: false, extended_attributes: false,
    };
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind { File, Directory }

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Seconds since Unix epoch, `None` if the filesystem has no value.
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub hidden: bool,
    pub read_only: bool,
    /// Driver-private locator (e.g. first cluster); opaque to callers.
    pub inode: u64,
}

pub trait Filesystem: Send + Sync {
    fn name(&self) -> &'static str;
    fn label(&self) -> Option<String>;
    fn capabilities(&self) -> Capabilities;
    fn stat(&self, path: &str) -> Result<DirEntry>;
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>>;
    /// Streaming positional read; returns bytes read (0 at EOF).
    fn read_at(&self, entry: &DirEntry, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// If the volume's state forbids writing (dirty, hibernated, ...), why. `None` = writable when the device is.
    fn write_blocked(&self) -> Option<&'static str> { None }

    // ---- mutation: drivers that cannot do these must say so, never emulate ----
    fn create_dir(&self, _path: &str) -> Result<DirEntry> { Err(FsError::Unsupported("create_dir")) }
    /// Streams `src` into a file. Fails with `AlreadyExists` unless `overwrite`.
    /// On any failure partway, allocated space is released.
    fn write_file(&self, _path: &str, _src: &mut dyn std::io::Read, _overwrite: bool) -> Result<DirEntry> {
        Err(FsError::Unsupported("write_file"))
    }
    /// Removes a file or an EMPTY directory.
    fn remove(&self, _path: &str) -> Result<()> { Err(FsError::Unsupported("remove")) }
    fn rename(&self, _from: &str, _to: &str) -> Result<()> { Err(FsError::Unsupported("rename")) }
}

/// Validates a single path component for FAT-family filesystems.
pub fn validate_fat_name(name: &str) -> Result<()> {
    let bad = |m: &str| Err(FsError::InvalidName(format!("{name:?}: {m}")));
    if name.is_empty() || name == "." || name == ".." { return bad("reserved"); }
    if name.encode_utf16().count() > 255 { return bad("longer than 255 characters"); }
    if name.ends_with('.') || name.ends_with(' ') { return bad("trailing dot or space"); }
    if name.chars().any(|c| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c)) { return bad("contains a forbidden character"); }
    Ok(())
}

/// Split a path into components, rejecting `..` escapes. Accepts `/a/b`, `a/b`, `""`.
pub fn normalize_path(path: &str) -> Result<Vec<&str>> {
    let mut out = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => { if out.pop().is_none() { return Err(FsError::NotFound(path.into())); } }
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes() {
        assert_eq!(normalize_path("/a//b/./c").unwrap(), ["a", "b", "c"]);
        assert_eq!(normalize_path("").unwrap(), Vec::<&str>::new());
        assert_eq!(normalize_path("a/b/..").unwrap(), ["a"]);
    }
    #[test]
    fn name_validation() {
        assert!(validate_fat_name("ok name.txt").is_ok());
        assert!(validate_fat_name("héllo-日本語.txt").is_ok());
        for bad in ["", ".", "..", "a/b", "a:b", "x?", "trail.", "trail ", "a\u{1}b", "q\"q"] {
            assert!(validate_fat_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_fat_name(&"a".repeat(256)).is_err());
    }
    #[test]
    fn rejects_escape() { assert!(normalize_path("/../etc").is_err()); }
}
