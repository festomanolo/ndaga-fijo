//! NTFS mutation: create / overwrite / delete / rename / mkdir.
//!
//! Safety model (same spirit as the FAT/exFAT drivers):
//! * only on a read-write device, and only if the volume is clean: not dirty, `$LogFile` clean,
//!   not hibernated (Fast Startup) — otherwise `FsError::Refused`;
//! * the volume's dirty bit is raised for the duration of each operation;
//! * file data is written first, then bitmaps, then the MFT record, and the directory index entry
//!   last; any failure before that point releases everything that was allocated;
//! * directory-index changes are planned fully in memory, then committed.
//!
//! Limitations: the MFT cannot grow (creation fails with `NoSpace` when no MFT record is free);
//! files with attribute lists, hard links, compressed/encrypted/sparse data are refused; no
//! `$LogFile`/`$UsnJrnl` entries are written (as ntfs-3g); new names use the Win32 namespace only;
//! deleting can leave empty index blocks (valid, not compacted).

use crate::*;
use fs_core::{validate_fat_name, DirEntry, EntryKind, FsError, Result};
use std::cmp::Ordering;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

const ENTRIES_AT: usize = 0x40; // entries start here inside an INDX block
const CHUNK: usize = 1 << 20;

fn align8(n: usize) -> usize { (n + 7) & !7 }
fn put16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }
fn put32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
fn put64(b: &mut [u8], o: usize, v: u64) { b[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn bit(b: &[u8], i: u64) -> bool { b[(i / 8) as usize] >> (i % 8) & 1 == 1 }
fn set_bit(b: &mut [u8], i: u64, v: bool) { if v { b[(i / 8) as usize] |= 1 << (i % 8) } else { b[(i / 8) as usize] &= !(1 << (i % 8)) } }
fn now_ft() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() + FILETIME_UNIX as u64) * 10_000_000 + d.subsec_nanos() as u64 / 100
}
const FILETIME_UNIX: i64 = 11_644_473_600;

// ---------------------------------------------------------------- records & attributes

pub(crate) struct Rec { pub num: u64, pub buf: Vec<u8> }

#[derive(Clone)]
pub(crate) struct RawAttr { pub ty: u32, pub id: u16, pub name: Vec<u16>, pub b: Vec<u8> }

fn fixup(buf: &mut [u8]) {
    let (uo, uc) = (le16(buf, 4) as usize, le16(buf, 6) as usize);
    let mut usn = le16(buf, uo).wrapping_add(1);
    if usn == 0 || usn == 0xFFFF { usn = 1; }
    put16(buf, uo, usn);
    for i in 1..uc {
        let p = i * 512 - 2;
        let (a, b) = (buf[p], buf[p + 1]);
        buf[uo + 2 * i] = a; buf[uo + 2 * i + 1] = b;
        put16(buf, p, usn);
    }
}

fn split(buf: &[u8]) -> Result<Vec<RawAttr>> {
    let used = (le32(buf, 24) as usize).min(buf.len());
    let mut o = le16(buf, 20) as usize;
    let mut v = Vec::new();
    loop {
        if o + 8 > used { return Err(FsError::Corrupt("attribute list runs past record")); }
        let ty = le32(buf, o);
        if ty == T_END { break; }
        let len = le32(buf, o + 4) as usize;
        if len < 24 || len % 8 != 0 || o + len > used { return Err(FsError::Corrupt("bad attribute length")); }
        let a = &buf[o..o + len];
        let (nl, no) = (a[9] as usize, le16(a, 10) as usize);
        if nl > 0 && no + nl * 2 > len { return Err(FsError::Corrupt("attribute name out of bounds")); }
        v.push(RawAttr { ty, id: le16(a, 14), name: (0..nl).map(|i| le16(a, no + i * 2)).collect(), b: a.to_vec() });
        o += len;
    }
    Ok(v)
}

fn join(buf: &mut [u8], attrs: &[RawAttr]) -> Result<()> {
    let (ao, alloc) = (le16(buf, 20) as usize, (le32(buf, 28) as usize).min(buf.len()));
    let total: usize = attrs.iter().map(|a| a.b.len()).sum::<usize>() + 8;
    if ao + total > alloc { return Err(FsError::NoSpace); }
    buf[ao..alloc].fill(0);
    let mut o = ao;
    for a in attrs { buf[o..o + a.b.len()].copy_from_slice(&a.b); o += a.b.len(); }
    put32(buf, o, T_END);
    put32(buf, 24, (o + 8) as u32);
    Ok(())
}

fn utf16(s: &str) -> Vec<u16> { s.encode_utf16().collect() }

fn res_attr(ty: u32, name: &str, id: u16, flags: u16, val: &[u8], indexed: u8) -> RawAttr {
    let nm = utf16(name);
    let voff = align8(0x18 + nm.len() * 2);
    let len = align8(voff + val.len());
    let mut b = vec![0u8; len];
    put32(&mut b, 0, ty); put32(&mut b, 4, len as u32);
    b[9] = nm.len() as u8;
    if !nm.is_empty() { put16(&mut b, 10, 0x18); }
    put16(&mut b, 12, flags); put16(&mut b, 14, id);
    put32(&mut b, 16, val.len() as u32); put16(&mut b, 20, voff as u16); b[22] = indexed;
    for (i, u) in nm.iter().enumerate() { put16(&mut b, 0x18 + i * 2, *u); }
    b[voff..voff + val.len()].copy_from_slice(val);
    RawAttr { ty, id, name: nm, b }
}

fn encode_runs(runs: &[Run]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for r in runs {
        // NTFS reads the length as a SIGNED little-endian number: a top byte with its high bit set (e.g. 171 = 0xAB)
        // needs an extra 0x00 byte, or ntfs-3g/Windows see a negative length and call the runlist corrupt.
        let lb = { let mut n = 1; while n < 8 && r.len >= (1u64 << (8 * n - 1)) { n += 1; } n };
        match r.lcn {
            None => { out.push(lb as u8); out.extend_from_slice(&r.len.to_le_bytes()[..lb]); }
            Some(l) => {
                let d = l as i64 - prev; prev = l as i64;
                let ob = { let mut n = 1; while n < 8 && !(d >= -(1i64 << (8 * n - 1)) && d < (1i64 << (8 * n - 1))) { n += 1; } n };
                out.push(((ob << 4) | lb) as u8);
                out.extend_from_slice(&r.len.to_le_bytes()[..lb]);
                out.extend_from_slice(&d.to_le_bytes()[..ob]);
            }
        }
    }
    out.push(0);
    out
}

#[allow(clippy::too_many_arguments)]
fn nonres_attr(ty: u32, name: &str, id: u16, flags: u16, runs: &[Run], alloc: u64, size: u64, init: u64, cs: u64) -> RawAttr {
    let nm = utf16(name);
    let mp = encode_runs(runs);
    let mpo = align8(0x40 + nm.len() * 2);
    let len = align8(mpo + mp.len());
    let mut b = vec![0u8; len];
    put32(&mut b, 0, ty); put32(&mut b, 4, len as u32); b[8] = 1; b[9] = nm.len() as u8;
    if !nm.is_empty() { put16(&mut b, 10, 0x40); }
    put16(&mut b, 12, flags); put16(&mut b, 14, id);
    put64(&mut b, 24, (alloc / cs).saturating_sub(1)); put16(&mut b, 32, mpo as u16);
    put64(&mut b, 40, alloc); put64(&mut b, 48, size); put64(&mut b, 56, init);
    for (i, u) in nm.iter().enumerate() { put16(&mut b, 0x40 + i * 2, *u); }
    b[mpo..mpo + mp.len()].copy_from_slice(&mp);
    RawAttr { ty, id, name: nm, b }
}

fn attr_value(a: &RawAttr) -> Option<&[u8]> {
    if a.b[8] != 0 { return None; }
    let (vl, vo) = (le32(&a.b, 16) as usize, le16(&a.b, 20) as usize);
    a.b.get(vo..vo + vl)
}

fn next_attr_id(buf: &mut [u8]) -> u16 { let id = le16(buf, 40); put16(buf, 40, id.wrapping_add(1)); id }

fn filename_value(parent_ref: u64, t: u64, alloc: u64, real: u64, fflags: u32, units: &[u16], ns: u8) -> Vec<u8> {
    let mut v = vec![0u8; 66 + units.len() * 2];
    put64(&mut v, 0, parent_ref);
    for i in 0..4 { put64(&mut v, 8 + i * 8, t); }
    put64(&mut v, 40, alloc); put64(&mut v, 48, real); put32(&mut v, 56, fflags);
    v[64] = units.len() as u8; v[65] = ns;
    for (i, u) in units.iter().enumerate() { put16(&mut v, 66 + i * 2, *u); }
    v
}

fn key_units(k: &[u8]) -> Vec<u16> {
    let n = *k.get(64).unwrap_or(&0) as usize;
    (0..n).filter(|i| 66 + i * 2 + 2 <= k.len()).map(|i| le16(k, 66 + i * 2)).collect()
}

// ---------------------------------------------------------------- index entries

#[derive(Clone, Debug)]
pub(crate) struct Ent { pub mref: u64, pub key: Vec<u8>, pub child: Option<u64>, pub last: bool }
impl Ent {
    fn end(child: Option<u64>) -> Self { Ent { mref: 0, key: vec![], child, last: true } }
    fn len(&self) -> usize { align8(16 + self.key.len()) + if self.child.is_some() { 8 } else { 0 } }
}
fn ents_len(e: &[Ent]) -> usize { e.iter().map(|x| x.len()).sum() }
fn ser_ents(ents: &[Ent]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ents_len(ents));
    for e in ents {
        let l = e.len();
        let mut b = vec![0u8; l];
        put64(&mut b, 0, e.mref); put16(&mut b, 8, l as u16); put16(&mut b, 10, e.key.len() as u16);
        put16(&mut b, 12, (e.child.is_some() as u16) | ((e.last as u16) << 1));
        b[16..16 + e.key.len()].copy_from_slice(&e.key);
        if let Some(c) = e.child { put64(&mut b, l - 8, c); }
        out.extend_from_slice(&b);
    }
    out
}
fn to_ents(v: Vec<IdxEntry>) -> Vec<Ent> {
    v.into_iter().map(|e| Ent { mref: e.mref, key: e.key, child: e.child, last: e.flags & 2 != 0 }).collect()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Loc { Root, Block(u64) }
struct Pn { loc: Loc, ents: Vec<Ent>, pos: usize }

struct DirCtx {
    num: u64,
    rec: Rec,
    attrs: Vec<RawAttr>,
    file: File,
    root: Vec<Ent>,
    root_hdr: [u8; 16],
    free_blocks: Vec<u64>, // VCNs of free index blocks
    nblocks: u64,
}

struct Plan { writes: Vec<(Loc, Vec<Ent>)>, used_blocks: Vec<u64> }

enum Need { Blocks, Err(FsError) }
impl From<FsError> for Need { fn from(e: FsError) -> Self { Need::Err(e) } }

// ---------------------------------------------------------------- the driver

impl NtfsFs {
    fn cmp_units(&self, a: &[u16], b: &[u16]) -> Ordering {
        for (x, y) in a.iter().zip(b) {
            let (ux, uy) = (self.upcase[*x as usize], self.upcase[*y as usize]);
            if ux != uy { return ux.cmp(&uy); }
        }
        a.len().cmp(&b.len())
    }

    pub(crate) fn check_writable(&self) -> Option<&'static str> {
        if self.version.0 != 3 { return Some("only NTFS 3.x volumes can be written"); }
        if self.vol_flags & 0x0001 != 0 { return Some("volume is marked dirty (unclean shutdown); run chkdsk on Windows first"); }
        let Ok(log) = self.load_file(2) else { return Some("cannot read $LogFile") };
        let Some(a) = log.attr(T_DATA, "") else { return Some("$LogFile has no data") };
        let mut h = [0u8; 0x40];
        if a.data_size() < 0x40 || self.read_attr(a, 0, &mut h).is_err() { return Some("cannot read $LogFile"); }
        if h[0..4] == *b"RSTR" {
            let ra = le16(&h, 0x18) as usize;
            if ra + 0x10 > h.len() || le16(&h, ra + 0x0E) & 0x0002 == 0 { return Some("$LogFile is not clean (unclean shutdown); run chkdsk on Windows first"); }
        } else if le32(&h, 0) != 0xFFFF_FFFF {
            return Some("$LogFile is in an unknown state; run chkdsk on Windows first");
        }
        if let Ok(root) = self.load_file(ROOT_REC_W) {
            if let Ok(Some((_, n))) = self.find_in(&root, "hiberfil.sys") {
                if let Ok(f) = self.load_file(n) {
                    let mut m = [0u8; 4];
                    if let Some(a) = f.attr(T_DATA, "") {
                        if a.data_size() >= 4 && self.read_attr(a, 0, &mut m).is_ok() && matches!(&m, b"hibr" | b"HIBR" | b"wake" | b"WAKE") {
                            return Some("Windows is hibernated (Fast Startup); shut it down fully first");
                        }
                    }
                }
            }
        }
        None
    }

    fn require_writable(&self) -> Result<()> {
        if self.dev.is_read_only() { return Err(FsError::Refused("device is read-only")); }
        if let Some(r) = self.write_block { return Err(FsError::Refused(r)); }
        Ok(())
    }

    // ---- record I/O
    fn rec_load(&self, n: u64) -> Result<Rec> {
        let mut b = self.read_record_raw(n)?;
        if &b[0..4] != b"FILE" { return Err(FsError::Corrupt("MFT record has no FILE signature")); }
        let (uo, uc) = (le16(&b, 4) as usize, le16(&b, 6) as usize);
        unfixup(&mut b, uo, uc)?;
        Ok(Rec { num: n, buf: b })
    }

    fn rec_store(&self, r: &Rec) -> Result<()> {
        let mut b = r.buf.clone();
        fixup(&mut b);
        self.dev.write_at(self.mft_offset(r.num)?, &b)?;
        if r.num < 4 && self.mftmirr_lcn != 0 {
            self.dev.write_at(self.mftmirr_lcn * self.cs + r.num * self.rec_size, &b)?;
        }
        Ok(())
    }

    fn fresh_record(&self, num: u64, seq: u16, dir: bool) -> Rec {
        let mut b = vec![0u8; self.rec_size as usize];
        b[0..4].copy_from_slice(b"FILE");
        let uc = self.rec_size as usize / 512 + 1;
        put16(&mut b, 4, 0x30); put16(&mut b, 6, uc as u16);
        put16(&mut b, 0x30, 1);
        put16(&mut b, 16, seq); put16(&mut b, 18, 1);
        put16(&mut b, 20, align8(0x30 + 2 * uc) as u16);
        put16(&mut b, 22, 1 | if dir { 2 } else { 0 });
        put32(&mut b, 28, self.rec_size as u32);
        put32(&mut b, 44, num as u32);
        let ao = align8(0x30 + 2 * uc);
        put32(&mut b, ao, T_END); put32(&mut b, 24, (ao + 8) as u32);
        Rec { num, buf: b }
    }

    // ---- attribute data writes (non-resident)
    fn write_attr_range(&self, a: &Attr, off: u64, data: &[u8]) -> Result<()> {
        let nr = a.nr.as_ref().ok_or(FsError::Unsupported("write into resident attribute"))?;
        let mut done = 0usize;
        while done < data.len() {
            let pos = off + done as u64;
            let vcn = pos / self.cs;
            let idx = nr.runs.partition_point(|r| r.vcn + r.len <= vcn);
            let r = nr.runs.get(idx).filter(|r| r.vcn <= vcn).ok_or(FsError::Corrupt("write beyond data runs"))?;
            let l = r.lcn.ok_or(FsError::Unsupported("write into sparse run"))?;
            let n = (((r.vcn + r.len) * self.cs - pos) as usize).min(data.len() - done);
            self.dev.write_at((l + (vcn - r.vcn)) * self.cs + pos % self.cs, &data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    // ---- cluster bitmap
    fn with_cbm<T>(&self, f: impl FnOnce(&mut Vec<u8>) -> Result<(T, Option<(usize, usize)>)>) -> Result<T> {
        let file = self.load_file(6)?;
        let a = file.attr(T_DATA, "").ok_or(FsError::Corrupt("$Bitmap has no data"))?;
        let mut g = self.cbm.lock().unwrap();
        if g.is_none() { *g = Some(self.read_all(a, 1 << 31)?); }
        let bm = g.as_mut().unwrap();
        let (v, dirty) = f(bm)?;
        if let Some((lo, hi)) = dirty { self.write_attr_range(a, lo as u64, &bm[lo..hi])?; }
        Ok(v)
    }

    fn find_extent(&self, bm: &[u8], from: u64, to: u64) -> Option<(u64, u64)> {
        let mut i = from;
        while i < to {
            if i % 8 == 0 && i + 8 <= to && bm[(i / 8) as usize] == 0xFF { i += 8; continue; }
            if !bit(bm, i) {
                let s = i;
                while i < to && !bit(bm, i) { i += 1; }
                return Some((s, i - s));
            }
            i += 1;
        }
        None
    }

    /// Allocates `want` clusters (preferring one contiguous extent, optionally right after `near`).
    fn alloc_clusters(&self, want: u64, near: Option<u64>) -> Result<Vec<Run>> {
        let total = self.total_clusters;
        let hint = *self.hint.lock().unwrap();
        let runs = self.with_cbm(|bm| {
            let mut picked: Vec<(u64, u64)> = Vec::new();
            if let Some(n) = near {
                if n + want <= total && (n..n + want).all(|c| !bit(bm, c)) { picked.push((n, want)); }
            }
            if picked.is_empty() {
                // first fit: contiguous extent, scanning from the hint then wrapping
                'outer: for (a, b) in [(hint.min(total), total), (0, hint.min(total))] {
                    let mut p = a;
                    while let Some((s, l)) = self.find_extent(bm, p, b) {
                        if l >= want { picked.push((s, want)); break 'outer; }
                        p = s + l;
                    }
                }
            }
            if picked.is_empty() {
                // fragmented: take extents until satisfied
                let mut need = want;
                let mut p = 0u64;
                while need > 0 {
                    let Some((s, l)) = self.find_extent(bm, p, total) else { return Err(FsError::NoSpace) };
                    let t = l.min(need);
                    picked.push((s, t)); need -= t; p = s + l;
                }
            }
            let (mut lo, mut hi) = (usize::MAX, 0usize);
            for &(s, l) in &picked {
                for c in s..s + l { set_bit(bm, c, true); }
                lo = lo.min((s / 8) as usize); hi = hi.max(((s + l - 1) / 8) as usize + 1);
            }
            Ok((picked, Some((lo, hi))))
        })?;
        if let Some(&(s, l)) = runs.last() { *self.hint.lock().unwrap() = s + l; }
        let mut vcn = 0;
        Ok(runs.into_iter().map(|(s, l)| { let r = Run { vcn, len: l, lcn: Some(s) }; vcn += l; r }).collect())
    }

    fn free_clusters(&self, runs: &[Run]) -> Result<()> {
        let rs: Vec<(u64, u64)> = runs.iter().filter_map(|r| r.lcn.map(|l| (l, r.len))).collect();
        if rs.is_empty() { return Ok(()); }
        self.with_cbm(|bm| {
            let (mut lo, mut hi) = (usize::MAX, 0usize);
            for &(s, l) in &rs {
                if s + l > self.total_clusters { return Err(FsError::Corrupt("run outside volume")); }
                for c in s..s + l { set_bit(bm, c, false); }
                lo = lo.min((s / 8) as usize); hi = hi.max(((s + l - 1) / 8) as usize + 1);
            }
            Ok(((), Some((lo, hi))))
        })
    }

    fn write_runs(&self, runs: &[Run], data: &[u8]) -> Result<()> {
        let mut done = 0usize;
        for r in runs {
            let l = r.lcn.ok_or(FsError::Corrupt("sparse run in new allocation"))?;
            let cap = (r.len * self.cs) as usize;
            let n = cap.min(data.len() - done);
            if n > 0 { self.dev.write_at(l * self.cs, &data[done..done + n])?; }
            if n < cap {
                // zero the slack in the last cluster so no stale bytes remain
                let slack = vec![0u8; cap - n];
                self.dev.write_at(l * self.cs + n as u64, &slack)?;
            }
            done += n;
            if done >= data.len() { break; }
        }
        Ok(())
    }

    // ---- MFT record allocation
    fn alloc_record(&self) -> Result<(u64, u16)> {
        match self.alloc_record_in_place() {
            Err(FsError::NoSpace) => { self.extend_mft()?; self.alloc_record_in_place() }
            r => r,
        }
    }

    fn alloc_record_in_place(&self) -> Result<(u64, u16)> {
        let f0 = self.load_file(0)?;
        let bm_attr = f0.attr(T_BITMAP, "").ok_or(FsError::Corrupt("$MFT has no bitmap"))?;
        if bm_attr.nr.is_none() { return Err(FsError::Unsupported("resident $MFT bitmap")); }
        let mut bm = self.read_all(bm_attr, 1 << 28)?;
        let nrec = f0.attr(T_DATA, "").map(|a| a.data_size()).unwrap_or(0) / self.rec_size;
        for n in 24..nrec {
            if (n / 8) as usize >= bm.len() { break; }
            if bit(&bm, n) { continue; }
            // Never reuse a record that still claims to be in use.
            let mut seq = 0u16;
            if let Ok(r) = self.rec_load(n) {
                if le16(&r.buf, 22) & 1 != 0 { continue; }
                seq = le16(&r.buf, 16);
            }
            set_bit(&mut bm, n, true);
            let o = (n / 8) as usize;
            self.write_attr_range(bm_attr, o as u64, &bm[o..o + 1])?;
            let seq = if seq == 0xFFFF || seq == 0 { 1 } else { seq + 1 };
            return Ok((n, seq));
        }
        Err(FsError::NoSpace)
    }

    /// Grows `$MFT` (and its bitmap) by up to 64 records: first into slack already allocated to the
    /// data attribute, otherwise by allocating clusters, preferably contiguous with the last run.
    fn extend_mft(&self) -> Result<()> {
        let f0 = self.load_file(0)?;
        let mut r0 = self.rec_load(0)?;
