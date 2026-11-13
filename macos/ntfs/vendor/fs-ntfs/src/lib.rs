//! NTFS driver. This file is the read path; `write.rs` adds mutation behind the same
//! safety rules as the FAT/exFAT drivers (writable device, clean volume, no hibernation).
//!
//! Supported (read): MFT with fragmented/attribute-list files, resident and non-resident data,
//! sparse runs, B-tree directories (INDEX_ROOT + INDEX_ALLOCATION), Win32/POSIX names (DOS short
//! names are hidden), $UpCase case-insensitive lookup, volume label.
//! Not supported: compressed (LZNT1) and EFS-encrypted data, dedup/cloud reparse points
//! (reads fail with `Unsupported`), alternate data streams, ACLs, hard-link enumeration.

use block_device::BlockDevice;
use fs_core::{normalize_path, Capabilities, DirEntry, EntryKind, Filesystem, FsError, Result};
use std::sync::{Arc, Mutex};

pub(crate) fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
pub(crate) fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
pub(crate) fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

pub(crate) const MFT_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
pub(crate) const T_STDINFO: u32 = 0x10;
pub(crate) const T_ATTRLIST: u32 = 0x20;
pub(crate) const T_FILENAME: u32 = 0x30;
pub(crate) const T_VOLNAME: u32 = 0x60;
pub(crate) const T_VOLINFO: u32 = 0x70;
pub(crate) const T_DATA: u32 = 0x80;
pub(crate) const T_INDEX_ROOT: u32 = 0x90;
pub(crate) const T_INDEX_ALLOC: u32 = 0xA0;
pub(crate) const T_BITMAP: u32 = 0xB0;
pub(crate) const T_END: u32 = 0xFFFF_FFFF;
const ROOT_REC: u64 = 5;
const MAX_FILE_RECORDS: u64 = 1 << 32;
const FILETIME_UNIX: i64 = 11_644_473_600;

#[derive(Clone, Debug)]
pub(crate) struct Run { pub vcn: u64, pub len: u64, pub lcn: Option<u64> }

#[derive(Clone, Debug, Default)]
pub(crate) struct NonRes { pub runs: Vec<Run>, pub alloc: u64, pub size: u64, pub init: u64, pub comp_unit: u8 }

#[derive(Clone, Debug)]
pub(crate) struct Attr {
    pub ty: u32, pub name: Vec<u16>, pub flags: u16, pub id: u16,
    pub res: Option<Vec<u8>>, pub nr: Option<NonRes>,
}

#[derive(Debug)]
pub(crate) struct File { pub num: u64, pub rec_flags: u16, pub attrs: Vec<Attr> }

impl File {
    pub fn attr(&self, ty: u32, name: &str) -> Option<&Attr> {
        let n: Vec<u16> = name.encode_utf16().collect();
        self.attrs.iter().find(|a| a.ty == ty && a.name == n)
    }
    pub fn is_dir(&self) -> bool { self.rec_flags & 2 != 0 }
}

impl Attr {
    pub fn data_size(&self) -> u64 {
        match (&self.res, &self.nr) { (Some(r), _) => r.len() as u64, (_, Some(n)) => n.size, _ => 0 }
    }
}

pub struct NtfsFs {
    pub(crate) dev: Arc<dyn BlockDevice>,
    pub(crate) bps: u64,
    pub(crate) cs: u64,
    pub(crate) total_clusters: u64,
    pub(crate) rec_size: u64,
    pub(crate) idx_size: u64,
    pub(crate) mft_lcn: u64,
    pub(crate) mft_runs: Mutex<Vec<Run>>,
    pub(crate) upcase: Vec<u16>,
    label: Option<String>,
    pub(crate) vol_flags: u16,
    pub(crate) version: (u8, u8),
    cache: Mutex<Option<(u64, Arc<File>)>>,
    pub(crate) wlock: Mutex<()>,
    pub(crate) mftmirr_lcn: u64,
    /// Why writing is refused for this volume (None = writable on a read-write device).
    pub(crate) write_block: Option<&'static str>,
    pub(crate) cbm: Mutex<Option<Vec<u8>>>,
    pub(crate) hint: Mutex<u64>,
}

pub(crate) fn parse_runs(b: &[u8], start_vcn: u64) -> Result<Vec<Run>> {
    let (mut i, mut vcn, mut lcn) = (0usize, start_vcn, 0i64);
    let mut out = Vec::new();
    while i < b.len() && b[i] != 0 {
        let (lb, ob) = ((b[i] & 0xF) as usize, (b[i] >> 4) as usize);
        i += 1;
        if lb == 0 || lb > 8 || ob > 8 || i + lb + ob > b.len() { return Err(FsError::Corrupt("bad data run")); }
        let mut len = 0u64;
        for k in 0..lb { len |= (b[i + k] as u64) << (8 * k); }
        i += lb;
        let l = if ob == 0 { None } else {
            let mut d = 0i64;
            for k in 0..ob { d |= (b[i + k] as i64) << (8 * k); }
            if b[i + ob - 1] & 0x80 != 0 && ob < 8 { d -= 1i64 << (8 * ob); }
            i += ob;
            lcn = lcn.checked_add(d).filter(|&v| v >= 0).ok_or(FsError::Corrupt("run LCN out of range"))?;
            Some(lcn as u64)
        };
        if len == 0 { return Err(FsError::Corrupt("zero-length run")); }
        out.push(Run { vcn, len, lcn: l });
        vcn = vcn.checked_add(len).ok_or(FsError::Corrupt("run overflow"))?;
    }
    Ok(out)
}

/// Verifies and removes update-sequence fixups in place (`stride` is always 512).
pub(crate) fn unfixup(buf: &mut [u8], usa_off: usize, usa_count: usize) -> Result<()> {
    if usa_count < 2 || usa_off + usa_count * 2 > buf.len() || (usa_count - 1) * 512 > buf.len() || usa_off < 8 {
        return Err(FsError::Corrupt("bad update sequence array"));
    }
    let usn = [buf[usa_off], buf[usa_off + 1]];
    for i in 1..usa_count {
        let p = i * 512 - 2;
        if buf[p..p + 2] != usn { return Err(FsError::Corrupt("update sequence mismatch (torn write?)")); }
        buf[p] = buf[usa_off + 2 * i]; buf[p + 1] = buf[usa_off + 2 * i + 1];
    }
    Ok(())
}

pub(crate) fn parse_attrs(buf: &[u8]) -> Result<Vec<Attr>> {
    let used = (le32(buf, 24) as usize).min(buf.len());
    let mut o = le16(buf, 20) as usize;
    let mut out = Vec::new();
    loop {
        if o + 8 > used { return Err(FsError::Corrupt("attribute list runs past record")); }
        let ty = le32(buf, o);
        if ty == T_END { break; }
        let len = le32(buf, o + 4) as usize;
        if len < 24 || len % 8 != 0 || o + len > used { return Err(FsError::Corrupt("bad attribute length")); }
        let a = &buf[o..o + len];
        let (nlen, noff) = (a[9] as usize, le16(a, 10) as usize);
        if nlen > 0 && noff + nlen * 2 > len { return Err(FsError::Corrupt("attribute name out of bounds")); }
        let name: Vec<u16> = (0..nlen).map(|i| le16(a, noff + i * 2)).collect();
        let mut at = Attr { ty, name, flags: le16(a, 12), id: le16(a, 14), res: None, nr: None };
        if a[8] == 0 {
            let (vl, vo) = (le32(a, 16) as usize, le16(a, 20) as usize);
            if vo + vl > len { return Err(FsError::Corrupt("resident value out of bounds")); }
            at.res = Some(a[vo..vo + vl].to_vec());
        } else {
            if len < 64 { return Err(FsError::Corrupt("short non-resident header")); }
            let (svcn, mp) = (le64(a, 16), le16(a, 32) as usize);
            if mp > len { return Err(FsError::Corrupt("mapping pairs out of bounds")); }
            at.nr = Some(NonRes {
                runs: parse_runs(&a[mp..], svcn)?, alloc: le64(a, 40), size: le64(a, 48), init: le64(a, 56),
                comp_unit: if len >= 72 { le16(a, 34) as u8 } else { 0 },
            });
        }
        out.push(at);
        o += len;
    }
    Ok(out)
}

fn filetime(t: u64) -> Option<i64> { if t == 0 { None } else { Some((t / 10_000_000) as i64 - FILETIME_UNIX) } }

pub(crate) struct IdxEntry { pub mref: u64, pub flags: u16, pub key: Vec<u8>, pub child: Option<u64> }

pub(crate) fn parse_node(buf: &[u8], h: usize) -> Result<Vec<IdxEntry>> {
    if h + 16 > buf.len() { return Err(FsError::Corrupt("index node header")); }
    let (eo, il) = (le32(buf, h) as usize, le32(buf, h + 4) as usize);
    let end = h.checked_add(il).filter(|&e| e <= buf.len()).ok_or(FsError::Corrupt("index length out of bounds"))?;
    let mut e = h.checked_add(eo).ok_or(FsError::Corrupt("index entries offset"))?;
    let mut out = Vec::new();
    while e + 16 <= end {
        let (elen, klen, flags) = (le16(buf, e + 8) as usize, le16(buf, e + 10) as usize, le16(buf, e + 12));
        if elen < 16 || e + elen > end || 16 + klen > elen { return Err(FsError::Corrupt("bad index entry")); }
        let child = if flags & 1 != 0 { if elen < 24 { return Err(FsError::Corrupt("index entry too short")); } Some(le64(buf, e + elen - 8)) } else { None };
        out.push(IdxEntry { mref: le64(buf, e), flags, key: buf[e + 16..e + 16 + klen].to_vec(), child });
        if flags & 2 != 0 { break; }
        e += elen;
    }
    Ok(out)
}

pub(crate) struct FileName { pub parent: u64, pub name: String, pub units: Vec<u16>, pub ns: u8, pub flags: u32 }
pub(crate) fn parse_filename(k: &[u8]) -> Option<FileName> {
    if k.len() < 66 { return None; }
    let nl = k[64] as usize;
    if 66 + nl * 2 > k.len() { return None; }
    let units: Vec<u16> = (0..nl).map(|i| le16(k, 66 + i * 2)).collect();
    Some(FileName { parent: le64(k, 0), name: String::from_utf16_lossy(&units), units, ns: k[65], flags: le32(k, 56) })
}

impl NtfsFs {
    pub fn open(dev: Arc<dyn BlockDevice>) -> Result<Self> {
        let mut s = [0u8; 512];
        if dev.size() < 8192 { return Err(FsError::Corrupt("too small for NTFS")); }
        dev.read_at(0, &mut s)?;
        if &s[3..11] != b"NTFS    " || s[510] != 0x55 || s[511] != 0xAA { return Err(FsError::Corrupt("not an NTFS boot sector")); }
        let bps = le16(&s, 11) as u64;
        let spc = if s[13] <= 0x80 { s[13] as u64 } else { 1u64 << (256 - s[13] as u32).min(31) };
        if !matches!(bps, 512 | 1024 | 2048 | 4096) || spc == 0 || !spc.is_power_of_two() { return Err(FsError::Corrupt("bad NTFS geometry")); }
        let cs = bps * spc;
        let total_sectors = le64(&s, 40);
        let mft_lcn = le64(&s, 48);
        // Positive: clusters; negative: 2^-n bytes (n is untrusted, so bound it before shifting).
        let size_of = |b: u8| -> Option<u64> {
            let v = b as i8;
            if v > 0 { Some(v as u64 * cs) } else if (9..=20).contains(&-(v as i32)) { Some(1u64 << -(v as i32)) } else { None }
        };
        let (Some(rec_size), Some(idx_size)) = (size_of(s[64]), size_of(s[68])) else { return Err(FsError::Corrupt("bad record/index size")) };
        let total_clusters = total_sectors / spc;
        if total_sectors == 0 || total_sectors.saturating_mul(bps) > dev.size() || rec_size < 512 || rec_size > 65536 || !rec_size.is_power_of_two()
            || idx_size < 512 || idx_size > 1 << 20 || !idx_size.is_power_of_two()
            || mft_lcn == 0 || mft_lcn >= total_clusters || mft_lcn.saturating_mul(cs).saturating_add(rec_size) > total_sectors * bps {
            return Err(FsError::Corrupt("inconsistent NTFS boot sector"));
        }
        let mut fs = Self {
            dev, bps, cs, total_clusters, rec_size, idx_size, mft_lcn, mft_runs: Mutex::new(vec![]), upcase: Vec::new(),
            label: None, vol_flags: 0, version: (0, 0), cache: Mutex::new(None), wlock: Mutex::new(()),
            mftmirr_lcn: le64(&s, 56), write_block: None, cbm: Mutex::new(None), hint: Mutex::new(0),
        };
        // Bootstrap: record 0 describes the MFT's own data runs.
        let mut r0 = vec![0u8; rec_size as usize];
        fs.dev.read_at(mft_lcn * cs, &mut r0)?;
        let attrs = fs.parse_record(&mut r0, 0)?;
        let data = attrs.iter().find(|a| a.ty == T_DATA && a.name.is_empty()).ok_or(FsError::Corrupt("$MFT has no data"))?;
        let nr = data.nr.as_ref().ok_or(FsError::Corrupt("$MFT data is resident"))?;
        *fs.mft_runs.lock().unwrap() = nr.runs.clone();
        if attrs.iter().any(|a| a.ty == T_ATTRLIST) {
            // Fragmented MFT: the full run list lives in extension records.
            let f = fs.load_file(0)?;
            let d = f.attr(T_DATA, "").and_then(|a| a.nr.as_ref()).ok_or(FsError::Corrupt("$MFT data missing"))?;
            *fs.mft_runs.lock().unwrap() = d.runs.clone();
        }
        let vol = fs.load_file(3)?;
        fs.label = vol.attr(T_VOLNAME, "").and_then(|a| a.res.as_ref()).map(|v| {
            let u: Vec<u16> = v.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        }).filter(|s| !s.is_empty());
        if let Some(v) = vol.attr(T_VOLINFO, "").and_then(|a| a.res.as_ref()).filter(|v| v.len() >= 12) {
            fs.version = (v[8], v[9]); fs.vol_flags = le16(v, 10);
        }
        fs.upcase = fs.load_upcase();
        fs.write_block = fs.check_writable();
        Ok(fs)
    }

    fn load_upcase(&self) -> Vec<u16> {
        let mut t: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
        for c in b'a'..=b'z' { t[c as usize] = c as u16 - 32; }
        let Ok(f) = self.load_file(10) else { return t };
        let Some(a) = f.attr(T_DATA, "") else { return t };
        if a.data_size() != 131072 { return t; }
        let mut raw = vec![0u8; 131072];
        if self.read_attr(a, 0, &mut raw).is_err() { return t; }
        for (i, x) in t.iter_mut().enumerate() { *x = le16(&raw, i * 2); }
        t
    }

    pub fn label_string(&self) -> Option<String> { self.label.clone() }

    // ---- MFT access ----
    pub(crate) fn mft_offset(&self, n: u64) -> Result<u64> {
        let byte = n.checked_mul(self.rec_size).ok_or(FsError::Corrupt("record number overflow"))?;
        let vcn = byte / self.cs;
        let runs = self.mft_runs.lock().unwrap();
        for r in runs.iter() {
            if vcn >= r.vcn && vcn < r.vcn + r.len {
                let l = r.lcn.ok_or(FsError::Corrupt("sparse MFT"))?;
                let off = (l + (vcn - r.vcn)) * self.cs + byte % self.cs;
                if off + self.rec_size > self.total_clusters * self.cs { return Err(FsError::Corrupt("MFT record outside volume")); }
                return Ok(off);
            }
        }
        Err(FsError::NotFound(format!("MFT record {n}")))
    }

    pub(crate) fn read_record_raw(&self, n: u64) -> Result<Vec<u8>> {
        let off = self.mft_offset(n)?;
        let mut b = vec![0u8; self.rec_size as usize];
        self.dev.read_at(off, &mut b)?;
        Ok(b)
    }

    /// Applies fixups, validates the header, returns the attributes.
    pub(crate) fn parse_record(&self, b: &mut [u8], n: u64) -> Result<Vec<Attr>> {
        if &b[0..4] != b"FILE" { return Err(FsError::Corrupt("MFT record has no FILE signature")); }
        unfixup(b, le16(b, 4) as usize, le16(b, 6) as usize)?;
        let (used, alloc, ao) = (le32(b, 24) as usize, le32(b, 28) as usize, le16(b, 20) as usize);
        if used > b.len() || alloc > b.len() || used > alloc || ao < 0x30 || ao % 8 != 0 || ao + 8 > used {
            return Err(FsError::Corrupt("bad MFT record header"));
        }
        let _ = n;
        parse_attrs(b)
    }

    pub(crate) fn load_file(&self, n: u64) -> Result<File> {
        if n >= MAX_FILE_RECORDS { return Err(FsError::Corrupt("record number out of range")); }
        let mut b = self.read_record_raw(n)?;
        let attrs = self.parse_record(&mut b, n)?;
        if le16(&b, 22) & 1 == 0 { return Err(FsError::NotFound(format!("MFT record {n} is not in use"))); }
        if le64(&b, 32) & MFT_MASK != 0 { return Err(FsError::Corrupt("extension record referenced as a file")); }
        let rec_flags = le16(&b, 22);
        let mut all: Vec<Attr> = Vec::new();
        if let Some(al) = attrs.iter().find(|a| a.ty == T_ATTRLIST) {
            let tmp = File { num: n, rec_flags, attrs: vec![al.clone()] };
            let list = self.read_all(tmp.attrs.first().unwrap(), 16 << 20)?;
            let mut seen = vec![n];
            let mut o = 0usize;
            while o + 26 <= list.len() {
                let el = le16(&list, o + 4) as usize;
                if el < 26 || o + el > list.len() { return Err(FsError::Corrupt("bad attribute list entry")); }
                let rn = le64(&list, o + 16) & MFT_MASK;
                if !seen.contains(&rn) {
                    seen.push(rn);
                    let mut eb = self.read_record_raw(rn)?;
                    let ea = self.parse_record(&mut eb, rn)?;
                    if le64(&eb, 32) & MFT_MASK != n { return Err(FsError::Corrupt("extension record has wrong base")); }
                    all.extend(ea);
                }
                o += el;
            }
            all.extend(attrs.into_iter());
        } else { all = attrs; }
        // Merge non-resident extents of the same attribute (by start VCN).
        let mut merged: Vec<Attr> = Vec::new();
        for a in all.into_iter().filter(|a| a.ty != T_ATTRLIST) {
            if let (Some(m), true) = (merged.iter_mut().find(|m| m.ty == a.ty && m.name == a.name && m.nr.is_some()), a.nr.is_some()) {
                let (mn, an) = (m.nr.as_mut().unwrap(), a.nr.unwrap());
                let first = an.runs.first().map(|r| r.vcn).unwrap_or(0);
                if first == 0 { mn.alloc = an.alloc; mn.size = an.size; mn.init = an.init; mn.comp_unit = an.comp_unit; m.flags = a.flags; }
                mn.runs.extend(an.runs);
                mn.runs.sort_by_key(|r| r.vcn);
            } else { merged.push(a); }
        }
        Ok(File { num: n, rec_flags, attrs: merged })
    }

    // ---- attribute data ----
    pub(crate) fn read_all(&self, a: &Attr, limit: u64) -> Result<Vec<u8>> {
        let sz = a.data_size();
        if sz > limit { return Err(FsError::Corrupt("attribute too large")); }
        let mut v = vec![0u8; sz as usize];
        self.read_attr(a, 0, &mut v)?;
        Ok(v)
    }

    /// Reads exactly `buf.len()` bytes at `off` (caller keeps the range within the data size).
    pub(crate) fn read_attr(&self, a: &Attr, off: u64, buf: &mut [u8]) -> Result<()> {
        if a.flags & 0x4000 != 0 { return Err(FsError::Unsupported("EFS-encrypted data")); }
        if a.flags & 0x0001 != 0 { return Err(FsError::Unsupported("NTFS-compressed data")); }
        if let Some(r) = &a.res {
            let end = off as usize + buf.len();
            if end > r.len() { return Err(FsError::Corrupt("read past resident data")); }
            buf.copy_from_slice(&r[off as usize..end]);
            return Ok(());
        }
        let nr = a.nr.as_ref().ok_or(FsError::Corrupt("empty attribute"))?;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let vcn = pos / self.cs;
            let idx = nr.runs.partition_point(|r| r.vcn + r.len <= vcn);
            let r = nr.runs.get(idx).filter(|r| r.vcn <= vcn).ok_or(FsError::Corrupt("data run missing"))?;
            let run_end = (r.vcn + r.len) * self.cs;
            let n = ((run_end - pos) as usize).min(buf.len() - done);
            match r.lcn {
                None => buf[done..done + n].fill(0),
                Some(l) => {
                    let start = (l + (vcn - r.vcn)) * self.cs + pos % self.cs;
                    if start + n as u64 > self.total_clusters * self.cs { return Err(FsError::Corrupt("data run outside volume")); }
                    self.dev.read_at(start, &mut buf[done..done + n])?;
                    // Bytes at or beyond the initialised size read as zero.
                    if pos + n as u64 > nr.init {
                        let z = nr.init.saturating_sub(pos).min(n as u64) as usize;
                        buf[done + z..done + n].fill(0);
                    }
                }
            }
            done += n;
        }
        Ok(())
    }

    // ---- directories ----
    pub(crate) fn name_eq(&self, a: &[u16], b: &[u16]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| self.upcase[x as usize] == self.upcase[y as usize])
    }

    fn index_unit(&self) -> u64 { if self.idx_size >= self.cs { self.cs } else { 512 } }

    /// All (mft ref, FILE_NAME key) pairs of a directory, in index order.
    pub(crate) fn dir_keys(&self, f: &File) -> Result<Vec<(u64, Vec<u8>)>> {
        if !f.is_dir() { return Err(FsError::NotADirectory(format!("record {}", f.num))); }
        let root = f.attr(T_INDEX_ROOT, "$I30").and_then(|a| a.res.as_ref()).ok_or(FsError::Corrupt("directory has no $I30 root"))?;
        if root.len() < 32 || le32(root, 0) != 0x30 { return Err(FsError::Corrupt("bad index root")); }
        let alloc = f.attr(T_INDEX_ALLOC, "$I30");
        let mut out = Vec::new();
        let mut budget = 1u32 << 22;
        self.walk_node(&parse_node(root, 16)?, alloc, &mut out, 0, &mut budget)?;
        Ok(out)
    }

    fn walk_node(&self, entries: &[IdxEntry], alloc: Option<&Attr>, out: &mut Vec<(u64, Vec<u8>)>, depth: u32, budget: &mut u32) -> Result<()> {
        if depth > 32 { return Err(FsError::Corrupt("index too deep")); }
        for e in entries {
            if let Some(vcn) = e.child {
                *budget = budget.checked_sub(1).ok_or(FsError::Corrupt("index walk too long"))?;
                let a = alloc.ok_or(FsError::Corrupt("index has children but no $INDEX_ALLOCATION"))?;
                let mut blk = vec![0u8; self.idx_size as usize];
                let off = vcn.checked_mul(self.index_unit()).ok_or(FsError::Corrupt("index VCN overflow"))?;
                if off + self.idx_size > a.data_size() { return Err(FsError::Corrupt("index block outside allocation")); }
                self.read_attr(a, off, &mut blk)?;
                if &blk[0..4] != b"INDX" { return Err(FsError::Corrupt("bad INDX signature")); }
                let (uo, uc) = (le16(&blk, 4) as usize, le16(&blk, 6) as usize);
                unfixup(&mut blk, uo, uc)?;
                if le64(&blk, 16) != vcn { return Err(FsError::Corrupt("INDX VCN mismatch")); }
                self.walk_node(&parse_node(&blk, 24)?, alloc, out, depth + 1, budget)?;
            }
            if e.flags & 2 == 0 { out.push((e.mref, e.key.clone())); }
        }
        Ok(())
    }

    fn entry_for(&self, mref: u64, name: &str) -> Result<Option<DirEntry>> {
        let n = mref & MFT_MASK;
        let f = match self.load_file(n) { Ok(f) => f, Err(FsError::NotFound(_)) => return Ok(None), Err(e) => return Err(e) };
        let si = f.attr(T_STDINFO, "").and_then(|a| a.res.as_ref()).filter(|v| v.len() >= 48);
        let (created, modified, flags) = match si { Some(v) => (filetime(le64(v, 0)), filetime(le64(v, 8)), le32(v, 32)), None => (None, None, 0) };
        let dir = f.is_dir();
        let size = if dir { 0 } else { f.attr(T_DATA, "").map(|a| a.data_size()).unwrap_or(0) };
        Ok(Some(DirEntry {
            name: name.to_string(), kind: if dir { EntryKind::Directory } else { EntryKind::File }, size,
            modified, created, hidden: flags & 0x2 != 0 || n < 16 && n != ROOT_REC, read_only: flags & 0x1 != 0, inode: n,
        }))
    }

    pub(crate) fn list_file(&self, dir: &File) -> Result<Vec<(String, Vec<u16>, u64)>> {
        let mut v = Vec::new();
        for (mref, key) in self.dir_keys(dir)? {
            let Some(fnm) = parse_filename(&key) else { return Err(FsError::Corrupt("bad FILE_NAME in index")) };
            if fnm.ns == 2 { continue; } // DOS-only short name; the Win32 name is listed separately
            if fnm.name == "." { continue; } // a directory's self-reference (Windows root directories carry one)
            if (mref & MFT_MASK) < 16 && (mref & MFT_MASK) != ROOT_REC { continue; } // $MFT, $Bitmap, ...
            v.push((fnm.name, fnm.units, mref & MFT_MASK));
        }
        Ok(v)
    }

    pub(crate) fn find_in(&self, dir: &File, name: &str) -> Result<Option<(String, u64)>> {
        let want: Vec<u16> = name.encode_utf16().collect();
        Ok(self.list_file(dir)?.into_iter().find(|(_, u, _)| self.name_eq(u, &want)).map(|(n, _, r)| (n, r)))
    }

    pub(crate) fn walk_dir(&self, comps: &[&str]) -> Result<File> {
        let mut cur = self.load_file(ROOT_REC)?;
        for c in comps {
            let (_, r) = self.find_in(&cur, c)?.ok_or_else(|| FsError::NotFound((*c).into()))?;
            let next = self.load_file(r)?;
            if !next.is_dir() { return Err(FsError::NotADirectory((*c).into())); }
            cur = next;
        }
        Ok(cur)
    }

    fn cached(&self, n: u64) -> Result<Arc<File>> {
        if let Some((i, f)) = &*self.cache.lock().unwrap() { if *i == n { return Ok(f.clone()); } }
        let f = Arc::new(self.load_file(n)?);
        *self.cache.lock().unwrap() = Some((n, f.clone()));
        Ok(f)
    }
    pub fn write_blocked_reason(&self) -> Option<&'static str> { self.write_block }
    pub(crate) fn invalidate(&self) { *self.cache.lock().unwrap() = None; }
}

impl Filesystem for NtfsFs {
    fn name(&self) -> &'static str { "NTFS" }
    fn label(&self) -> Option<String> { self.label.clone() }
    fn capabilities(&self) -> Capabilities {
        if self.dev.is_read_only() || self.write_block.is_some() { return Capabilities::READ_ONLY; }
        Capabilities { read: true, write: true, create: true, delete: true, rename: true, permissions: false, symbolic_links: false, extended_attributes: false }
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let d = self.walk_dir(&normalize_path(path)?)?;
        let mut out = Vec::new();
        for (name, _, r) in self.list_file(&d)? { if let Some(e) = self.entry_for(r, &name)? { out.push(e); } }
        Ok(out)
    }

    fn stat(&self, path: &str) -> Result<DirEntry> {
        let comps = normalize_path(path)?;
        let Some((last, parents)) = comps.split_last() else {
            return self.entry_for(ROOT_REC, "/")?.ok_or(FsError::Corrupt("root directory missing"));
        };
        let d = self.walk_dir(parents)?;
        let (n, r) = self.find_in(&d, last)?.ok_or_else(|| FsError::NotFound(path.into()))?;
        self.entry_for(r, &n)?.ok_or_else(|| FsError::NotFound(path.into()))
    }

    fn read_at(&self, entry: &DirEntry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if entry.kind == EntryKind::Directory { return Err(FsError::IsADirectory(entry.name.clone())); }
        let f = self.cached(entry.inode)?;
        let Some(a) = f.attr(T_DATA, "") else { return Ok(0) };
        let size = a.data_size();
        if offset >= size || buf.is_empty() { return Ok(0); }
        let n = (buf.len() as u64).min(size - offset) as usize;
        self.read_attr(a, offset, &mut buf[..n])?;
        Ok(n)
    }

    fn write_blocked(&self) -> Option<&'static str> { self.write_block }

    fn create_dir(&self, path: &str) -> Result<DirEntry> { self.w_create_dir(path) }
    fn write_file(&self, path: &str, src: &mut dyn std::io::Read, overwrite: bool) -> Result<DirEntry> { self.w_write_file(path, src, overwrite) }
    fn remove(&self, path: &str) -> Result<()> { self.w_remove(path) }
    fn rename(&self, from: &str, to: &str) -> Result<()> { self.w_rename(from, to) }
}

mod write;

// ---- Added for Ndaga Fijo: lets the staged writer copy only the clusters in use.
impl NtfsFs {
    /// (cluster size in bytes, clusters in the volume).
    pub fn geometry(&self) -> (u64, u64) { (self.cs, self.total_clusters) }
    /// Contents of `$Bitmap`: bit n set means cluster n is in use.
    pub fn cluster_bitmap(&self) -> Result<Vec<u8>> {
        let f = self.load_file(6)?;
        let a = f.attr(T_DATA, "").ok_or(FsError::Corrupt("$Bitmap has no data"))?;
        self.read_all(a, 1 << 30)
    }
}
