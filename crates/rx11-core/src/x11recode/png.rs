//! Lossless "opaque bytes" PNG codec on the `png` crate: the pixel block is
//! carried as a 1px tall grayscale image, so nothing in this path ever
//! interprets pixel values — decode returns exactly what encode received.
//! This is the lossless tier of the recoder (and the round-trip proof for
//! the layout math in `frame.rs`).
//!
//! Chunk assembly, CRC-32 and filter selection (the crate's default
//! `Adaptive`) are the library's job; the hand-rolled predecessor this
//! replaced always emitted filter 0 and left the filter stage on the table.
//!
//! Why `png` and not `zune-png` (evaluated 2026-10-21, v0.5.2): zune-png's
//! decoder is the fastest in the ecosystem, but its encoder runs on
//! zune-inflate's deflate, whose `DeflateEncodingStrategy` has exactly one
//! variant — `NoCompression` ("Not yet complete"). A PNG tier that cannot
//! shrink IDAT is not a PNG tier: the gradient round-trip came out LARGER
//! than the input (4165 B for 4096 B of pixels), which would silently
//! degrade every container to raw. Re-evaluate when the encoder grows real
//! strategies.

use std::io::Cursor;

use super::frame::MAX_RECODE_REQUEST;

/// Encode arbitrary bytes as an 8-bit grayscale PNG (width = data len,
/// height 1). Failure (empty input, absurd width, encoder errors) surfaces
/// as an EMPTY return: the caller's container-size check degrades that unit
/// to raw passthrough — encoding never fails upward and never corrupts.
pub(crate) fn encode_raw_png(data: &[u8]) -> Vec<u8> {
    let Ok(width) = u32::try_from(data.len()) else {
        return Vec::new();
    };
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(data.len() / 2 + 128);
    let result = (|| -> Result<(), png::EncodingError> {
        let mut encoder = png::Encoder::new(&mut out, width, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(data)?;
        Ok(())
    })();
    if result.is_err() {
        return Vec::new();
    }
    out
}

/// Decode the exact output of [`encode_raw_png`]; rejects anything else
/// shaped like a PNG we did not produce. Shape checks mirror the encoder
/// (gray8, single row, not interlaced); corruption is rejected by the
/// library's chunk CRC and inflate checksums.
pub(crate) fn decode_raw_png(png_data: &[u8]) -> Option<Vec<u8>> {
    let mut reader = png::Decoder::new(Cursor::new(png_data)).read_info().ok()?;
    {
        let info = reader.info();
        if info.width == 0
            || info.height != 1
            || info.color_type != png::ColorType::Grayscale
            || info.bit_depth != png::BitDepth::Eight
            || info.interlaced
        {
            return None;
        }
    }
    let size = reader.output_buffer_size()?;
    if size as u64 > MAX_RECODE_REQUEST {
        return None; // hand-built giant IHDR: our machine never made one
    }
    let mut buf = vec![0u8; size];
    let out = reader.next_frame(&mut buf).ok()?;
    let len = (out.line_size * out.height as usize).min(buf.len());
    buf.truncate(len);
    Some(buf)
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
            assert!(!png.is_empty());
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

    #[test]
    fn raw_png_rejects_foreign_shapes() {
        // Not gray8: an RGB single-row image is a different contract.
        let mut rgb = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut rgb, 4, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&[7u8; 12])
                .unwrap();
        }
        assert!(decode_raw_png(&rgb).is_none());
        // Not one row: gray but height 2.
        let mut tall = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut tall, 4, 2);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header()
                .unwrap()
                .write_image_data(&[7u8; 8])
                .unwrap();
        }
        assert!(decode_raw_png(&tall).is_none());
        // Empty never encodes: the caller degrades to raw.
        assert!(encode_raw_png(&[]).is_empty());
    }

    #[test]
    fn adaptive_filters_compress_gradients() {
        // The property the hand-rolled encoder could not have: per-row
        // filter choice (Adaptive) shrinks gradient-ish pixel spans hard
        // even before deflate, and the round trip stays byte-exact.
        let data: Vec<u8> = (0..4096).map(|i| (i / 16) as u8).collect();
        let png = encode_raw_png(&data);
        assert!(
            png.len() < data.len() / 2,
            "{} vs {}",
            png.len(),
            data.len()
        );
        assert_eq!(decode_raw_png(&png).unwrap(), data);
    }
}
