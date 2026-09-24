//! Minimal PE reader/writer for resource-only DLLs.
//!
//! Reading works on any PE32/PE32+ image (so `list` can inspect arbitrary
//! executables). Writing always produces a fresh x64 resource-only DLL with a
//! single `.rsrc` section; existing resources are re-emitted from the parsed
//! [`Resource`] list.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};

use crate::resid::ResId;

pub const RT_ICON: u16 = 3;
pub const RT_GROUP_ICON: u16 = 14;

const RSRC_RVA: u32 = 0x1000;
const FILE_ALIGN: u32 = 0x200;
const SECTION_ALIGN: u32 = 0x1000;
const SIZE_OF_HEADERS: u32 = 0x200;
const IMAGE_BASE: u64 = 0x1_8000_0000;

/// One leaf of the resource tree: type / name / language / raw data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub rtype: ResId,
    pub name: ResId,
    pub lang: u16,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn u16_at(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .with_context(|| format!("unexpected end of file at offset {off:#x}"))
}

fn u32_at(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .with_context(|| format!("unexpected end of file at offset {off:#x}"))
}

struct Section {
    name: [u8; 8],
    va: u32,
    vsize: u32,
    raw: u32,
    rawsize: u32,
    characteristics: u32,
}

struct Headers {
    entry_point: u32,
    rsrc_rva: u32,
    rsrc_size: u32,
    sections: Vec<Section>,
}

impl Headers {
    fn parse(image: &[u8]) -> Result<Headers> {
        ensure!(u16_at(image, 0)? == 0x5A4D, "not a PE file (missing MZ signature)");
        let pe = u32_at(image, 0x3C)? as usize;
        ensure!(u32_at(image, pe)? == 0x0000_4550, "not a PE file (missing PE signature)");
        let coff = pe + 4;
        let nsec = u16_at(image, coff + 2)? as usize;
        let opt_size = u16_at(image, coff + 16)? as usize;
        let opt = coff + 20;
        let magic = u16_at(image, opt)?;
        let (dd_off, nrva_off) = match magic {
            0x10B => (96, 92),   // PE32
            0x20B => (112, 108), // PE32+
            _ => bail!("unknown optional header magic {magic:#x}"),
        };
        let entry_point = u32_at(image, opt + 16)?;
        let nrva = u32_at(image, opt + nrva_off)?;
        let (rsrc_rva, rsrc_size) = if nrva >= 3 {
            (u32_at(image, opt + dd_off + 16)?, u32_at(image, opt + dd_off + 20)?)
        } else {
            (0, 0)
        };
        let table = opt + opt_size;
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let s = table + i * 40;
            let mut name = [0u8; 8];
            name.copy_from_slice(image.get(s..s + 8).context("truncated section table")?);
            sections.push(Section {
                name,
                vsize: u32_at(image, s + 8)?,
                va: u32_at(image, s + 12)?,
                rawsize: u32_at(image, s + 16)?,
                raw: u32_at(image, s + 20)?,
                characteristics: u32_at(image, s + 36)?,
            });
        }
        Ok(Headers { entry_point, rsrc_rva, rsrc_size, sections })
    }

    fn rva_to_offset(&self, rva: u32) -> Result<usize> {
        for s in &self.sections {
            let span = s.vsize.max(s.rawsize);
            if rva >= s.va && rva < s.va.saturating_add(span) {
                return Ok((s.raw + (rva - s.va)) as usize);
            }
        }
        bail!("RVA {rva:#x} does not belong to any section")
    }
}

/// Fails unless the image is a resource-only DLL (no entry point, no code,
/// nothing but a `.rsrc` section). Rebuilding anything else would silently
/// throw away its code.
pub fn check_resource_only(image: &[u8]) -> Result<()> {
    let h = Headers::parse(image)?;
    let only_rsrc = h.sections.len() == 1 && &h.sections[0].name == b".rsrc\0\0\0";
    let has_code = h.sections.iter().any(|s| s.characteristics & 0x20 != 0);
    ensure!(
        h.entry_point == 0 && only_rsrc && !has_code,
        "this file is not a resource-only DLL; refusing to rewrite it \
         (only DLLs created by this tool can be modified)"
    );
    Ok(())
}

/// Parses every resource in the image.
pub fn read_resources(image: &[u8]) -> Result<Vec<Resource>> {
    let h = Headers::parse(image)?;
    if h.rsrc_rva == 0 || h.rsrc_size == 0 {
        return Ok(Vec::new());
    }
    let base = h.rva_to_offset(h.rsrc_rva)?;
    ensure!(base < image.len(), "resource section lies outside the file");
    let mut out = Vec::new();
    walk(image, &h, base, 0, 0, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn read_dir_string(image: &[u8], at: usize) -> Result<String> {
    let len = u16_at(image, at)? as usize;
    let mut units = Vec::with_capacity(len);
    for i in 0..len {
        units.push(u16_at(image, at + 2 + i * 2)?);
    }
    Ok(String::from_utf16_lossy(&units))
}

fn walk(
    image: &[u8],
    h: &Headers,
    base: usize,
    dir: usize,
    level: usize,
    path: &mut Vec<ResId>,
    out: &mut Vec<Resource>,
) -> Result<()> {
    ensure!(level <= 2, "resource directory nested too deeply");
    let d = base + dir;
    let named = u16_at(image, d + 12)? as usize;
    let ids = u16_at(image, d + 14)? as usize;
    for i in 0..named + ids {
        let e = d + 16 + i * 8;
        let name = u32_at(image, e)?;
        let off = u32_at(image, e + 4)?;
        let id = if name & 0x8000_0000 != 0 {
            ResId::Name(read_dir_string(image, base + (name & 0x7FFF_FFFF) as usize)?)
        } else {
            ResId::Id(name as u16)
        };
        path.push(id);
        if off & 0x8000_0000 != 0 {
            walk(image, h, base, (off & 0x7FFF_FFFF) as usize, level + 1, path, out)?;
        } else {
            ensure!(level == 2, "resource data entry found above the language level");
            let de = base + off as usize;
            let rva = u32_at(image, de)?;
            let size = u32_at(image, de + 4)? as usize;
            let start = h.rva_to_offset(rva)?;
            let data = image
                .get(start..start + size)
                .context("resource data lies outside the file")?
                .to_vec();
            let lang = match &path[2] {
                ResId::Id(l) => *l,
                ResId::Name(n) => bail!("unexpected named language entry `{n}`"),
            };
            out.push(Resource {
                rtype: path[0].clone(),
                name: path[1].clone(),
                lang,
                data,
            });
        }
        path.pop();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn align(v: usize, a: usize) -> usize {
    v.div_ceil(a) * a
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

type Tree<'a> = BTreeMap<&'a ResId, BTreeMap<&'a ResId, BTreeMap<u16, &'a [u8]>>>;

/// Builds the raw contents of a `.rsrc` section whose RVA will be `section_rva`.
///
/// Layout: directory tables, name strings, data entries, then the data blobs
/// (8-byte aligned). Entries within each directory come out sorted because the
/// tree is keyed by [`ResId`], whose ordering matches what the Windows loader
/// expects (named entries first, then IDs ascending).
fn build_rsrc(resources: &[Resource], section_rva: u32) -> Result<Vec<u8>> {
    let mut tree: Tree = BTreeMap::new();
    for r in resources {
        let prev = tree
            .entry(&r.rtype)
            .or_default()
            .entry(&r.name)
            .or_default()
            .insert(r.lang, &r.data);
        ensure!(
            prev.is_none(),
            "duplicate resource type={} name={} lang={:#06x}",
            r.rtype,
            r.name,
            r.lang
        );
    }

    // Pass 1: lay out directories, strings, data entries and data blobs.
    let mut off = 16 + 8 * tree.len();
    let mut type_dirs = Vec::new();
    for names in tree.values() {
        type_dirs.push(off);
        off += 16 + 8 * names.len();
    }
    let mut name_dirs = Vec::new();
    for names in tree.values() {
        let mut v = Vec::new();
        for langs in names.values() {
            v.push(off);
            off += 16 + 8 * langs.len();
        }
        name_dirs.push(v);
    }
    let mut strings: BTreeMap<&ResId, usize> = BTreeMap::new();
    for (t, names) in &tree {
        for id in std::iter::once(t).chain(names.keys()) {
            if let ResId::Name(s) = id {
                strings.entry(id).or_insert_with(|| {
                    let at = off;
                    off += 2 + 2 * s.encode_utf16().count();
                    at
                });
            }
        }
    }
    off = align(off, 4);
    let leaves: Vec<&[u8]> = tree
        .values()
        .flat_map(|n| n.values())
        .flat_map(|l| l.values().copied())
        .collect();
    let mut data_entries = Vec::with_capacity(leaves.len());
    for _ in &leaves {
        data_entries.push(off);
        off += 16;
    }
    off = align(off, 8);
    let mut blobs = Vec::with_capacity(leaves.len());
    for data in &leaves {
        blobs.push(off);
        off = align(off + data.len(), 8);
    }
    let mut buf = vec![0u8; off];

    // Pass 2: emit.
    let entry_name = |id: &ResId| -> u32 {
        match id {
            ResId::Name(_) => 0x8000_0000 | strings[id] as u32,
            ResId::Id(n) => *n as u32,
        }
    };
    let dir_header = |buf: &mut [u8], at: usize, named: usize, ids: usize| {
        put_u16(buf, at + 12, named as u16);
        put_u16(buf, at + 14, ids as u16);
    };
    let count = |keys: &mut dyn Iterator<Item = &ResId>| -> (usize, usize) {
        keys.fold((0, 0), |(n, i), k| match k {
            ResId::Name(_) => (n + 1, i),
            ResId::Id(_) => (n, i + 1),
        })
    };

    let (n, i) = count(&mut tree.keys().copied());
    dir_header(&mut buf, 0, n, i);
    let mut k = 0;
    for (ti, (t, names)) in tree.iter().enumerate() {
        let e = 16 + ti * 8;
        put_u32(&mut buf, e, entry_name(t));
        put_u32(&mut buf, e + 4, 0x8000_0000 | type_dirs[ti] as u32);
        let (n, i) = count(&mut names.keys().copied());
        dir_header(&mut buf, type_dirs[ti], n, i);
        for (ni, (name, langs)) in names.iter().enumerate() {
            let e = type_dirs[ti] + 16 + ni * 8;
            put_u32(&mut buf, e, entry_name(name));
            put_u32(&mut buf, e + 4, 0x8000_0000 | name_dirs[ti][ni] as u32);
            dir_header(&mut buf, name_dirs[ti][ni], 0, langs.len());
            for (li, (lang, data)) in langs.iter().enumerate() {
                let e = name_dirs[ti][ni] + 16 + li * 8;
                put_u32(&mut buf, e, *lang as u32);
                put_u32(&mut buf, e + 4, data_entries[k] as u32);
                put_u32(&mut buf, data_entries[k], section_rva + blobs[k] as u32);
                put_u32(&mut buf, data_entries[k] + 4, data.len() as u32);
                buf[blobs[k]..blobs[k] + data.len()].copy_from_slice(data);
                k += 1;
            }
        }
    }
    for (id, at) in &strings {
        if let ResId::Name(s) = id {
            let units: Vec<u16> = s.encode_utf16().collect();
            put_u16(&mut buf, *at, units.len() as u16);
            for (i, u) in units.iter().enumerate() {
                put_u16(&mut buf, at + 2 + i * 2, *u);
            }
        }
    }
    Ok(buf)
}

struct Buf(Vec<u8>);

impl Buf {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn pad_to(&mut self, len: usize) {
        assert!(self.0.len() <= len, "buffer overran {len:#x}");
        self.0.resize(len, 0);
    }
}

/// Standard PE checksum (as computed by `MapFileAndCheckSum`).
fn pe_checksum(data: &[u8], checksum_off: usize) -> u32 {
    let mut sum: u32 = 0;
    for (i, chunk) in data.chunks(2).enumerate() {
        let off = i * 2;
        if off == checksum_off || off == checksum_off + 2 {
            continue;
        }
        let word = if chunk.len() == 2 {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            chunk[0] as u16
        };
        sum += word as u32;
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum = (sum & 0xFFFF) + (sum >> 16);
    sum.wrapping_add(data.len() as u32)
}

/// Builds a complete x64 resource-only DLL image containing `resources`.
pub fn build_dll(resources: &[Resource]) -> Result<Vec<u8>> {
    let rsrc = build_rsrc(resources, RSRC_RVA)?;
    ensure!(rsrc.len() < u32::MAX as usize, "resources too large");
    let rsrc_raw = align(rsrc.len(), FILE_ALIGN as usize) as u32;
    let size_of_image = RSRC_RVA + align(rsrc.len(), SECTION_ALIGN as usize) as u32;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let mut b = Buf(Vec::with_capacity(SIZE_OF_HEADERS as usize + rsrc_raw as usize));
    // IMAGE_DOS_HEADER (no DOS stub; e_lfanew points right after it)
    b.bytes(b"MZ");
    b.pad_to(0x3C);
    b.u32(0x40);
    b.bytes(b"PE\0\0");
    // IMAGE_FILE_HEADER
    b.u16(0x8664); // AMD64
    b.u16(1); // NumberOfSections
    b.u32(timestamp);
    b.u32(0); // PointerToSymbolTable
    b.u32(0); // NumberOfSymbols
    b.u16(240); // SizeOfOptionalHeader (PE32+)
    b.u16(0x2022); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE | DLL
    // IMAGE_OPTIONAL_HEADER64
    let opt = b.0.len();
    b.u16(0x20B);
    b.u8(14); // MajorLinkerVersion
    b.u8(0);
    b.u32(0); // SizeOfCode
    b.u32(rsrc_raw); // SizeOfInitializedData
    b.u32(0); // SizeOfUninitializedData
    b.u32(0); // AddressOfEntryPoint (none: resource-only)
    b.u32(0); // BaseOfCode
    b.u64(IMAGE_BASE);
    b.u32(SECTION_ALIGN);
    b.u32(FILE_ALIGN);
    b.u16(6); // MajorOperatingSystemVersion
    b.u16(0);
    b.u16(0); // MajorImageVersion
    b.u16(0);
    b.u16(6); // MajorSubsystemVersion
    b.u16(0);
    b.u32(0); // Win32VersionValue
    b.u32(size_of_image);
    b.u32(SIZE_OF_HEADERS);
    let checksum_off = b.0.len();
    b.u32(0); // CheckSum (patched below)
    b.u16(2); // IMAGE_SUBSYSTEM_WINDOWS_GUI
    b.u16(0x0160); // HIGH_ENTROPY_VA | DYNAMIC_BASE | NX_COMPAT
    b.u64(0x10_0000); // SizeOfStackReserve
    b.u64(0x1000); // SizeOfStackCommit
    b.u64(0x10_0000); // SizeOfHeapReserve
    b.u64(0x1000); // SizeOfHeapCommit
    b.u32(0); // LoaderFlags
    b.u32(16); // NumberOfRvaAndSizes
    for i in 0..16 {
        if i == 2 {
            b.u32(RSRC_RVA);
            b.u32(rsrc.len() as u32);
        } else {
            b.u64(0);
        }
    }
    debug_assert_eq!(b.0.len() - opt, 240);
    // IMAGE_SECTION_HEADER for .rsrc
    b.bytes(b".rsrc\0\0\0");
    b.u32(rsrc.len() as u32); // VirtualSize
    b.u32(RSRC_RVA);
    b.u32(rsrc_raw); // SizeOfRawData
    b.u32(SIZE_OF_HEADERS); // PointerToRawData
    b.u32(0); // PointerToRelocations
    b.u32(0); // PointerToLinenumbers
    b.u16(0); // NumberOfRelocations
    b.u16(0); // NumberOfLinenumbers
    b.u32(0x4000_0040); // CNT_INITIALIZED_DATA | MEM_READ
    b.pad_to(SIZE_OF_HEADERS as usize);
    b.bytes(&rsrc);
    b.pad_to(SIZE_OF_HEADERS as usize + rsrc_raw as usize);

    let mut image = b.0;
    let checksum = pe_checksum(&image, checksum_off);
    put_u32(&mut image, checksum_off, checksum);
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Resource> {
        let id = |t: u16, n: ResId, lang: u16, data: &[u8]| Resource {
            rtype: ResId::Id(t),
            name: n,
            lang,
            data: data.to_vec(),
        };
        vec![
            id(RT_ICON, ResId::Id(1), 0x409, b"icon-one"),
            id(RT_ICON, ResId::Id(2), 0x409, b"icon-two-with-more-bytes"),
            id(RT_ICON, ResId::Id(2), 0x411, b"icon-two-ja"),
            id(RT_GROUP_ICON, ResId::Name("MAIN".into()), 0x409, b"group"),
            id(RT_GROUP_ICON, ResId::Id(100), 0x409, b"group100"),
            Resource {
                rtype: ResId::Name("CUSTOM".into()),
                name: ResId::Name("Data".into()),
                lang: 0,
                data: vec![],
            },
        ]
    }

    #[test]
    fn round_trip() {
        let input = sample();
        let image = build_dll(&input).unwrap();
        check_resource_only(&image).unwrap();
        let mut got = read_resources(&image).unwrap();
        let mut want = input;
        let key = |r: &Resource| (r.rtype.clone(), r.name.clone(), r.lang);
        got.sort_by_key(key);
        want.sort_by_key(key);
        assert_eq!(got, want);
    }

    #[test]
    fn empty_dll() {
        let image = build_dll(&[]).unwrap();
        assert!(read_resources(&image).unwrap().is_empty());
        assert_eq!(image.len() % FILE_ALIGN as usize, 0);
    }

    #[test]
    fn duplicate_resource_rejected() {
        let mut v = sample();
        v.push(v[0].clone());
        assert!(build_dll(&v).is_err());
    }

    #[test]
    fn checksum_verifies() {
        let image = build_dll(&sample()).unwrap();
        let opt = 0x40 + 4 + 20;
        let stored = u32_at(&image, opt + 64).unwrap();
        assert_eq!(stored, pe_checksum(&image, opt + 64));
    }
}
