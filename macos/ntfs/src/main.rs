//! ndaga-ntfs: the NTFS half of Ndaga Fijo's ISO mode, built on VOLUMA's NTFS driver.
//!
//!   ndaga-ntfs copy   --target PATH [--offset N --length N] --src DIR
//!       Copies a directory tree into a freshly made (mkntfs) NTFS volume at PATH, or at a byte window of it.
//!   ndaga-ntfs stream --image IMG --target DEV --offset N
//!       Writes only the clusters in use (per $Bitmap), plus the backup boot sector, of a staged NTFS image to DEV.
//!
//! Progress goes to stdout as `P <done> <total> <name>` lines, logs as `L <text>`, errors to stderr.
//! Licensed under the GNU GPL v3 or later; the vendored driver crates are MIT (see vendor/).

use block_device::{BlockDevice, ImageDevice, PartitionWindow};
use fs_core::Filesystem;
use fs_ntfs::NtfsFs;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Res<T> = Result<T, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else { usage() };
    let mut o: HashMap<String, String> = HashMap::new();
    let mut i = 1;
    while i + 1 < args.len() {
        if let Some(k) = args[i].strip_prefix("--") { o.insert(k.to_string(), args[i + 1].clone()); i += 2 } else { usage() }
    }
    let r = match cmd.as_str() {
        "copy" => copy(&o),
        "stream" => stream(&o),
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!("usage: ndaga-ntfs copy --target PATH [--offset N --length N] --src DIR\n       ndaga-ntfs stream --image IMG --target DEV --offset N");
    std::process::exit(2)
}

fn req<'a>(o: &'a HashMap<String, String>, k: &str) -> Res<&'a str> {
    o.get(k).map(|s| s.as_str()).ok_or_else(|| format!("missing --{k}"))
}
fn num(o: &HashMap<String, String>, k: &str) -> Res<Option<u64>> {
    o.get(k).map(|s| s.parse::<u64>().map_err(|_| format!("--{k} is not a number"))).transpose()
}

struct Progress { total: u64, done: u64, last: Instant }
impl Progress {
    fn new(total: u64) -> Self { Self { total, done: 0, last: Instant::now() - Duration::from_secs(1) } }
    fn add(&mut self, n: u64, name: &str) {
        self.done += n;
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            println!("P {} {} {}", self.done, self.total, name);
            let _ = std::io::stdout().flush();
        }
    }
}

/// fsync for files; device nodes don't take fsync (ENOTTY), so ask the disk to flush its cache instead.
fn flush(f: &std::fs::File) -> Res<()> {
    match f.sync_all() {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENOTTY) || e.raw_os_error() == Some(libc::ENODEV) => {
            const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;
            unsafe { libc::ioctl(f.as_raw_fd(), DKIOCSYNCHRONIZECACHE) };
            Ok(())
        }
        Err(e) => Err(format!("flush failed: {e}")),
    }
// ---------------------------------------------------------------- copy

struct Tree { dirs: Vec<String>, files: Vec<(PathBuf, String, u64)>, total: u64 }

fn walk(root: &Path) -> Res<Tree> {
    let mut t = Tree { dirs: vec![], files: vec![], total: 0 };
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut ents: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .filter_map(|e| e.ok()).collect();
        ents.sort_by_key(|e| e.file_name());
        let mut subdirs = vec![];
        for e in ents {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let md = std::fs::metadata(e.path()).map_err(|er| format!("cannot stat {r}: {er}"))?;
            if md.is_dir() { t.dirs.push(r.clone()); subdirs.push((e.path(), r)); }
            else if md.is_file() { t.total += md.len(); t.files.push((e.path(), r, md.len())); }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    t.dirs.sort_by_key(|d| d.matches('/').count());               // parents before children
    Ok(t)
}

struct Counting<'a, R: Read> { inner: R, p: &'a mut Progress, name: &'a str }
impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.p.add(n as u64, self.name);
        Ok(n)
    }
}

fn copy(o: &HashMap<String, String>) -> Res<()> {
    let target = req(o, "target")?;
    let src = PathBuf::from(req(o, "src")?);
    let dev: Arc<dyn BlockDevice> = Arc::new(ImageDevice::open_read_write(target).map_err(|e| format!("cannot open {target}: {e}"))?);
    let dev: Arc<dyn BlockDevice> = match num(o, "offset")? {
        Some(off) => {
            let len = num(o, "length")?.ok_or("--offset needs --length")?;
            Arc::new(PartitionWindow::new(dev, off, len).map_err(|e| e.to_string())?)
        }
        None => dev,
    };
    let fs = NtfsFs::open(dev).map_err(|e| format!("not a usable NTFS volume: {e}"))?;
    if let Some(why) = fs.write_blocked() { return Err(format!("the NTFS volume cannot be written: {why}")); }

    let tree = walk(&src)?;
    println!("L Copying {} files in {} folders ({} bytes) with the VOLUMA NTFS driver", tree.files.len(), tree.dirs.len(), tree.total);
    let mut p = Progress::new(tree.total);
    for d in &tree.dirs {
        fs.create_dir(d).map_err(|e| format!("cannot create folder {d}: {e}"))?;
    }
    for (path, rel, _) in &tree.files {
        let f = std::fs::File::open(path).map_err(|e| format!("cannot read {rel}: {e}"))?;
        let mut r = Counting { inner: std::io::BufReader::with_capacity(1 << 20, f), p: &mut p, name: rel };
        fs.write_file(rel, &mut r, false).map_err(|e| format!("cannot write {rel}: {e}"))?;
    }
    drop(fs);
    // Flush whatever the OS still buffers for the target (block device or image file).
    std::fs::OpenOptions::new().write(true).open(target).and_then(|f| f.sync_all()).map_err(|e| format!("flush failed: {e}"))?;
    println!("P {} {} done", p.total, p.total);
    println!("L Copied {} files", tree.files.len());
    Ok(())
}

// ---------------------------------------------------------------- stream

/// Byte ranges of the image that must reach the device: in-use clusters (gaps under 1 MiB merged, since
/// one larger write beats many small ones) and everything past the last cluster (the backup boot sector).
fn ranges(bitmap: &[u8], cs: u64, clusters: u64, image_len: u64) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = Vec::new();
    let gap = ((1u64 << 20) / cs).max(1);
    let mut c = 0u64;
    while c < clusters {
        if bitmap.get((c / 8) as usize).map_or(false, |b| b >> (c % 8) & 1 == 1) {
            let s = c;
            while c < clusters && bitmap.get((c / 8) as usize).map_or(false, |b| b >> (c % 8) & 1 == 1) { c += 1; }
            match v.last_mut() { Some(l) if s - l.1 / cs <= gap => l.1 = c * cs, _ => v.push((s * cs, c * cs)) }
        } else { c += 1; }
    }
    let tail = clusters * cs;
    if tail < image_len { v.push((tail, image_len)); }
    v
}

fn stream(o: &HashMap<String, String>) -> Res<()> {
    let image = req(o, "image")?;
    let target = req(o, "target")?;
    let offset = num(o, "offset")?.ok_or("missing --offset")?;
    let img_dev = Arc::new(ImageDevice::open_read_only(image).map_err(|e| format!("cannot open {image}: {e}"))?);
    let image_len = img_dev.size();
    let fs = NtfsFs::open(img_dev).map_err(|e| format!("staged image is not NTFS: {e}"))?;
    let (cs, clusters) = fs.geometry();
    let bm = fs.cluster_bitmap().map_err(|e| format!("cannot read $Bitmap: {e}"))?;
    drop(fs);
    let rs = ranges(&bm, cs, clusters, image_len);
    let total: u64 = rs.iter().map(|r| r.1 - r.0).sum();
    println!("L Streaming {total} bytes in {} extents to {target}", rs.len());

    let src = std::fs::File::open(image).map_err(|e| e.to_string())?;
    let dst = std::fs::OpenOptions::new().write(true).custom_flags(0).open(target).map_err(|e| format!("cannot open {target}: {e}"))?;
    unsafe { libc::fcntl(dst.as_raw_fd(), libc::F_NOCACHE, 1); }
    use std::os::unix::fs::FileExt;
    const BUF: usize = 8 << 20;
    // Raw devices want sector-aligned buffers.
    let mut store = vec![0u8; BUF + 4096];
    let pad = store.as_ptr().align_offset(4096);
    let buf = &mut store[pad..pad + BUF];
    let mut p = Progress::new(total);
    for (s, e) in rs {
        let mut at = s;
        while at < e {
            let n = ((e - at) as usize).min(BUF);
            src.read_exact_at(&mut buf[..n], at).map_err(|er| format!("image read error at {at}: {er}"))?;
            dst.write_all_at(&buf[..n], offset + at).map_err(|er| format!("device write error at {}: {er}", offset + at))?;
            at += n as u64;
            p.add(n as u64, "");
        }
    }
    dst.sync_all().map_err(|e| format!("flush failed: {e}"))?;
    println!("P {total} {total} done");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ranges;
    #[test]
    fn merges_and_keeps_tail() {
        // clusters 0-1 and 3 in use (gap of 1 cluster merges), 4 KiB clusters, image has a 512-byte tail.
        let r = ranges(&[0b0000_1011], 4096, 8, 8 * 4096 + 512);
        assert_eq!(r, vec![(0, 4 * 4096), (8 * 4096, 8 * 4096 + 512)]);
        let r = ranges(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80], 4096, 264, 264 * 4096);
        assert_eq!(r, vec![(263 * 4096, 264 * 4096)]);
    }
}
