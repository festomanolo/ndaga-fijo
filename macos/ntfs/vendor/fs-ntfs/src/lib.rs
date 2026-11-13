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
