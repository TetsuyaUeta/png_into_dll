//! Icon image handling: PNG/ICO input, DIB conversion, and the
//! RT_GROUP_ICON / .ico directory formats.

use std::fs;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::resid::ResId;

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
const ICO_MAGIC: [u8; 4] = [0, 0, 1, 0];
const BITMAPINFOHEADER_SIZE: u32 = 40;

/// A single icon image as stored in an RT_ICON resource (or .ico entry):
/// either a DIB (BITMAPINFOHEADER + pixels + mask) or a raw PNG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    pub color_count: u8,
    pub planes: u16,
    pub bit_count: u16,
    pub data: Vec<u8>,
}

impl IconImage {
    pub fn is_png(&self) -> bool {
        self.data.starts_with(PNG_SIG)
    }

    /// Short human-readable description, e.g. `32x32 32bpp` or `256x256 PNG`.
    pub fn describe(&self) -> String {
        if self.is_png() {
            format!("{}x{} PNG", self.width, self.height)
        } else {
            format!("{}x{} {}bpp", self.width, self.height, self.bit_count)
        }
    }

    /// Key used to order images inside a group (smallest first).
    pub fn sort_key(&self) -> (u32, u32, u16) {
        (self.width, self.height, self.bit_count)
    }

    /// Directory entries store 256 as 0 in their single-byte size fields.
    fn dir_byte(v: u32) -> u8 {
        if v >= 256 { 0 } else { v as u8 }
    }
}

/// An icon (RT_GROUP_ICON) made of one or more images of different sizes.
#[derive(Clone, Debug)]
pub struct IconGroup {
    pub id: ResId,
    pub lang: u16,
    pub images: Vec<IconImage>,
}

impl IconGroup {
    pub fn describe(&self) -> String {
        self.images
            .iter()
            .map(IconImage::describe)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// Input files
// ---------------------------------------------------------------------------

/// Loads a PNG (one image) or ICO (all of its images) file. The format is
/// detected from the file contents, not the extension.
pub fn load_image_file(path: &Path, keep_png: bool) -> Result<Vec<IconImage>> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let result = if bytes.starts_with(PNG_SIG) {
        png_to_icon(&bytes, keep_png).map(|img| vec![img])
    } else if bytes.starts_with(&ICO_MAGIC) {
        parse_ico(&bytes)
    } else {
        Err(anyhow::anyhow!("unsupported file format (expected PNG or ICO)"))
    };
    result.with_context(|| format!("{}", path.display()))
}

fn png_dims(png: &[u8]) -> Result<(u32, u32)> {
    ensure!(
        png.len() >= 24 && png.starts_with(PNG_SIG) && &png[12..16] == b"IHDR",
        "malformed PNG header"
    );
    let be = |o: usize| u32::from_be_bytes([png[o], png[o + 1], png[o + 2], png[o + 3]]);
    Ok((be(16), be(20)))
}

fn png_to_icon(png: &[u8], keep_png: bool) -> Result<IconImage> {
    let (width, height) = png_dims(png)?;
    ensure!(
        (1..=256).contains(&width) && (1..=256).contains(&height),
        "image is {width}x{height}; icon images must be between 1x1 and 256x256"
    );
    // Windows (Vista+) only reliably renders PNG-compressed icon images at
    // 256px; smaller sizes are converted to 32-bit DIBs unless asked not to.
    let data = if keep_png || width == 256 || height == 256 {
        png.to_vec()
    } else {
        let rgba = decode_png_rgba(png)?;
        rgba_to_dib(width, height, &rgba)
    };
    Ok(IconImage { width, height, color_count: 0, planes: 1, bit_count: 32, data })
}

/// Decodes any PNG into tightly packed 8-bit RGBA.
fn decode_png_rgba(png: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("invalid PNG")?;
    let mut buf = vec![0u8; reader.output_buffer_size().context("PNG too large")?];
    let info = reader.next_frame(&mut buf).context("PNG decode failed")?;
    ensure!(info.bit_depth == png::BitDepth::Eight, "unexpected PNG bit depth after expansion");
    let pixels = (info.width * info.height) as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    for row in buf[..info.buffer_size()].chunks_exact(info.line_size) {
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(&row[..info.width as usize * 4]),
            png::ColorType::Rgb => {
                for p in row[..info.width as usize * 3].chunks_exact(3) {
                    rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for p in row[..info.width as usize * 2].chunks_exact(2) {
                    rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in &row[..info.width as usize] {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::Indexed => bail!("palette PNG was not expanded"),
        }
    }
    Ok(rgba)
}

/// Encodes RGBA pixels as a 32bpp icon DIB: BITMAPINFOHEADER (height doubled),
/// bottom-up BGRA XOR bitmap, then a 1bpp bottom-up AND mask.
pub fn rgba_to_dib(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    assert_eq!(rgba.len(), w * h * 4);
    let xor_size = w * h * 4;
    let mask_stride = w.div_ceil(32) * 4;
    let and_size = mask_stride * h;
    let mut out = Vec::with_capacity(BITMAPINFOHEADER_SIZE as usize + xor_size + and_size);
    out.extend_from_slice(&BITMAPINFOHEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&((height * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((xor_size + and_size) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // ppm x/y, clrUsed, clrImportant
    for y in (0..h).rev() {
        for p in rgba[y * w * 4..(y + 1) * w * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    for y in (0..h).rev() {
        let mut row = vec![0u8; mask_stride];
        for x in 0..w {
            if rgba[(y * w + x) * 4 + 3] == 0 {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    out
}

// ---------------------------------------------------------------------------
// ICO / GRPICONDIR
// ---------------------------------------------------------------------------

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Determines the real pixel size from the image data (PNG IHDR or
/// BITMAPINFOHEADER), since directory entries cap out at 255.
fn detect_dims(data: &[u8]) -> Option<(u32, u32)> {
    if data.starts_with(PNG_SIG) {
        png_dims(data).ok()
    } else if data.len() >= 12 && u32_at(data, 0) == BITMAPINFOHEADER_SIZE {
        let w = (u32_at(data, 4) as i32).unsigned_abs();
        let h = (u32_at(data, 8) as i32).unsigned_abs() / 2;
        Some((w, h))
    } else {
        None
    }
}

/// Shared shape of ICONDIRENTRY / GRPICONDIRENTRY minus the trailing field.
struct DirEntry {
    width: u8,
    height: u8,
    color_count: u8,
    planes: u16,
    bit_count: u16,
    size: u32,
}

fn parse_dir(bytes: &[u8], entry_len: usize) -> Result<Vec<(DirEntry, u32)>> {
    ensure!(bytes.len() >= 6, "icon directory truncated");
    ensure!(bytes[..4] == ICO_MAGIC, "not an icon directory");
    let count = u16_at(bytes, 4) as usize;
    ensure!(bytes.len() >= 6 + count * entry_len, "icon directory truncated");
    Ok((0..count)
        .map(|i| {
            let e = 6 + i * entry_len;
            let entry = DirEntry {
                width: bytes[e],
                height: bytes[e + 1],
                color_count: bytes[e + 2],
                planes: u16_at(bytes, e + 4),
                bit_count: u16_at(bytes, e + 6),
                size: u32_at(bytes, e + 8),
            };
            // trailing field: u32 offset (ICO) or u16 resource id (group)
            let tail = if entry_len == 16 { u32_at(bytes, e + 12) } else { u16_at(bytes, e + 12) as u32 };
            (entry, tail)
        })
        .collect())
}

fn make_image(entry: &DirEntry, data: &[u8]) -> IconImage {
    let fallback = |v: u8| if v == 0 { 256 } else { v as u32 };
    let (width, height) =
        detect_dims(data).unwrap_or((fallback(entry.width), fallback(entry.height)));
    IconImage {
        width,
        height,
        color_count: entry.color_count,
        planes: entry.planes,
        bit_count: entry.bit_count,
        data: data.to_vec(),
    }
}

/// Parses a .ico file into its images.
pub fn parse_ico(bytes: &[u8]) -> Result<Vec<IconImage>> {
    let entries = parse_dir(bytes, 16)?;
    ensure!(!entries.is_empty(), "ICO file contains no images");
    entries
        .iter()
        .map(|(entry, offset)| {
            let start = *offset as usize;
            let data = bytes
                .get(start..start + entry.size as usize)
                .context("ICO image data lies outside the file")?;
            Ok(make_image(entry, data))
        })
        .collect()
}

/// Serializes images as a .ico file.
pub fn to_ico(images: &[IconImage]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ICO_MAGIC);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for img in images {
        write_entry(&mut out, img);
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.data.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img.data);
    }
    out
}

/// Builds the RT_GROUP_ICON payload; image `i` refers to RT_ICON `first_id + i`.
pub fn build_group_dir(images: &[IconImage], first_id: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ICO_MAGIC);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        write_entry(&mut out, img);
        out.extend_from_slice(&(first_id + i as u16).to_le_bytes());
    }
    out
}

fn write_entry(out: &mut Vec<u8>, img: &IconImage) {
    out.push(IconImage::dir_byte(img.width));
    out.push(IconImage::dir_byte(img.height));
    out.push(img.color_count);
    out.push(0);
    out.extend_from_slice(&img.planes.to_le_bytes());
    out.extend_from_slice(&img.bit_count.to_le_bytes());
    out.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
}

/// Parses an RT_GROUP_ICON payload, resolving each entry's RT_ICON data via
/// `lookup(id)`. Entries whose icon is missing are skipped.
pub fn parse_group_dir(
    bytes: &[u8],
    mut lookup: impl FnMut(u16) -> Option<Vec<u8>>,
) -> Result<Vec<IconImage>> {
    Ok(parse_dir(bytes, 14)?
        .iter()
        .filter_map(|(entry, id)| lookup(*id as u16).map(|data| make_image(entry, &data)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let a = if (i % w + i / w).is_multiple_of(2) { 255 } else { 0 };
                [10, 20, 30, a]
            })
            .collect()
    }

    #[test]
    fn dib_layout() {
        let dib = rgba_to_dib(10, 3, &checker(10, 3));
        // header + 10*3*4 XOR + 3 rows * 4-byte mask stride
        assert_eq!(dib.len(), 40 + 120 + 12);
        assert_eq!(u32_at(&dib, 4), 10);
        assert_eq!(u32_at(&dib, 8), 6);
        assert_eq!(u16_at(&dib, 14), 32);
        // first XOR row is the bottom image row (y=2): pixel (0,2) opaque, BGR order
        assert_eq!(&dib[40..44], &[30, 20, 10, 255]);
        // AND mask: bottom row (y=2) starts opaque, so bits 0101010101 -> 0x55, 0x40
        assert_eq!(&dib[160..164], &[0x55, 0x40, 0, 0]);
        assert_eq!(detect_dims(&dib), Some((10, 3)));
    }

    #[test]
    fn ico_round_trip() {
        let a = IconImage {
            width: 10,
            height: 3,
            color_count: 0,
            planes: 1,
            bit_count: 32,
            data: rgba_to_dib(10, 3, &checker(10, 3)),
        };
        let mut b = a.clone();
        b.width = 256;
        b.height = 256;
        b.data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x01\0\0\0\x01\0rest".to_vec();
        let ico = to_ico(&[a.clone(), b.clone()]);
        assert_eq!(parse_ico(&ico).unwrap(), vec![a, b]);
    }

    #[test]
    fn group_dir_round_trip() {
        let img = IconImage {
            width: 16,
            height: 16,
            color_count: 0,
            planes: 1,
            bit_count: 32,
            data: rgba_to_dib(16, 16, &checker(16, 16)),
        };
        let dir = build_group_dir(&[img.clone(), img.clone()], 7);
        assert_eq!(dir.len(), 6 + 14 * 2);
        let got = parse_group_dir(&dir, |id| (id == 8).then(|| img.data.clone())).unwrap();
        assert_eq!(got, vec![img]);
    }

    #[test]
    fn png_decode_all_color_types() {
        for ct in [
            png::ColorType::Grayscale,
            png::ColorType::GrayscaleAlpha,
            png::ColorType::Rgb,
            png::ColorType::Rgba,
        ] {
            let mut out = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut out, 4, 2);
                enc.set_color(ct);
                enc.set_depth(png::BitDepth::Eight);
                let mut w = enc.write_header().unwrap();
                let n = ct.samples() * 8;
                w.write_image_data(&vec![200u8; n]).unwrap();
            }
            let img = png_to_icon(&out, false).unwrap();
            assert_eq!((img.width, img.height), (4, 2));
            assert!(!img.is_png());
            assert_eq!(img.data.len(), 40 + 4 * 2 * 4 + 4 * 2);
            let img = png_to_icon(&out, true).unwrap();
            assert!(img.is_png());
        }
    }
}
