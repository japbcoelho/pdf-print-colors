//! Image passes: 16-bit images to 8 bits (Ghostscript drops 16-bit images with a soft mask when
//! it converts colour), and greyscale images to black ink only (DeviceCMYK with C = M = Y = 0).

use anyhow::Result;
use lopdf::{Document, Object, ObjectId, StringFormat};

use crate::util::{Family, Masks, decodable, family, num, resolve, zlib};

#[derive(Default, Debug)]
pub struct ImageStats {
    pub converted: usize,
    pub indexed: usize,
    /// greyscale images under a filter this tool can't undo (DCT, JPX, CCITT, JBIG2)
    pub left_gray: usize,
}

fn is_image(s: &lopdf::Stream) -> bool {
    crate::util::name_is(&s.dict, b"Subtype", b"Image")
}

fn int(d: &lopdf::Dictionary, key: &[u8]) -> Option<usize> {
    d.get(key)
        .ok()
        .and_then(num)
        .filter(|v| *v >= 0.0)
        .map(|v| v as usize)
}

/// 16-bit images to 8 bits: the high byte of each sample. Returns how many were changed.
pub fn sixteen_to_eight(doc: &mut Document) -> Result<usize> {
    let ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(id, o)| {
            let s = o.as_stream().ok()?;
            (is_image(s) && int(&s.dict, b"BitsPerComponent") == Some(16) && decodable(s))
                .then_some(*id)
        })
        .collect();
    let mut n = 0;
    for id in ids {
        let s = doc.get_object_mut(id)?.as_stream_mut()?;
        let Ok(data) = s.decompressed_content() else {
            continue;
        };
        let eight: Vec<u8> = data.as_chunks::<2>().0.iter().map(|p| p[0]).collect();
        s.dict.set("BitsPerComponent", 8);
        s.set_plain_content(zlib(&eight)?);
        s.dict.set("Filter", "FlateDecode");
        n += 1;
    }
    Ok(n)
}

/// Greyscale samples to K-only CMYK samples at the same bit depth: each pixel becomes
/// `0 0 0 k` with `k = 1 - grey` (after the image's `/Decode`), rows padded to whole bytes.
pub fn gray_samples_to_k(
    data: &[u8],
    width: usize,
    height: usize,
    bpc: usize,
    decode: (f32, f32),
) -> Vec<u8> {
    let max = (1usize << bpc) - 1;
    let lut: Vec<u32> = (0..=max)
        .map(|s| {
            let v = decode.0 + s as f32 * (decode.1 - decode.0) / max as f32;
            ((1.0 - v).clamp(0.0, 1.0) * max as f32).round() as u32
        })
        .collect();
    let in_row = (width * bpc).div_ceil(8);
    let out_row = (width * 4 * bpc).div_ceil(8);
    let mut out = vec![0u8; out_row * height];
    if bpc == 8 {
        for y in 0..height {
            let row = data.get(y * in_row..(y + 1) * in_row).unwrap_or(&[]);
            for (x, s) in row.iter().enumerate() {
                out[y * out_row + 4 * x + 3] = lut[*s as usize] as u8;
            }
        }
        return out;
    }
    for y in 0..height {
        let row = data.get(y * in_row..(y + 1) * in_row).unwrap_or(&[]);
        let o = &mut out[y * out_row..(y + 1) * out_row];
        for x in 0..width {
            let bit = x * bpc;
            let s = read_bits(row, bit, bpc);
            write_bits(o, (4 * x + 3) * bpc, bpc, lut[s as usize]);
        }
    }
    out
}

fn read_bits(row: &[u8], bit: usize, n: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..n {
        let b = bit + i;
        let byte = row.get(b / 8).copied().unwrap_or(0);
        v = (v << 1) | ((byte >> (7 - b % 8)) & 1) as u32;
    }
    v
}

fn write_bits(row: &mut [u8], bit: usize, n: usize, v: u32) {
    for i in 0..n {
        let b = bit + i;
        if (v >> (n - 1 - i)) & 1 == 1 {
            row[b / 8] |= 1 << (7 - b % 8);
        }
    }
}

/// PNG row filters (the PDF `/Predictor 15` form): each row gets the filter, of None, Sub, Up
/// and Paeth, with the smallest sum of absolute values, and a leading byte naming it. Lossless;
/// with a K-only image the zero C, M and Y bytes stay zero, so Flate packs it much tighter.
fn png_filter(data: &[u8], row: usize, bpp: usize) -> Vec<u8> {
    let rows = data.len().checked_div(row).unwrap_or(0);
    let mut out = Vec::with_capacity(data.len() + rows);
    let zero = vec![0u8; row];
    let mut cand = [
        vec![0u8; row],
        vec![0u8; row],
        vec![0u8; row],
        vec![0u8; row],
    ];
    for y in 0..rows {
        let cur = &data[y * row..(y + 1) * row];
        let up = if y == 0 {
            &zero[..]
        } else {
            &data[(y - 1) * row..y * row]
        };
        for i in 0..row {
            let a = if i >= bpp { cur[i - bpp] } else { 0 };
            let b = up[i];
            let c = if i >= bpp { up[i - bpp] } else { 0 };
            let (pa, pb, pc) = (
                (b as i16 - c as i16).abs(),
                (a as i16 - c as i16).abs(),
                (a as i16 + b as i16 - 2 * c as i16).abs(),
            );
            let paeth = if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            };
            cand[0][i] = cur[i];
            cand[1][i] = cur[i].wrapping_sub(a);
            cand[2][i] = cur[i].wrapping_sub(b);
            cand[3][i] = cur[i].wrapping_sub(paeth);
        }
        let cost = |v: &Vec<u8>| {
            v.iter()
                .map(|x| (*x as i8).unsigned_abs() as u64)
                .sum::<u64>()
        };
        let best = (0..4).min_by_key(|k| cost(&cand[*k])).unwrap_or(0);
        out.push([0u8, 1, 2, 4][best]);
        out.extend_from_slice(&cand[best]);
    }
    out
}
/// A grey palette (one byte per entry, 0 = black) as a K-only CMYK palette.
fn k_palette(gray: &[u8]) -> Vec<u8> {
    gray.iter().flat_map(|g| [0, 0, 0, 255 - g]).collect()
}

/// Every greyscale image (not the masks) to K-only DeviceCMYK; indexed images with a grey palette
/// get a K-only palette.
pub fn gray_images_to_k(doc: &mut Document, masks: &Masks) -> Result<ImageStats> {
    let mut st = ImageStats::default();
    enum Job {
        Samples(ObjectId),
        Palette(ObjectId, Object),
    }
    let mut jobs = Vec::new();
    for (id, o) in &doc.objects {
        let Ok(s) = o.as_stream() else { continue };
        if !is_image(s) || masks.images.contains(id) {
            continue;
        }
        if s.dict
            .get(b"ImageMask")
            .and_then(Object::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(cs) = s.dict.get(b"ColorSpace") else {
            continue;
        };
        match family(doc, cs) {
            Family::Gray => {
                if decodable(s) {
                    jobs.push(Job::Samples(*id));
                } else {
                    st.left_gray += 1;
                }
            }
            Family::Indexed => {
                let Ok(a) = resolve(doc, cs).as_array() else {
                    continue;
                };
                if a.len() != 4 || family(doc, &a[1]) != Family::Gray {
                    continue;
                }
                let lookup = match resolve(doc, &a[3]) {
                    Object::String(b, _) => Some(b.clone()),
                    Object::Stream(ls) => ls.decompressed_content().ok(),
                    _ => None,
                };
                if let Some(lookup) = lookup {
                    let new = Object::Array(vec![
                        Object::Name(b"Indexed".to_vec()),
                        Object::Name(b"DeviceCMYK".to_vec()),
                        a[2].clone(),
                        Object::String(k_palette(&lookup), StringFormat::Hexadecimal),
                    ]);
                    jobs.push(Job::Palette(*id, new));
                }
            }
            _ => {}
        }
    }
    for job in jobs {
        match job {
            Job::Palette(id, cs) => {
                doc.get_object_mut(id)?
                    .as_stream_mut()?
                    .dict
                    .set("ColorSpace", cs);
                st.indexed += 1;
            }
            Job::Samples(id) => {
                let s = doc.get_object_mut(id)?.as_stream_mut()?;
                let (Some(w), Some(h)) = (int(&s.dict, b"Width"), int(&s.dict, b"Height")) else {
                    continue;
                };
                let bpc = int(&s.dict, b"BitsPerComponent").unwrap_or(8);
                if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
                    st.left_gray += 1;
                    continue;
                }
                let decode = s
                    .dict
                    .get(b"Decode")
                    .ok()
                    .and_then(|d| d.as_array().ok())
                    .and_then(|a| Some((num(a.first()?)?, num(a.get(1)?)?)))
                    .unwrap_or((0.0, 1.0));
                let Ok(data) = s.decompressed_content() else {
                    st.left_gray += 1;
                    continue;
                };
                let out = gray_samples_to_k(&data, w, h, bpc, decode);
                let row = (w * 4 * bpc).div_ceil(8);
                s.set_plain_content(zlib(&png_filter(&out, row, (4 * bpc).div_ceil(8)))?);
                s.dict.set("Filter", "FlateDecode");
                let mut parms = lopdf::Dictionary::new();
                parms.set("Predictor", 15);
                parms.set("Colors", 4);
                parms.set("BitsPerComponent", bpc as i64);
                parms.set("Columns", w as i64);
                s.dict.set("DecodeParms", parms);
                s.dict
                    .set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()));
                s.dict.remove(b"Decode");
                st.converted += 1;
            }
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_grey_becomes_k_channel() {
        let out = gray_samples_to_k(&[0, 128, 255], 3, 1, 8, (0.0, 1.0));
        assert_eq!(out, vec![0, 0, 0, 255, 0, 0, 0, 127, 0, 0, 0, 0]);
    }

    #[test]
    fn one_bit_grey_keeps_its_depth_and_decode() {
        // pixels 0,1,1,0 (black, white, white, black) -> K bits 1,0,0,1, four bits per pixel
        let out = gray_samples_to_k(&[0b0110_0000], 4, 1, 1, (0.0, 1.0));
        assert_eq!(out, vec![0b0001_0000, 0b0000_0001]);
        // with /Decode [1 0] sample 1 is black
        let inv = gray_samples_to_k(&[0b0110_0000], 4, 1, 1, (1.0, 0.0));
        assert_eq!(inv, vec![0b0000_0001, 0b0001_0000]);
    }

    #[test]
    fn png_filtered_rows_decode_back_through_lopdf() {
        let data: Vec<u8> = (0..64u32).map(|i| ((i * 37) % 251) as u8).collect();
        let mut parms = lopdf::Dictionary::new();
        parms.set("Predictor", 15);
        parms.set("Colors", 4);
        parms.set("BitsPerComponent", 8);
        parms.set("Columns", 4);
        let mut d = lopdf::Dictionary::new();
        d.set("Filter", "FlateDecode");
        d.set("DecodeParms", parms);
        let s = lopdf::Stream::new(d, zlib(&png_filter(&data, 16, 4)).unwrap());
        assert_eq!(s.decompressed_content().unwrap(), data);
    }

    #[test]
    fn palette_entries_become_k_only() {
        assert_eq!(k_palette(&[0, 255]), vec![0, 0, 0, 255, 0, 0, 0, 0]);
    }
}
