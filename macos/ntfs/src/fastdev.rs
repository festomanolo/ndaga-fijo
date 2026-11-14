//! FastDevice: the block layer `ndaga-ntfs` puts under the NTFS driver (and the image streamer).
//!
//! The driver issues a mix of large sequential data writes (file contents, in 1 MiB chunks) and small
//! scattered metadata writes (MFT records and their mirror, $Bitmap bytes, index blocks, the volume dirty
//! flag, set and cleared around every file). Sent to a USB stick as they come, every small write lands
//! in a different flash erase block between two data chunks, and the stick slows to a few MB/s.
//! FastDevice splits the two:
//!   * small or unaligned writes go to an in-memory write-back page cache, flushed once at the end,
//!     in ascending order with neighbouring pages merged;
//!   * large page-aligned writes are coalesced into contiguous batches (8 MiB) and handed to a pool of
//!     writer threads, so several writes are in flight while the caller reads the next source data.
//! Reads see the newest data: cached pages are overlaid, and a read waits for in-flight writes it
//! overlaps. All device I/O is 4 KiB-aligned with aligned buffers, so a raw node (/dev/rdiskN) works.
//! Licensed under the GNU GPL v3 or later.

use block_device::{BlockDevice, BlockError, Result};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

pub const PAGE: u64 = 4096;
/// Aligned writes at least this large bypass the page cache.
const BIG: u64 = 64 << 10;
/// Contiguous big writes are merged up to this size before being queued.
pub const BATCH: usize = 8 << 20;
/// Cache size at which dirty pages are written out early (only reached on huge metadata loads).
const CACHE_LIMIT: usize = 64 << 20;

/// A heap buffer aligned to PAGE, as raw devices want.
pub struct AlignedBuf { ptr: *mut u8, len: usize, cap: usize }
unsafe impl Send for AlignedBuf {}
impl AlignedBuf {
    pub fn new(cap: usize) -> Self {
        let cap = (cap.max(1) as u64).div_ceil(PAGE) as usize * PAGE as usize;
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(cap, PAGE as usize).unwrap()) };
        assert!(!ptr.is_null(), "out of memory");
        Self { ptr, len: 0, cap }
    }
    pub fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
    pub fn as_mut_slice(&mut self) -> &mut [u8] { unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) } }
    pub fn set_len(&mut self, n: usize) { assert!(n <= self.cap); self.len = n; }
    fn push(&mut self, d: &[u8]) {
        assert!(self.len + d.len() <= self.cap);
        unsafe { std::ptr::copy_nonoverlapping(d.as_ptr(), self.ptr.add(self.len), d.len()) };
        self.len += d.len();
    }
}
impl Drop for AlignedBuf {
    fn drop(&mut self) { unsafe { dealloc(self.ptr, Layout::from_size_align(self.cap, PAGE as usize).unwrap()) } }
}

struct Job { off: u64, buf: AlignedBuf }

struct Shared {
    file: File,
    inflight: Mutex<Vec<(u64, u64)>>,
    cv: Condvar,
    err: Mutex<Option<String>>,
    written: AtomicU64,
    jobs: AtomicU64,
    pool: Mutex<Vec<AlignedBuf>>,      // spent BATCH buffers, reused instead of reallocated
}
impl Shared {
    fn take_buf(&self) -> AlignedBuf {
        self.pool.lock().unwrap().pop().unwrap_or_else(|| AlignedBuf::new(BATCH))
    }
    fn give_buf(&self, mut b: AlignedBuf) {
        let mut p = self.pool.lock().unwrap();
        if b.cap == BATCH && p.len() < 32 { b.len = 0; p.push(b); }
    }
    fn overlaps(v: &[(u64, u64)], s: u64, e: u64) -> bool { v.iter().any(|&(a, b)| a < e && s < b) }
    fn wait_clear(&self, s: u64, e: u64) {
        let mut g = self.inflight.lock().unwrap();
        while Self::overlaps(&g, s, e) { g = self.cv.wait(g).unwrap(); }
    }
    fn wait_idle(&self) {
        let mut g = self.inflight.lock().unwrap();
        while !g.is_empty() { g = self.cv.wait(g).unwrap(); }
    }
    fn fail(&self, m: String) { let mut e = self.err.lock().unwrap(); if e.is_none() { *e = Some(m); } }
    fn check(&self) -> Result<()> {
        match self.err.lock().unwrap().clone() { Some(m) => Err(BlockError::Io(io::Error::other(m))), None => Ok(()) }
    }
}

struct State {
    cache: BTreeMap<u64, Box<[u8]>>,   // absolute page offset -> PAGE bytes, newer than the device
    pend: Option<(u64, AlignedBuf)>,   // a contiguous batch being assembled
}

pub struct FastDevice {
    sh: Arc<Shared>,
    base: u64,
    size: u64,
    dev_end: u64,
    is_device: bool,
    st: Mutex<State>,
    tx: Mutex<Option<SyncSender<Job>>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// Size of a regular file, or of a disk node (whose metadata length is 0).
fn device_size(f: &File) -> io::Result<u64> {
    let len = f.metadata()?.len();
    if len > 0 { return Ok(len); }
    #[cfg(target_os = "macos")]
    {
        let (mut bs, mut count) = (0u32, 0u64);
        let fd = f.as_raw_fd();
        // DKIOCGETBLOCKSIZE / DKIOCGETBLOCKCOUNT
        if unsafe { libc::ioctl(fd, 0x4004_6418, &mut bs) == 0 && libc::ioctl(fd, 0x4008_6419, &mut count) == 0 } && bs > 0 {
            return Ok(bs as u64 * count);
        }
    }
    Ok(0)
}

pub fn writer_count() -> usize {
    std::env::var("NDAGA_NTFS_WRITERS").ok().and_then(|s| s.parse().ok()).filter(|&n: &usize| (1..=16).contains(&n)).unwrap_or(4)
}

impl FastDevice {
    /// Opens `path` read-write and exposes the byte window [base, base+len) (len None: to the end).
    pub fn open(path: &str, base: u64, len: Option<u64>, writers: usize) -> std::result::Result<Arc<Self>, String> {
        let file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
        let ft = file.metadata().map_err(|e| e.to_string())?.file_type();
        let is_device = ft.is_block_device() || ft.is_char_device();
        if is_device { unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1); } }
        let dev_end = device_size(&file).map_err(|e| format!("cannot size {path}: {e}"))?;
        let size = len.unwrap_or(dev_end.saturating_sub(base));
        if base % PAGE != 0 || base.checked_add(size).map_or(true, |e| e > dev_end) {
            return Err(format!("bad window {base}+{size} on {path} ({dev_end} bytes)"));
        }
        let sh = Arc::new(Shared { file, inflight: Mutex::new(vec![]), cv: Condvar::new(), err: Mutex::new(None), written: AtomicU64::new(0), jobs: AtomicU64::new(0), pool: Mutex::new(vec![]) });
        let (tx, rx) = sync_channel::<Job>(writers * 2);
        let rx = Arc::new(Mutex::new(rx));
        let mut hs = vec![];
        for _ in 0..writers.max(1) {
            let (sh, rx) = (sh.clone(), rx.clone());
            hs.push(std::thread::spawn(move || loop {
                let job = { let r = rx.lock().unwrap(); r.recv() };
                let Ok(job) = job else { break };
                let n = job.buf.len as u64;
                if sh.err.lock().unwrap().is_none() {
                    if let Err(e) = sh.file.write_all_at(job.buf.as_slice(), job.off) {
                        sh.fail(format!("device write error at {}: {e}", job.off));
                    }
                }
                sh.written.fetch_add(n, Ordering::Relaxed);
                sh.jobs.fetch_add(1, Ordering::Relaxed);
                let off = job.off;
                sh.give_buf(job.buf);
                let mut g = sh.inflight.lock().unwrap();
                if let Some(i) = g.iter().position(|&(a, _)| a == off) { g.swap_remove(i); }
                drop(g);
                sh.cv.notify_all();
            }));
        }
        Ok(Arc::new(Self {
            sh, base, size, dev_end, is_device,
            st: Mutex::new(State { cache: BTreeMap::new(), pend: None }),
            tx: Mutex::new(Some(tx)), workers: Mutex::new(hs),
        }))
    }

    /// Bytes the writer threads have put on the device so far.
    pub fn written(&self) -> u64 { self.sh.written.load(Ordering::Relaxed) }
    /// "N MB in M writes", for the log.
    pub fn summary(&self) -> String {
        format!("{} MB in {} device writes", self.written() >> 20, self.sh.jobs.load(Ordering::Relaxed))
    }

    fn submit(&self, off: u64, buf: AlignedBuf) -> Result<()> {
        if buf.len == 0 { return Ok(()); }
        let end = off + buf.len as u64;
        {
            // Never let two writes to the same bytes race: wait out any older overlapping one.
            let mut g = self.sh.inflight.lock().unwrap();
            while Shared::overlaps(&g, off, end) { g = self.sh.cv.wait(g).unwrap(); }
            g.push((off, end));
        }
        let tx = self.tx.lock().unwrap();
        let tx = tx.as_ref().ok_or_else(|| BlockError::Io(io::Error::other("device already closed")))?;
        tx.send(Job { off, buf }).map_err(|_| BlockError::Io(io::Error::other("writer threads stopped")))
    }

    fn submit_pending(&self, st: &mut State) -> Result<()> {
        match st.pend.take() { Some((o, b)) => self.submit(o, b), None => Ok(()) }
    }

    /// Makes [s, e) safe to read from the device: the batch being assembled is queued if it overlaps,
    /// then any overlapping in-flight write is waited for.
    fn settle(&self, st: &mut State, s: u64, e: u64) -> Result<()> {
        if let Some((o, b)) = &st.pend { if *o < e && s < o + b.len as u64 { self.submit_pending(st)?; } }
        self.sh.wait_clear(s, e);
        self.sh.check()
    }

    fn read_device(&self, st: &mut State, s: u64, e: u64) -> Result<AlignedBuf> {
        self.settle(st, s, e)?;
        let mut b = AlignedBuf::new((e - s) as usize);
        b.set_len((e - s) as usize);
        self.sh.file.read_exact_at(b.as_mut_slice(), s)?;
        Ok(b)
    }

    fn cache_write(&self, st: &mut State, abs: u64, data: &[u8]) -> Result<()> {
        let end = abs + data.len() as u64;
        let mut p = abs / PAGE * PAGE;
        while p < end {
            let pe = (p + PAGE).min(self.dev_end);
            let (s, e) = (abs.max(p), end.min(pe));
            if !st.cache.contains_key(&p) {
                let mut page = vec![0u8; PAGE as usize].into_boxed_slice();
                if !(s == p && e == pe) {
                    let b = self.read_device(st, p, pe)?;
                    page[..(pe - p) as usize].copy_from_slice(b.as_slice());
                }
                st.cache.insert(p, page);
            }
            let page = st.cache.get_mut(&p).unwrap();
            page[(s - p) as usize..(e - p) as usize].copy_from_slice(&data[(s - abs) as usize..(e - abs) as usize]);
            p += PAGE;
        }
        if st.cache.len() * PAGE as usize > CACHE_LIMIT { self.flush_cache(st)?; }
        Ok(())
    }

    fn big_write(&self, st: &mut State, abs: u64, data: &[u8]) -> Result<()> {
        let end = abs + data.len() as u64;
        // These whole pages are now newest on the write path, so stale cached copies go.
        let stale: Vec<u64> = st.cache.range(abs..end).map(|(k, _)| *k).collect();
        for k in stale { st.cache.remove(&k); }
        let mut at = 0usize;
        while at < data.len() {
            let off = abs + at as u64;
            let joinable = matches!(&st.pend, Some((o, b)) if o + b.len as u64 == off && b.len < BATCH);
            if !joinable {
                self.submit_pending(st)?;
                st.pend = Some((off, self.sh.take_buf()));
            }
            let (_, b) = st.pend.as_mut().unwrap();
            let n = (BATCH - b.len).min(data.len() - at);
            b.push(&data[at..at + n]);
            at += n;
            if b.len == BATCH { self.submit_pending(st)?; }
        }
        Ok(())
    }

    /// Writes every cached page out (ascending, adjacent pages merged) and empties the cache.
    fn flush_cache(&self, st: &mut State) -> Result<()> {
        self.submit_pending(st)?;
        let cache = std::mem::take(&mut st.cache);
        let mut run: Option<(u64, AlignedBuf)> = None;
        for (p, page) in cache {
            let n = ((p + PAGE).min(self.dev_end) - p) as usize;
            let joinable = matches!(&run, Some((o, b)) if o + b.len as u64 == p && b.len + n <= BATCH);
            if !joinable {
                if let Some((o, b)) = run.take() { self.submit(o, b)?; }
                run = Some((p, self.sh.take_buf()));
            }
            run.as_mut().unwrap().1.push(&page[..n]);
        }
        if let Some((o, b)) = run { self.submit(o, b)?; }
        Ok(())
    }

    /// Writes everything out, waits for it, and asks the device to flush its own cache.
    pub fn finish(&self) -> std::result::Result<(), String> {
        let mut st = self.st.lock().unwrap();
        // Data first, then the metadata that points at it.
        self.submit_pending(&mut st).map_err(|e| e.to_string())?;
        self.sh.wait_idle();
        self.flush_cache(&mut st).map_err(|e| e.to_string())?;
        self.sh.wait_idle();
        self.sh.check().map_err(|e| e.to_string())?;
        match self.sh.file.sync_all() {
            Ok(()) => Ok(()),
            Err(_) if self.is_device => {
                const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416;
                unsafe { libc::ioctl(self.sh.file.as_raw_fd(), DKIOCSYNCHRONIZECACHE) };
                Ok(())
            }
            Err(e) => Err(format!("flush failed: {e}")),
        }
    }
}

impl Drop for FastDevice {
    fn drop(&mut self) {
        self.tx.lock().unwrap().take();
        for h in self.workers.lock().unwrap().drain(..) { let _ = h.join(); }
    }
}

fn bounds(offset: u64, len: usize, size: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(e) if e <= size => Ok(()),
        _ => Err(BlockError::OutOfBounds { offset, len: len as u64, size }),
    }
}

impl BlockDevice for FastDevice {
    fn size(&self) -> u64 { self.size }
    fn is_read_only(&self) -> bool { false }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        bounds(offset, buf.len(), self.size)?;
        if buf.is_empty() { return Ok(()); }
        let abs = self.base + offset;
        let end = abs + buf.len() as u64;
        let (s, e) = (abs / PAGE * PAGE, (end.div_ceil(PAGE) * PAGE).min(self.dev_end));
        let mut st = self.st.lock().unwrap();
        self.sh.check()?;
        let all_cached = (s..e).step_by(PAGE as usize).all(|p| st.cache.contains_key(&p));
        let dev = if all_cached { None } else { Some(self.read_device(&mut st, s, e)?) };
        let mut p = s;
        while p < e {
            let pe = (p + PAGE).min(e);
            let (cs, ce) = (abs.max(p), end.min(pe));
            if cs < ce {
                let dst = &mut buf[(cs - abs) as usize..(ce - abs) as usize];
                match st.cache.get(&p) {
                    Some(page) => dst.copy_from_slice(&page[(cs - p) as usize..(ce - p) as usize]),
                    None => dst.copy_from_slice(&dev.as_ref().unwrap().as_slice()[(cs - s) as usize..(ce - s) as usize]),
                }
            }
            p = pe;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        bounds(offset, data.len(), self.size)?;
        if data.is_empty() { return Ok(()); }
        let abs = self.base + offset;
        let end = abs + data.len() as u64;
        let mut st = self.st.lock().unwrap();
        self.sh.check()?;
        let (a0, a1) = (abs.div_ceil(PAGE) * PAGE, end / PAGE * PAGE);
        if a1 > a0 && a1 - a0 >= BIG {
            if abs < a0 { self.cache_write(&mut st, abs, &data[..(a0 - abs) as usize])?; }
            self.big_write(&mut st, a0, &data[(a0 - abs) as usize..(a1 - abs) as usize])?;
            if a1 < end { self.cache_write(&mut st, a1, &data[(a1 - abs) as usize..])?; }
            Ok(())
        } else {
            self.cache_write(&mut st, abs, data)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(len: u64) -> (String, File) {
        let p = std::env::temp_dir().join(format!("fastdev-{}-{}", std::process::id(), rand_id()));
        let f = OpenOptions::new().read(true).write(true).create(true).open(&p).unwrap();
        f.set_len(len).unwrap();
        (p.to_string_lossy().into_owned(), f)
    }
    fn rand_id() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 }

    /// Random mixed writes and reads through FastDevice must match a plain in-memory model.
    #[test]
    fn matches_model() {
        let len = 24u64 << 20;
        let (path, f) = temp(len + 8192);
        let dev = FastDevice::open(&path, 4096, Some(len), 3).unwrap();
        let mut model = vec![0u8; len as usize];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = |m: u64| { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed % m };
        for i in 0..2000 {
            let big = rnd(4) == 0;
            let n = if big { (rnd(64) + 1) * 65536 + rnd(5000) } else { rnd(9000) + 1 } as usize;
            let off = if big && rnd(2) == 0 { rnd(len / 4096) * 4096 } else { rnd(len) };
            let n = n.min((len - off) as usize);
            if rnd(3) == 0 {
                let mut b = vec![0u8; n];
                dev.read_at(off, &mut b).unwrap();
                assert!(b == model[off as usize..off as usize + n], "read mismatch at step {i}");
            } else {
                let d: Vec<u8> = (0..n).map(|_| rnd(256) as u8).collect();
                dev.write_at(off, &d).unwrap();
                model[off as usize..off as usize + n].copy_from_slice(&d);
            }
        }
        dev.finish().unwrap();
        drop(dev);
        let mut on_disk = vec![0u8; len as usize];
        f.read_exact_at(&mut on_disk, 4096).unwrap();
        assert!(on_disk == model, "device contents differ from model after finish");
        let mut guard = vec![1u8; 4096];
        f.read_exact_at(&mut guard, 0).unwrap();
        assert!(guard.iter().all(|&b| b == 0), "wrote outside the window");
        std::fs::remove_file(path).ok();
    }
}
