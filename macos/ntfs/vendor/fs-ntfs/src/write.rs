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
        let mut attrs = split(&r0.buf)?;
        if attrs.iter().any(|a| a.ty == T_ATTRLIST) { return Err(FsError::Unsupported("$MFT with an attribute list")); }
        let d = f0.attr(T_DATA, "").and_then(|a| a.nr.clone()).ok_or(FsError::Corrupt("$MFT data is resident"))?;
        let bmap = f0.attr(T_BITMAP, "").cloned().ok_or(FsError::Corrupt("$MFT has no bitmap"))?;
        let bnr = bmap.nr.clone().ok_or(FsError::Unsupported("resident $MFT bitmap"))?;
        let mut runs = d.runs.clone();
        let (mut alloc, mut fresh): (u64, Vec<Run>) = (d.alloc, Vec::new());
        let slack = (alloc.saturating_sub(d.size) / self.rec_size) * self.rec_size;
        let grow = if slack > 0 { slack } else {
            let ext = (64 * self.rec_size).div_ceil(self.cs).max(1);
            let near = runs.last().and_then(|r| r.lcn.map(|l| l + r.len));
            fresh = self.alloc_clusters(ext, near)?;
            let mut vcn = runs.last().map(|r| r.vcn + r.len).unwrap_or(0);
            for g in &fresh {
                match runs.last_mut() { Some(l) if l.lcn.map(|x| x + l.len) == g.lcn => l.len += g.len, _ => runs.push(Run { vcn, len: g.len, lcn: g.lcn }) }
                vcn += g.len;
            }
            alloc += ext * self.cs;
            (ext * self.cs / self.rec_size) * self.rec_size
        };
        let result = (|| -> Result<()> {
            let (old_n, new_size) = (d.size / self.rec_size, d.size + grow);
            let new_n = new_size / self.rec_size;
            // bitmap must cover the new records
            let need = align8(new_n.div_ceil(8) as usize) as u64;
            if need > bnr.alloc { return Err(FsError::Unsupported("$MFT bitmap needs more clusters")); }
            if need > bnr.size {
                let zeros = vec![0u8; (need - bnr.size) as usize];
                self.write_attr_range(&bmap, bnr.size, &zeros)?;
            }
            // $MFT's own data runs must be visible before we format records inside the new space
            let saved = self.mft_runs.lock().unwrap().clone();
            *self.mft_runs.lock().unwrap() = runs.clone();
            let fmt = (|| -> Result<()> {
                for n in old_n..new_n { let rec = self.fresh_record(n, 0, false); let mut b = rec.buf.clone(); put16(&mut b, 22, 0); let rr = Rec { num: n, buf: b }; self.rec_store(&rr)?; }
                // record 0
                let di = attrs.iter().position(|a| a.ty == T_DATA && a.name.is_empty()).ok_or(FsError::Corrupt("$MFT data attr missing"))?;
                let (id, flags) = (attrs[di].id, le16(&attrs[di].b, 12));
                attrs[di] = nonres_attr(T_DATA, "", id, flags, &runs, alloc, new_size, new_size, self.cs);
                if need > bnr.size {
                    let bi = attrs.iter().position(|a| a.ty == T_BITMAP && a.name.is_empty()).ok_or(FsError::Corrupt("$MFT bitmap attr missing"))?;
                    put64(&mut attrs[bi].b, 48, need); put64(&mut attrs[bi].b, 56, need);
                }
                join(&mut r0.buf, &attrs)?;
                self.rec_store(&r0)
            })();
            if fmt.is_err() { *self.mft_runs.lock().unwrap() = saved; }
            fmt
        })();
        if result.is_err() { self.free_clusters(&fresh).ok(); }
        result
    }

    fn free_record(&self, n: u64) -> Result<()> {
        let f0 = self.load_file(0)?;
        let a = f0.attr(T_BITMAP, "").ok_or(FsError::Corrupt("$MFT has no bitmap"))?;
        let mut byte = [0u8; 1];
        let o = n / 8;
        self.read_attr(a, o, &mut byte)?;
        byte[0] &= !(1 << (n % 8));
        self.write_attr_range(a, o, &byte)
    }

    // ---- volume dirty flag
    fn set_dirty(&self, on: bool) -> Result<()> {
        let mut r = self.rec_load(3)?;
        let attrs = split(&r.buf)?;
        let a = attrs.iter().find(|a| a.ty == T_VOLINFO).ok_or(FsError::Corrupt("$Volume has no volume information"))?;
        let vo = le16(&a.b, 20) as usize;
        let ao = le16(&r.buf, 20) as usize + attrs.iter().take_while(|x| !std::ptr::eq(*x, a)).map(|x| x.b.len()).sum::<usize>();
        let p = ao + vo + 10;
        let mut f = le16(&r.buf, p);
        if on { f |= 1 } else { f &= !1 }
        put16(&mut r.buf, p, f);
        self.rec_store(&r)
    }

    fn op<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.require_writable()?;
        let _g = self.wlock.lock().unwrap();
        self.invalidate();
        self.set_dirty(true)?;
        let r = f();
        self.invalidate();
        // An I/O failure may have left metadata half-written: keep the dirty bit so Windows runs chkdsk.
        if !matches!(&r, Err(FsError::Block(_))) { self.set_dirty(false)?; }
        r
    }

    // ---- stdinfo helpers
    fn touch(&self, r: &mut Rec, mtime: bool) -> Result<()> {
        let mut attrs = split(&r.buf)?;
        let now = now_ft();
        if let Some(a) = attrs.iter_mut().find(|a| a.ty == T_STDINFO) {
            let vo = le16(&a.b, 20) as usize;
            if a.b.len() >= vo + 32 {
                if mtime { put64(&mut a.b, vo + 8, now); }
                put64(&mut a.b, vo + 16, now); put64(&mut a.b, vo + 24, now);
            }
        }
        join(&mut r.buf, &attrs)
    }

    fn seq_of(&self, n: u64) -> Result<u16> { Ok(le16(&self.rec_load(n)?.buf, 16)) }

    // ================================================================ directory index
    fn dir_ctx(&self, dnum: u64) -> Result<DirCtx> {
        let rec = self.rec_load(dnum)?;
        if le16(&rec.buf, 22) & 2 == 0 { return Err(FsError::NotADirectory(format!("record {dnum}"))); }
        let attrs = split(&rec.buf)?;
        if attrs.iter().any(|a| a.ty == T_ATTRLIST) { return Err(FsError::Unsupported("directory with an attribute list")); }
        let file = self.load_file(dnum)?;
        let ra = attrs.iter().find(|a| a.ty == T_INDEX_ROOT && a.name == utf16("$I30")).ok_or(FsError::Corrupt("directory has no $I30 root"))?;
        let rv = attr_value(ra).ok_or(FsError::Corrupt("non-resident index root"))?;
        if rv.len() < 32 || le32(rv, 0) != 0x30 { return Err(FsError::Corrupt("bad index root")); }
        let root = to_ents(parse_node(rv, 16)?);
        let mut root_hdr = [0u8; 16];
        root_hdr.copy_from_slice(&rv[..16]);
        let (mut free_blocks, mut nblocks) = (Vec::new(), 0);
        if let Some(al) = file.attr(T_INDEX_ALLOC, "$I30") {
            nblocks = al.data_size() / self.idx_size;
            let bm = attrs.iter().find(|a| a.ty == T_BITMAP && a.name == utf16("$I30")).and_then(attr_value)
                .ok_or(FsError::Unsupported("index bitmap missing or non-resident"))?;
            let unit = self.idx_size / self.index_unit_w();
            for b in 0..nblocks {
                if (b / 8) as usize >= bm.len() || !bit(bm, b) { free_blocks.push(b * unit); }
            }
        }
        Ok(DirCtx { num: dnum, rec, attrs, file, root, root_hdr, free_blocks, nblocks })
    }

    fn index_unit_w(&self) -> u64 { if self.idx_size >= self.cs { self.cs } else { 512 } }

    fn read_node_w(&self, c: &DirCtx, loc: Loc) -> Result<Vec<Ent>> {
        match loc {
            Loc::Root => Ok(c.root.clone()),
            Loc::Block(vcn) => {
                let al = c.file.attr(T_INDEX_ALLOC, "$I30").ok_or(FsError::Corrupt("index has children but no allocation"))?;
                let off = vcn * self.index_unit_w();
                if off + self.idx_size > al.data_size() { return Err(FsError::Corrupt("index block outside allocation")); }
                let mut blk = vec![0u8; self.idx_size as usize];
                self.read_attr(al, off, &mut blk)?;
                if &blk[0..4] != b"INDX" { return Err(FsError::Corrupt("bad INDX signature")); }
                let (uo, uc) = (le16(&blk, 4) as usize, le16(&blk, 6) as usize);
                unfixup(&mut blk, uo, uc)?;
                if le64(&blk, 16) != vcn { return Err(FsError::Corrupt("INDX VCN mismatch")); }
                Ok(to_ents(parse_node(&blk, 24)?))
            }
        }
    }

    fn write_block_w(&self, c: &DirCtx, vcn: u64, ents: &[Ent]) -> Result<()> {
        let al = c.file.attr(T_INDEX_ALLOC, "$I30").ok_or(FsError::Corrupt("no index allocation"))?;
        self.write_block_to(al, vcn, ents)
    }

    fn write_block_to(&self, al: &Attr, vcn: u64, ents: &[Ent]) -> Result<()> {
        let mut b = vec![0u8; self.idx_size as usize];
        b[0..4].copy_from_slice(b"INDX");
        let uc = self.idx_size as usize / 512 + 1;
        put16(&mut b, 4, 0x28); put16(&mut b, 6, uc as u16);
        put64(&mut b, 16, vcn);
        let body = ser_ents(ents);
        put32(&mut b, 24, (ENTRIES_AT - 24) as u32);
        put32(&mut b, 28, (ENTRIES_AT - 24 + body.len()) as u32);
        put32(&mut b, 32, (self.idx_size as usize - 24) as u32);
        b[36] = ents.iter().any(|e| e.child.is_some()) as u8;
        put16(&mut b, 0x28, 1);
        b[ENTRIES_AT..ENTRIES_AT + body.len()].copy_from_slice(&body);
        fixup(&mut b);
        self.write_attr_range(al, vcn * self.index_unit_w(), &b)
    }

    fn root_attr(&self, c: &DirCtx, id: u16, ents: &[Ent]) -> RawAttr {
        let body = ser_ents(ents);
        let mut v = vec![0u8; 32 + body.len()];
        v[..16].copy_from_slice(&c.root_hdr);
        put32(&mut v, 16, 16); put32(&mut v, 20, (16 + body.len()) as u32); put32(&mut v, 24, (16 + body.len()) as u32);
        v[28] = ents.iter().any(|e| e.child.is_some()) as u8;
        v[32..].copy_from_slice(&body);
        res_attr(T_INDEX_ROOT, "$I30", id, 0, &v, 0)
    }

    fn node_fits(&self, c: &DirCtx, loc: Loc, ents: &[Ent]) -> bool {
        match loc {
            Loc::Block(_) => ents_len(ents) <= self.idx_size as usize - ENTRIES_AT,
            Loc::Root => {
                let others: usize = c.attrs.iter().filter(|a| !(a.ty == T_INDEX_ROOT && a.name == utf16("$I30"))).map(|a| a.b.len()).sum();
                let mine = align8(0x20 + 32 + ents_len(ents));
                le16(&c.rec.buf, 20) as usize + others + mine + 8 <= le32(&c.rec.buf, 28) as usize
            }
        }
    }

    fn cmp_ent(&self, e: &Ent, units: &[u16]) -> Ordering { self.cmp_units(&key_units(&e.key), units) }

    /// Descends to the node/position for `units`; `found` tells whether an equal key exists at stack.top().pos.
    fn locate(&self, c: &DirCtx, units: &[u16]) -> Result<(Vec<Pn>, bool)> {
        let (mut loc, mut stack) = (Loc::Root, Vec::new());
        for _ in 0..40 {
            let ents = self.read_node_w(c, loc)?;
            let mut pos = ents.len().saturating_sub(1);
            for (i, e) in ents.iter().enumerate() {
                if e.last { pos = i; break; }
                match self.cmp_ent(e, units) {
                    Ordering::Greater => { pos = i; break; }
                    Ordering::Equal => { stack.push(Pn { loc, ents, pos: i }); return Ok((stack, true)); }
                    Ordering::Less => {}
                }
            }
            match ents.get(pos).and_then(|e| e.child) {
                Some(ch) => { stack.push(Pn { loc, ents, pos }); loc = Loc::Block(ch); }
                None => { stack.push(Pn { loc, ents, pos }); return Ok((stack, false)); }
            }
        }
        Err(FsError::Corrupt("index too deep"))
    }

    fn take_block(&self, c: &DirCtx, plan: &mut Plan) -> std::result::Result<u64, Need> {
        let v = c.free_blocks.iter().copied().find(|v| !plan.used_blocks.contains(v)).ok_or(Need::Blocks)?;
        plan.used_blocks.push(v);
        Ok(v)
    }

    fn put_write(plan: &mut Plan, loc: Loc, ents: Vec<Ent>) {
        if let Some(w) = plan.writes.iter_mut().find(|w| w.0 == loc) { w.1 = ents; } else { plan.writes.push((loc, ents)); }
    }

    /// Resolves overflow bottom-up; `stack.last()` is the modified node.
    fn settle(&self, c: &DirCtx, plan: &mut Plan, mut stack: Vec<Pn>) -> std::result::Result<(), Need> {
        while let Some(cur) = stack.pop() {
            if self.node_fits(c, cur.loc, &cur.ents) { Self::put_write(plan, cur.loc, cur.ents); return Ok(()); }
            if cur.loc == Loc::Root {
                // Push the root's entries down into a new block; the root keeps a single pointer.
                let b = self.take_block(c, plan)?;
                // Write the new root now: if the pushed-down block fits, nothing below revisits it.
                Self::put_write(plan, Loc::Root, vec![Ent::end(Some(b))]);
                let root = Pn { loc: Loc::Root, ents: vec![Ent::end(Some(b))], pos: 0 };
                stack.push(root);
                stack.push(Pn { loc: Loc::Block(b), ents: cur.ents, pos: 0 });
                continue;
            }
            let Loc::Block(vcn) = cur.loc else { unreachable!() };
            let mut parent = stack.pop().ok_or(Need::Err(FsError::Corrupt("split without parent")))?;
            // split at the entry that best halves the byte size
            let n = cur.ents.len() - 1; // non-end entries
            if n < 2 { return Err(Need::Err(FsError::NoSpace)); }
            let (mut acc, half, mut m) = (0usize, ents_len(&cur.ents) / 2, 0usize);
            for (i, e) in cur.ents.iter().take(n).enumerate() { acc += e.len(); if acc >= half { m = i; break; } }
            m = m.clamp(1, n - 1);
            let mut left: Vec<Ent> = cur.ents[..m].to_vec();
            let med = cur.ents[m].clone();
            let mut right: Vec<Ent> = cur.ents[m + 1..].to_vec();
            left.push(Ent::end(med.child));
            let _ = &mut right;
            let rb = self.take_block(c, plan)?;
            Self::put_write(plan, Loc::Block(vcn), left);
            Self::put_write(plan, Loc::Block(rb), right);
            let promoted = Ent { mref: med.mref, key: med.key, child: Some(vcn), last: false };
            parent.ents[parent.pos].child = Some(rb);
            parent.ents.insert(parent.pos, promoted);
            stack.push(parent);
        }
        Ok(())
    }

    fn commit_plan(&self, c: &mut DirCtx, plan: &Plan) -> Result<()> {
        for (loc, ents) in &plan.writes {
            if let Loc::Block(v) = loc { self.write_block_w(c, *v, ents)?; }
        }
        let mut changed = false;
        if !plan.used_blocks.is_empty() {
            let unit = self.idx_size / self.index_unit_w();
            let bi = c.attrs.iter().position(|a| a.ty == T_BITMAP && a.name == utf16("$I30")).ok_or(FsError::Corrupt("index bitmap missing"))?;
            let mut val = attr_value(&c.attrs[bi]).unwrap().to_vec();
            for v in &plan.used_blocks { set_bit(&mut val, v / unit, true); }
            let id = c.attrs[bi].id;
            c.attrs[bi] = res_attr(T_BITMAP, "$I30", id, 0, &val, 0);
            changed = true;
        }
        if let Some((_, ents)) = plan.writes.iter().find(|w| w.0 == Loc::Root) {
            let ri = c.attrs.iter().position(|a| a.ty == T_INDEX_ROOT && a.name == utf16("$I30")).unwrap();
            let id = c.attrs[ri].id;
            c.attrs[ri] = self.root_attr(c, id, ents);
            changed = true;
        }
        if changed {
            join(&mut c.rec.buf, &c.attrs)?;
            self.rec_store(&c.rec)?;
        }
        Ok(())
    }

    /// Adds clusters to (or creates) the directory's INDEX_ALLOCATION and bitmap so a new block exists.
    fn extend_index(&self, dnum: u64) -> Result<()> {
        let mut c = self.dir_ctx(dnum)?;
        let per = (self.idx_size / self.cs).max(1);
        let existing = c.file.attr(T_INDEX_ALLOC, "$I30").cloned();
        let (mut runs, old_alloc, id) = match &existing {
            Some(a) => {
                let nr = a.nr.as_ref().ok_or(FsError::Corrupt("resident index allocation"))?;
                let id = c.attrs.iter().find(|x| x.ty == T_INDEX_ALLOC).map(|x| x.id).unwrap();
                (nr.runs.clone(), nr.alloc, id)
            }
            None => (Vec::new(), 0, next_attr_id(&mut c.rec.buf)),
        };
        let near = runs.last().and_then(|r| r.lcn.map(|l| l + r.len));
        let new = self.alloc_clusters(per, near)?;
        let base = runs.last().map(|r| r.vcn + r.len).unwrap_or(0);
        let mut vcn = base;
        for r in new {
            match runs.last_mut() {
                Some(l) if l.lcn.map(|x| x + l.len) == r.lcn => l.len += r.len,
                _ => runs.push(Run { vcn, len: r.len, lcn: r.lcn }),
            }
            vcn += r.len;
        }
        let alloc = old_alloc + per * self.cs;
        let attr = nonres_attr(T_INDEX_ALLOC, "$I30", id, 0, &runs, alloc, alloc, alloc, self.cs);
        match c.attrs.iter().position(|a| a.ty == T_INDEX_ALLOC) { Some(i) => c.attrs[i] = attr, None => c.attrs.push(attr) }
        let nblocks = alloc / self.idx_size;
        let need = align8(((nblocks + 7) / 8) as usize).max(8);
        match c.attrs.iter().position(|a| a.ty == T_BITMAP && a.name == utf16("$I30")) {
            Some(i) => {
                let mut v = attr_value(&c.attrs[i]).ok_or(FsError::Unsupported("non-resident index bitmap"))?.to_vec();
                if v.len() < need { v.resize(need, 0); }
                let id = c.attrs[i].id;
                c.attrs[i] = res_attr(T_BITMAP, "$I30", id, 0, &v, 0);
            }
            None => { let id = next_attr_id(&mut c.rec.buf); c.attrs.push(res_attr(T_BITMAP, "$I30", id, 0, &vec![0u8; need], 0)); }
        }
        // First conversion: the root is (nearly) full, which is why we are here. Move its entries into
        // block 0 in the same record rewrite so the record has room for the new attributes.
        let mut pushdown: Option<Vec<Ent>> = None;
        if existing.is_none() {
            let id0 = c.attrs.iter().find(|a| a.ty == T_INDEX_ROOT).map(|a| a.id).unwrap();
            pushdown = Some(c.root.clone());
            let ri = c.attrs.iter().position(|a| a.ty == T_INDEX_ROOT && a.name == utf16("$I30")).unwrap();
            c.attrs[ri] = self.root_attr(&c, id0, &[Ent::end(Some(0))]);
            let bi = c.attrs.iter().position(|a| a.ty == T_BITMAP && a.name == utf16("$I30")).unwrap();
            let mut v = attr_value(&c.attrs[bi]).unwrap().to_vec();
            set_bit(&mut v, 0, true);
            let id = c.attrs[bi].id;
            c.attrs[bi] = res_attr(T_BITMAP, "$I30", id, 0, &v, 0);
        }
        c.attrs.sort_by_key(|a| a.ty);
        let r = (|| -> Result<()> {
            if let Some(ents) = &pushdown {
                let tmp = Attr { ty: T_INDEX_ALLOC, name: utf16("$I30"), flags: 0, id, res: None,
                                 nr: Some(NonRes { runs: runs.clone(), alloc, size: alloc, init: alloc, comp_unit: 0 }) };
                self.write_block_to(&tmp, 0, ents)?;
            }
            join(&mut c.rec.buf, &c.attrs)?;
            self.rec_store(&c.rec)
        })();
        if r.is_err() { self.free_clusters(&runs[runs.len().saturating_sub(1)..]).ok(); }
        r
    }

    fn with_index_plan(&self, dnum: u64, build: &dyn Fn(&DirCtx, &mut Plan) -> std::result::Result<(), Need>) -> Result<()> {
        for _ in 0..16 {
            let mut c = self.dir_ctx(dnum)?;
            let mut plan = Plan { writes: Vec::new(), used_blocks: Vec::new() };
            match build(&c, &mut plan) {
                Ok(()) => return self.commit_plan(&mut c, &plan),
                Err(Need::Blocks) => self.extend_index(dnum)?,
                Err(Need::Err(e)) => return Err(e),
            }
        }
        Err(FsError::NoSpace)
    }

    fn idx_insert(&self, dnum: u64, key: Vec<u8>, mref: u64) -> Result<()> {
        let units = key_units(&key);
        self.with_index_plan(dnum, &|c, plan| {
            let (mut stack, found) = self.locate(c, &units)?;
            if found { return Err(Need::Err(FsError::AlreadyExists(String::from_utf16_lossy(&units)))); }
            let leaf = stack.last_mut().unwrap();
            let pos = leaf.pos;
            leaf.ents.insert(pos, Ent { mref, key: key.clone(), child: None, last: false });
            self.settle(c, plan, stack)
        })
    }

    fn idx_remove(&self, dnum: u64, units: &[u16], mref: u64) -> Result<()> {
        self.with_index_plan(dnum, &|c, plan| {
            let (mut stack, found) = self.locate(c, units)?;
            if !found { return Err(Need::Err(FsError::Corrupt("index entry to remove not found"))); }
            let node = stack.last_mut().unwrap();
            if node.ents[node.pos].mref & MFT_MASK != mref & MFT_MASK { return Err(Need::Err(FsError::Corrupt("index entry points to a different file"))); }
            let i = node.pos;
            let Some(child) = node.ents[i].child else {
                node.ents.remove(i);
                return self.settle(c, plan, stack);
            };
            // internal entry: replace with in-order predecessor (or successor) from a leaf
            let walk = |start: u64, rightmost: bool| -> std::result::Result<Vec<Pn>, Need> {
                let (mut loc, mut path) = (Loc::Block(start), Vec::new());
                for _ in 0..40 {
                    let ents = self.read_node_w(c, loc)?;
                    let pos = if rightmost { ents.len() - 1 } else { 0 };
                    let next = ents[pos].child;
                    path.push(Pn { loc, ents, pos });
                    match next { Some(ch) => loc = Loc::Block(ch), None => return Ok(path) }
                }
                Err(Need::Err(FsError::Corrupt("index too deep")))
            };
            let mut leafpath = walk(child, true)?;
            let leaf = leafpath.last_mut().unwrap();
            let moved = if leaf.ents.len() >= 2 { Some(leaf.ents.remove(leaf.ents.len() - 2)) } else { None };
            let moved = match moved {
                Some(m) => m,
                None => {
                    let next = stack.last().unwrap().ents[i + 1].child
                        .ok_or(Need::Err(FsError::Unsupported("directory index has an empty leaf here")))?;
                    leafpath = walk(next, false)?;
                    let leaf = leafpath.last_mut().unwrap();
                    if leaf.ents.len() < 2 { return Err(Need::Err(FsError::Unsupported("directory index has empty leaves here"))); }
                    leaf.ents.remove(0)
                }
            };
            let leaf = leafpath.pop().unwrap();
            Self::put_write(plan, leaf.loc, leaf.ents);
            let n = stack.last_mut().unwrap();
            n.ents[i].key = moved.key; n.ents[i].mref = moved.mref;
            self.settle(c, plan, stack)
        })
    }

    fn idx_update_key(&self, dnum: u64, units: &[u16], new_key: Vec<u8>) -> Result<()> {
        self.with_index_plan(dnum, &|c, plan| {
            let (mut stack, found) = self.locate(c, units)?;
            if !found { return Err(Need::Err(FsError::Corrupt("index entry to update not found"))); }
            let n = stack.last_mut().unwrap();
            let p = n.pos;
            if n.ents[p].key.len() != new_key.len() { return Err(Need::Err(FsError::Corrupt("key size changed"))); }
            n.ents[p].key = new_key.clone();
            self.settle(c, plan, stack)
        })
    }

    #[doc(hidden)]
    /// Test hook: walks a directory's whole index and checks strict ordering and a consistent structure.
    pub fn verify_dir_index(&self, path: &str) -> Result<usize> {
        let d = self.walk_dir(&normalize_path(path)?)?;
        let keys = self.dir_keys(&d)?;
        let mut prev: Option<Vec<u16>> = None;
        for (_, k) in &keys {
            let u = key_units(k);
            if let Some(p) = &prev { if self.cmp_units(p, &u) != Ordering::Less { return Err(FsError::Corrupt("index keys out of order")); } }
            prev = Some(u);
        }
        Ok(keys.len())
    }

    #[doc(hidden)]
    /// Test hook: simulates a crashed writer by raising the volume dirty bit.
    pub fn debug_set_volume_dirty(&self) -> Result<()> { self.set_dirty(true) }

    #[doc(hidden)]
    /// Test hook: byte offset of the first `$LogFile` cluster (so tests can fake an unclean log).
    pub fn debug_logfile_offset(&self) -> Result<u64> {
        let f = self.load_file(2)?;
        let nr = f.attr(T_DATA, "").and_then(|a| a.nr.as_ref()).ok_or(FsError::Corrupt("$LogFile resident"))?;
        Ok(nr.runs.first().and_then(|r| r.lcn).ok_or(FsError::Corrupt("$LogFile sparse"))? * self.cs)
    }

    #[doc(hidden)]
    pub fn dump_index(&self, path: &str) -> Result<String> {
        let d = self.walk_dir(&normalize_path(path)?)?;
        let c = self.dir_ctx(d.num)?;
        let mut out = String::new();
        let mut todo = vec![Loc::Root];
        while let Some(loc) = todo.pop() {
            let ents = self.read_node_w(&c, loc)?;
            out += &format!("{loc:?}: ");
            for e in &ents {
                let n = String::from_utf16_lossy(&key_units(&e.key));
                let short = if n.len() > 4 { n[n.len().saturating_sub(8)..].to_string() } else { n };
                out += &format!("[{}{}{}] ", short, if e.last { "END" } else { "" }, e.child.map(|c| format!("->{c}")).unwrap_or_default());
                if let Some(ch) = e.child { todo.push(Loc::Block(ch)); }
            }
            out += "\n";
        }
        Ok(out)
    }

    // ================================================================ file-level helpers
    fn name_units(&self, name: &str) -> Result<Vec<u16>> {
        validate_fat_name(name)?;
        Ok(utf16(name))
    }

    fn split_parent<'a>(&self, path: &'a str) -> Result<(Vec<&'a str>, &'a str)> {
        let comps = normalize_path(path)?;
        let (last, parents) = comps.split_last().ok_or(FsError::InvalidName("root".into()))?;
        Ok((parents.to_vec(), last))
    }

    /// (STANDARD_INFORMATION tail, descriptor bytes to copy) inherited from the parent. The descriptor is
    /// always copied by value: sharing a non-resident attribute's runs would make two files own the same clusters.
    fn security_for_child(&self, parent_attrs: &[RawAttr], parent: &File) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        let si = parent_attrs.iter().find(|a| a.ty == T_STDINFO).and_then(attr_value);
        match si {
            Some(v) if v.len() >= 72 => Ok((v[32..72].to_vec(), None)),
            _ => {
                let sd = match parent.attr(0x50, "") {
                    Some(a) => {
                        if a.data_size() > 1 << 16 { return Err(FsError::Unsupported("parent security descriptor too large to inherit")); }
                        Some(self.read_all(a, 1 << 16)?)
                    }
                    None => None,
                };
                Ok((vec![0u8; 16], sd))
            }
        }
    }

    /// Returns the record plus any clusters allocated for its security descriptor (caller frees them on failure).
    fn build_new_record(&self, num: u64, seq: u16, parent: &Rec, parent_attrs: &[RawAttr], parent_file: &File, units: &[u16], dir: bool,
                        data: Option<RawAttr>, size: u64, alloc: u64) -> Result<(Rec, Vec<Run>)> {
        let mut sd_runs: Vec<Run> = Vec::new();
        let r = self.build_new_record_inner(num, seq, parent, parent_attrs, parent_file, units, dir, data, size, alloc, &mut sd_runs);
        match r { Ok(rec) => Ok((rec, sd_runs)), Err(e) => { self.free_clusters(&sd_runs).ok(); Err(e) } }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_new_record_inner(&self, num: u64, seq: u16, parent: &Rec, parent_attrs: &[RawAttr], parent_file: &File, units: &[u16], dir: bool,
                        data: Option<RawAttr>, size: u64, alloc: u64, sd_runs: &mut Vec<Run>) -> Result<Rec> {
        let mut r = self.fresh_record(num, seq, dir);
        let now = now_ft();
        let (tail, sd) = self.security_for_child(parent_attrs, parent_file)?;
        let fflags: u32 = 0x20; // ARCHIVE, as ntfs-3g
        let mut si = vec![0u8; 32 + tail.len()];
        for i in 0..4 { put64(&mut si, i * 8, now); }
        put32(&mut si, 32 - 0, fflags);
        // tail layout: [0..4] flags(already) — rebuild: flags + max_ver + ver + class + owner + sec + quota + usn
        if tail.len() == 40 { si[32..72].copy_from_slice(&tail); put32(&mut si, 32, fflags); put32(&mut si, 36, 0); put32(&mut si, 40, 0); put32(&mut si, 44, 0); put64(&mut si, 56, 0); put64(&mut si, 64, 0); }
        else { si.truncate(48); put32(&mut si, 32, fflags); }
        let id0 = next_attr_id(&mut r.buf);
        let mut attrs = vec![res_attr(T_STDINFO, "", id0, 0, &si, 0)];
        let pref = parent.num | ((le16(&parent.buf, 16) as u64) << 48);
        let (fa, fl) = if dir { (0x1000_0000 | fflags, 0u64) } else { (fflags, 0) };
        let _ = fl;
        let fnv = filename_value(pref, now, alloc, size, fa, units, 1);
        let id1 = next_attr_id(&mut r.buf);
        attrs.push(res_attr(T_FILENAME, "", id1, 0, &fnv, 1));
        if let Some(sd) = sd {
            let id = next_attr_id(&mut r.buf);
            if sd.len() <= 160 { attrs.push(res_attr(0x50, "", id, 0, &sd, 0)); }
            else {
                let runs = self.alloc_clusters((sd.len() as u64).div_ceil(self.cs), None)?;
                *sd_runs = runs.clone();
                self.write_runs(&runs, &sd)?;
                let al = runs.iter().map(|r| r.len).sum::<u64>() * self.cs;
                attrs.push(nonres_attr(0x50, "", id, 0, &runs, al, sd.len() as u64, sd.len() as u64, self.cs));
            }
        }
        if dir {
            let id = next_attr_id(&mut r.buf);
            // new root header copied from the parent's index root
            let pri = parent_attrs.iter().find(|a| a.ty == T_INDEX_ROOT).and_then(attr_value).ok_or(FsError::Corrupt("parent has no index root"))?;
            let mut v = vec![0u8; 48];
            v[..16].copy_from_slice(&pri[..16]);
            put32(&mut v, 16, 16); put32(&mut v, 20, 32); put32(&mut v, 24, 32);
            put32(&mut v, 32 + 8, 16); put16(&mut v, 32 + 12, 2);
            attrs.push(res_attr(T_INDEX_ROOT, "$I30", id, 0, &v, 0));
        } else if let Some(mut d) = data {
            let id = next_attr_id(&mut r.buf); put16(&mut d.b, 14, id); d.id = id; attrs.push(d);
        }
        attrs.sort_by_key(|a| a.ty);
        join(&mut r.buf, &attrs)?;
        Ok(r)
    }

    fn entry_after(&self, num: u64, name: &str) -> Result<DirEntry> {
        self.invalidate();
        let f = self.load_file(num)?;
        let dir = f.is_dir();
        Ok(DirEntry {
            name: name.to_string(), kind: if dir { EntryKind::Directory } else { EntryKind::File },
            size: if dir { 0 } else { f.attr(T_DATA, "").map(|a| a.data_size()).unwrap_or(0) },
            modified: None, created: None, hidden: false, read_only: false, inode: num,
        })
    }

    /// Reads up to `limit` bytes from `src` (loops until full or EOF).
    fn fill(src: &mut dyn Read, buf: &mut Vec<u8>, limit: usize) -> Result<()> {
        let start = buf.len();
        buf.resize(limit, 0);
        let mut got = start;
        while got < limit {
            match src.read(&mut buf[got..]) { Ok(0) => break, Ok(n) => got += n, Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}, Err(e) => { buf.truncate(got); return Err(FsError::Source(e.to_string())); } }
        }
        buf.truncate(got);
        Ok(())
    }

    /// Streams `first` + the rest of `src` into freshly allocated clusters. Returns (runs, size).
    fn stream_clusters(&self, src: &mut dyn Read, first: Vec<u8>) -> Result<(Vec<Run>, u64)> {
        let mut runs: Vec<Run> = Vec::new();
        let (mut size, mut buf) = (0u64, first);
        let mut res: Result<()> = Ok(());
        loop {
            // Every chunk but the last must be cluster-aligned: top up whatever is buffered to a full chunk.
            res = Self::fill(src, &mut buf, CHUNK);
            if res.is_err() || buf.is_empty() { break; }
            let clusters = (buf.len() as u64).div_ceil(self.cs);
            let near = runs.last().and_then(|r| r.lcn.map(|l| l + r.len));
            let got = match self.alloc_clusters(clusters, near) { Ok(g) => g, Err(e) => { res = Err(e); break; } };
            if let Err(e) = self.write_runs(&got, &buf) { self.free_clusters(&got).ok(); res = Err(e); break; }
            let mut vcn = runs.last().map(|r| r.vcn + r.len).unwrap_or(0);
            for g in got {
                match runs.last_mut() { Some(l) if l.lcn.map(|x| x + l.len) == g.lcn => l.len += g.len, _ => runs.push(Run { vcn, len: g.len, lcn: g.lcn }) }
                vcn += g.len;
            }
            size += buf.len() as u64;
            buf = Vec::new();
        }
        match res { Ok(()) => Ok((runs, size)), Err(e) => { self.free_clusters(&runs).ok(); Err(e) } }
    }

    fn data_attr_for(&self, id: u16, first: &[u8], runs: &[Run], size: u64) -> RawAttr {
        if runs.is_empty() { res_attr(T_DATA, "", id, 0, first, 0) }
        else { let alloc = runs.iter().map(|r| r.len).sum::<u64>() * self.cs; nonres_attr(T_DATA, "", id, 0, runs, alloc, size, size, self.cs) }
    }

    // ================================================================ public operations
    pub(crate) fn w_create_dir(&self, path: &str) -> Result<DirEntry> {
        self.op(|| {
            let (parents, name) = self.split_parent(path)?;
            let units = self.name_units(name)?;
            let pdir = self.walk_dir(&parents)?;
            if self.find_in(&pdir, name)?.is_some() { return Err(FsError::AlreadyExists(path.into())); }
            let parent = self.rec_load(pdir.num)?;
            let pattrs = split(&parent.buf)?;
            let (num, seq) = self.alloc_record()?;
            let (rec, sd_runs) = match self.build_new_record(num, seq, &parent, &pattrs, &pdir, &units, true, None, 0, 0) {
                Ok(r) => r, Err(e) => { self.free_record(num).ok(); return Err(e); }
            };
            let res = self.rec_store(&rec).and_then(|_| {
                let key = key_of(&rec)?;
                self.idx_insert(pdir.num, key, num | ((seq as u64) << 48))
            });
            if let Err(e) = res { self.free_clusters(&sd_runs).ok(); self.free_record(num).ok(); return Err(e); }
            let mut p = self.rec_load(pdir.num)?; self.touch(&mut p, true)?; self.rec_store(&p)?;
            self.entry_after(num, name)
        })
    }

    pub(crate) fn w_write_file(&self, path: &str, src: &mut dyn Read, overwrite: bool) -> Result<DirEntry> {
        self.op(|| {
            let (parents, name) = self.split_parent(path)?;
            let units = self.name_units(name)?;
            let pdir = self.walk_dir(&parents)?;
            let existing = self.find_in(&pdir, name)?;
            if let Some((_, n)) = &existing {
                if !overwrite { return Err(FsError::AlreadyExists(path.into())); }
                let f = self.load_file(*n)?;
                if f.is_dir() { return Err(FsError::IsADirectory(path.into())); }
                self.check_simple_file(&f)?;
            }
            // 1. data
            let mut first = Vec::new();
            Self::fill(src, &mut first, self.rec_size as usize)?;
            let small = first.len() < self.rec_size as usize;
            let (mut runs, mut size) = (Vec::new(), first.len() as u64);
            let mut resident_bytes = first.clone();
            if !small || !self.resident_fits(first.len()) {
                let (r, s) = self.stream_clusters(src, std::mem::take(&mut first))?;
                runs = r; size = s; resident_bytes = Vec::new();
            }
            let alloc = runs.iter().map(|r| r.len).sum::<u64>() * self.cs;
            let result = match existing {
                None => self.create_file_record(&pdir, name, &units, &resident_bytes, &runs, size, alloc),
                Some((_, n)) => self.replace_file_data(n, &resident_bytes, &runs, size, alloc),
            };
            match result {
                Ok(n) => {
                    let mut p = self.rec_load(pdir.num)?; self.touch(&mut p, true)?; self.rec_store(&p)?;
                    self.entry_after(n, name)
                }
                Err(e) => { self.free_clusters(&runs).ok(); Err(e) }
            }
        })
    }

    fn resident_fits(&self, n: usize) -> bool {
        // conservative: header (0x38+8) + stdinfo (0x18+72) + filename (0x18+66+2*255 max) + data attr
        n + 0x18 + 0x60 + 0x18 + 66 + 2 * 255 + 0x40 + 16 < self.rec_size as usize
    }

    fn check_simple_file(&self, f: &File) -> Result<()> {
        let raw = split(&self.rec_load(f.num)?.buf)?;
        if raw.iter().any(|a| a.ty == T_ATTRLIST) { return Err(FsError::Unsupported("file with an attribute list")); }
        if le16(&self.rec_load(f.num)?.buf, 18) != 1 { return Err(FsError::Unsupported("file with hard links")); }
        for a in &f.attrs {
            if a.ty == T_DATA && a.flags & 0xC001 != 0 { return Err(FsError::Unsupported("compressed, encrypted or sparse file")); }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn create_file_record(&self, pdir: &File, name: &str, units: &[u16], first: &[u8], runs: &[Run], size: u64, alloc: u64) -> Result<u64> {
        let parent = self.rec_load(pdir.num)?;
        let pattrs = split(&parent.buf)?;
        let (num, seq) = self.alloc_record()?;
        let mut tmp = self.fresh_record(num, seq, false);
        let id = le16(&tmp.buf, 40) + 2; // ids 0,1 go to STDINFO/FILENAME; DATA gets a later one
        let _ = &mut tmp;
        let data = self.data_attr_for(id, first, runs, size);
        let (rec, sd_runs) = match self.build_new_record(num, seq, &parent, &pattrs, pdir, units, false, Some(data), size, alloc) {
            Ok(r) => r, Err(e) => { self.free_record(num).ok(); return Err(e); }
        };
        let res = self.rec_store(&rec).and_then(|_| self.idx_insert(pdir.num, key_of(&rec)?, num | ((seq as u64) << 48)));
        if let Err(e) = res { self.free_clusters(&sd_runs).ok(); self.free_record(num).ok(); return Err(e); }
        let _ = name;
        Ok(num)
    }

    fn replace_file_data(&self, n: u64, first: &[u8], runs: &[Run], size: u64, alloc: u64) -> Result<u64> {
        let f = self.load_file(n)?;
        let old: Vec<Run> = f.attr(T_DATA, "").and_then(|a| a.nr.as_ref()).map(|nr| nr.runs.clone()).unwrap_or_default();
        let mut rec = self.rec_load(n)?;
        let mut attrs = split(&rec.buf)?;
        let di = attrs.iter().position(|a| a.ty == T_DATA && a.name.is_empty());
        let id = match di { Some(i) => attrs[i].id, None => next_attr_id(&mut rec.buf) };
        let new = self.data_attr_for(id, first, runs, size);
        match di { Some(i) => attrs[i] = new, None => attrs.push(new) }
        let now = now_ft();
        let mut parents: Vec<(u64, Vec<u8>, Vec<u8>)> = Vec::new();
        for a in attrs.iter_mut() {
            if a.ty == T_STDINFO {
                let vo = le16(&a.b, 20) as usize;
                put64(&mut a.b, vo + 8, now); put64(&mut a.b, vo + 16, now); put64(&mut a.b, vo + 24, now);
            } else if a.ty == T_FILENAME {
                let vo = le16(&a.b, 20) as usize;
                let before = a.b[vo..vo + le32(&a.b, 16) as usize].to_vec();
                for i in 0..4 { put64(&mut a.b, vo + 8 + i * 8, now); }
                put64(&mut a.b, vo + 40, alloc); put64(&mut a.b, vo + 48, size);
                let after = a.b[vo..vo + le32(&a.b, 16) as usize].to_vec();
                parents.push((le64(&before, 0) & MFT_MASK, before, after));
            }
        }
        attrs.sort_by_key(|a| a.ty);
        join(&mut rec.buf, &attrs)?;
        self.rec_store(&rec)?;
        for (p, before, after) in parents {
            let _ = before;
            self.idx_update_key(p, &key_units(&after), after)?;
        }
        self.free_clusters(&old)?;
        Ok(n)
    }

    pub(crate) fn w_remove(&self, path: &str) -> Result<()> {
        self.op(|| {
            let (parents, name) = self.split_parent(path)?;
            let pdir = self.walk_dir(&parents)?;
            let (_, n) = self.find_in(&pdir, name)?.ok_or_else(|| FsError::NotFound(path.into()))?;
            let f = self.load_file(n)?;
            if n < 24 { return Err(FsError::Refused("refusing to delete an NTFS system file")); }
            if f.is_dir() {
                if !self.list_file(&f)?.is_empty() { return Err(FsError::NotEmpty(path.into())); }
            } else { self.check_simple_file(&f)?; }
            if f.attrs.iter().any(|a| a.ty == T_FILENAME && a.nr.is_some()) { return Err(FsError::Corrupt("non-resident FILE_NAME")); }
            let rec = self.rec_load(n)?;
            let raw = split(&rec.buf)?;
            if raw.iter().any(|a| a.ty == T_ATTRLIST) { return Err(FsError::Unsupported("file with an attribute list")); }
            let mref_seq = le16(&rec.buf, 16);
            // 1. unlink from every parent index
            for a in raw.iter().filter(|a| a.ty == T_FILENAME) {
                let v = attr_value(a).ok_or(FsError::Corrupt("non-resident FILE_NAME"))?;
                self.idx_remove(le64(v, 0) & MFT_MASK, &key_units(v), n | ((mref_seq as u64) << 48))?;
            }
            // 2. free clusters and the record
            let mut all: Vec<Run> = Vec::new();
            for a in &f.attrs { if let Some(nr) = &a.nr { all.extend(nr.runs.iter().cloned()); } }
            self.free_clusters(&all)?;
            let mut dead = self.rec_load(n)?;
            let fl = le16(&dead.buf, 22) & !1;
            put16(&mut dead.buf, 22, fl);
            put16(&mut dead.buf, 18, 0);
            self.rec_store(&dead)?;
            self.free_record(n)?;
            let mut p = self.rec_load(pdir.num)?; self.touch(&mut p, true)?; self.rec_store(&p)?;
            Ok(())
        })
    }

    pub(crate) fn w_rename(&self, from: &str, to: &str) -> Result<()> {
        self.op(|| {
            let (fp, fname) = self.split_parent(from)?;
            let (tp, tname) = self.split_parent(to)?;
            let tunits = self.name_units(tname)?;
            let src_dir = self.walk_dir(&fp)?;
            let dst_dir = self.walk_dir(&tp)?;
            let (_, n) = self.find_in(&src_dir, fname)?.ok_or_else(|| FsError::NotFound(from.into()))?;
            if n < 24 { return Err(FsError::Refused("refusing to rename an NTFS system file")); }
            if let Some((_, other)) = self.find_in(&dst_dir, tname)? {
                if other != n { return Err(FsError::AlreadyExists(to.into())); }
            }
            let f = self.load_file(n)?;
            if f.is_dir() {
                // cannot move a directory into itself
                let mut cur = dst_dir.num;
                for _ in 0..4096 {
                    if cur == n { return Err(FsError::InvalidName("cannot move a directory into itself".into())); }
                    if cur == 5 { break; }
                    let g = self.load_file(cur)?;
                    let fnm = g.attrs.iter().find(|a| a.ty == T_FILENAME).and_then(|a| a.res.as_ref()).ok_or(FsError::Corrupt("directory without name"))?;
                    cur = le64(fnm, 0) & MFT_MASK;
                }
            } else { self.check_simple_file(&f)?; }
            let mut rec = self.rec_load(n)?;
            let mut attrs = split(&rec.buf)?;
            if attrs.iter().any(|a| a.ty == T_ATTRLIST) { return Err(FsError::Unsupported("file with an attribute list")); }
            let seq = le16(&rec.buf, 16);
            let mref = n | ((seq as u64) << 48);
            let old_names: Vec<Vec<u8>> = attrs.iter().filter(|a| a.ty == T_FILENAME).filter_map(|a| attr_value(a).map(|v| v.to_vec())).collect();
            let template = old_names.iter().find(|v| v[65] != 2).or(old_names.first()).cloned().ok_or(FsError::Corrupt("file has no name"))?;
            let dparent = self.rec_load(dst_dir.num)?;
            let pref = dst_dir.num | ((le16(&dparent.buf, 16) as u64) << 48);
            let now = now_ft();
            let mut key = filename_value(pref, le64(&template, 8), le64(&template, 40), le64(&template, 48), le32(&template, 56), &tunits, 1);
            for i in 1..4 { put64(&mut key, 8 + i * 8, le64(&template, 8 + i * 8)); }
            let _ = now;
            // 1. unlink old names
            let mut removed: Vec<Vec<u8>> = Vec::new();
            for v in &old_names {
                self.idx_remove(le64(v, 0) & MFT_MASK, &key_units(v), mref)?;
                removed.push(v.clone());
            }
            // 2. relink under the new name; restore the old names if that fails
            let relink = (|| -> Result<()> {
                let first = attrs.iter().position(|a| a.ty == T_FILENAME).unwrap();
                let id = attrs[first].id;
                attrs.retain(|a| a.ty != T_FILENAME);
                attrs.push(res_attr(T_FILENAME, "", id, 0, &key, 1));
                attrs.sort_by_key(|a| a.ty);
                join(&mut rec.buf, &attrs)?;
                self.rec_store(&rec)?;
                self.idx_insert(dst_dir.num, key.clone(), mref)
            })();
            if let Err(e) = relink {
                for v in removed { self.idx_insert(le64(&v, 0) & MFT_MASK, v, mref).ok(); }
                return Err(e);
            }
            let mut r = self.rec_load(n)?; self.touch(&mut r, false)?; self.rec_store(&r)?;
            for d in [src_dir.num, dst_dir.num] { let mut p = self.rec_load(d)?; self.touch(&mut p, true)?; self.rec_store(&p)?; }
            Ok(())
        })
    }
}

const ROOT_REC_W: u64 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    fn decode_signed(b: &[u8]) -> Vec<(i64, Option<i64>)> {
        // an independent reader following ntfs-3g: length and offset are both signed little-endian
        let (mut i, mut out) = (0, vec![]);
        while b[i] != 0 {
            let (lb, ob) = ((b[i] & 15) as usize, (b[i] >> 4) as usize); i += 1;
            let mut len = b[i + lb - 1] as i8 as i64; for k in (0..lb - 1).rev() { len = (len << 8) + b[i + k] as i64 } i += lb;
            let off = if ob == 0 { None } else { let mut o = b[i + ob - 1] as i8 as i64; for k in (0..ob - 1).rev() { o = (o << 8) + b[i + k] as i64 } i += ob; Some(o) };
            out.push((len, off));
        }
        out
    }
    #[test]
    fn run_lengths_and_offsets_round_trip_as_signed_numbers() {
        for len in [1u64, 2, 127, 128, 129, 171, 255, 256, 32767, 32768, 65535, 65536, 8_388_607, 8_388_608, 1 << 31, (1 << 32) + 5] {
            for lcn in [0u64, 1, 127, 128, 255, 256, 5119, 1 << 20, 1 << 33] {
                let enc = encode_runs(&[Run { vcn: 0, len, lcn: Some(lcn) }, Run { vcn: len, len: 3, lcn: Some(lcn + 1000) }]);
                let d = decode_signed(&enc);
                assert_eq!(d[0].0, len as i64, "length {len}");
                assert_eq!(d[0].1, Some(lcn as i64), "lcn {lcn}");
                assert_eq!(d[1], (3, Some(1000)));
            }
        }
        // sparse run too
        assert_eq!(decode_signed(&encode_runs(&[Run { vcn: 0, len: 200, lcn: None }]))[0], (200, None));
    }
}

fn key_of(rec: &Rec) -> Result<Vec<u8>> {
    split(&rec.buf)?.iter().find(|a| a.ty == T_FILENAME).and_then(attr_value).map(|v| v.to_vec()).ok_or(FsError::Corrupt("new record has no FILE_NAME"))
}
