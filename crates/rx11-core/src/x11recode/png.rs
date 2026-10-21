//! Lossless "opaque bytes" PNG codec: the pixel block is carried as a 1px
//! tall grayscale image, so nothing in this path ever interprets pixel
//! values — decode returns exactly what encode received. This is the
//! lossless tier of the recoder (and the round-trip proof for the layout
//! math in `frame.rs`).

use flate2::write::ZlibEncoder;
use std::io::Write;

fn crc32(data: &[u8]) -> u32 {
    // Standard IEEE CRC-32, table-driven once per process.
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *v = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ *b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Encode arbitrary bytes as an 8-bit grayscale PNG (width = data len,
/// height 1, filter 0 per scanline).
pub(crate) fn encode_raw_png(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 128);
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(data.len() as u32).to_be_bytes()); // width
    ihdr.extend_from_slice(&1u32.to_be_bytes()); // height
    ihdr.push(8); // bit depth
    ihdr.push(0); // color type: grayscale
    ihdr.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
    chunk(&mut out, b"IHDR", &ihdr);
    // Scanline data: one filter byte (0 = None) then the raw bytes.
    let mut raw = Vec::with_capacity(data.len() + 1);
    raw.push(0);
    raw.extend_from_slice(data);
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    // Compression failure here is impossible in practice (in-memory write);
    // an empty IDAT would be caught by the reader, so propagate as empty.
    let _ = enc.write_all(&raw);
    let idat = enc.finish().unwrap_or_default();
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", b"");
    out
}

/// Decode the exact output of [`encode_raw_png`]; rejects anything else
/// shaped like a PNG we did not produce.
pub(crate) fn decode_raw_png(png: &[u8]) -> Option<Vec<u8>> {
    if png.len() < 8 + 12 + 12 + 12 || png[0..8] != *b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let mut pos = 8usize;
    let mut width = 0u32;
    let mut idat: Vec<u8> = Vec::new();
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]]) as usize;
        let kind = &png[pos + 4..pos + 8];
        let body_start = pos + 8;
        if body_start + len + 4 > png.len() {
            return None;
        }
        let body = &png[body_start..body_start + len];
        let crc = u32::from_be_bytes([
            png[body_start + len],
            png[body_start + len + 1],
            png[body_start + len + 2],
            png[body_start + len + 3],
        ]);
        let mut crc_input = Vec::with_capacity(4 + len);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(body);
        if crc32(&crc_input) != crc {
            return None;
        }
        match kind {
            b"IHDR" => {
                // gray8, height 1, no interlace — only what we encode.
                let height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                if len != 13 || body[8] != 8 || body[9] != 0 || body[12] != 0 || height != 1 {
                    return None;
                }
                width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos = body_start + len + 4;
    }
    use flate2::read::ZlibDecoder;
    let mut raw = Vec::with_capacity(width as usize + 1);
    std::io::Read::read_to_end(&mut ZlibDecoder::new(&idat[..]), &mut raw).ok()?;
    if raw.len() != width as usize + 1 || raw[0] != 0 {
        return None;
    }
    Some(raw.split_off(1)) // drop the scanline filter byte, O(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_png_round_trips_arbitrary_bytes() {
        for case in [
            &b"hello"[..],
            &[0u8; 4096][..],
            &(0..9000u32).map(|i| (i % 256) as u8).collect::<Vec<u8>>(),
        ] {
            let png = encode_raw_png(case);
            assert_eq!(decode_raw_png(&png).unwrap(), case);
        }
    }

    #[test]
    fn raw_png_rejects_tampered_stream() {
        let mut png = encode_raw_png(b"payload-bytes");
        let last = png.len() - 30;
        png[last] ^= 0xFF;
        assert!(decode_raw_png(&png).is_none());
    }
}
