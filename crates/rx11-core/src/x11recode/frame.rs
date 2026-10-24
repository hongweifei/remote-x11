//! PutImage frame recognition, the recode container format, and the
//! pixel layout math that makes a reconstructed request byte-faithful.
//!
//! Wire facts this encodes (X11 protocol spec, PutImage = request 53).
//!
//! Every X11 request starts with the same 4-byte header:
//!
//! ```text
//!  1      opcode (PutImage = 53)
//!  1      request data (PutImage: format, 2 = ZPixmap)
//!  2      length of the request in 4-byte units
//! ```
//!
//! so the request length lives at bytes 2..4, and a request occupies
//! `length * 4` bytes. A length of 0 marks an EXTENDED request, whose real
//! length is the u32 pair that follows the 4-byte header (bytes 4..12),
//! counting the bytes after those 12 bytes.
//!
//! PutImage itself continues:
//!
//! ```text
//!  4      window
//!  4      gc
//!  2      width
//!  2      height
//!  1      left pad
//!  1      depth
//!  2      unused
//!  *      image data: rows of ceil(width*bpp/32)*4 bytes
//! ```
//!
//! The standard-form PutImage therefore has its image data at offset 20;
//! the extended form adds the 8-byte length pair, so the data starts at 28.
//!
//! Requests are written in the CONNECTION byte order, latched from the
//! client setup message's first byte ('B' or 'l').
//!
//! Recode only touches ZPixmap at depth 24/32 with left_pad == 0 (bpp 32),
//! the layout every modern TrueColor visual uses. Everything else must
//! survive the machine byte-identically.

use bytes::{BufMut, Bytes, BytesMut};

/// Core X11 request opcode for PutImage.
const OP_PUT_IMAGE: u8 = 53;
/// ZPixmap format value — pixel-per-pixel layout (vs bitmap planes).
pub(crate) const FORMAT_ZPIXMAP: u8 = 2;

/// Maximum request the machine will ever buffer for recoding; a larger or
/// corrupt length disables the machine (raw passthrough keeps working).
pub(crate) const MAX_RECODE_REQUEST: u64 = 16 * 1024 * 1024 + 64;

pub fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

#[inline]
pub(crate) fn u16_at(b: &[u8], at: usize, big_endian: bool) -> u16 {
    let (x, y) = (b[at], b[at + 1]);
    if big_endian {
        u16::from_be_bytes([x, y])
    } else {
        u16::from_le_bytes([x, y])
    }
}

#[inline]
pub(crate) fn u32_at(b: &[u8], at: usize, big_endian: bool) -> u32 {
    let q = [b[at], b[at + 1], b[at + 2], b[at + 3]];
    if big_endian {
        u32::from_be_bytes(q)
    } else {
        u32::from_le_bytes(q)
    }
}

/// A recognized, recodable PutImage request within `req` (a complete request
/// buffer). All offsets are relative to the request start.
pub(crate) struct PutImageFrame {
    /// Byte offset where image data begins (24 standard, 32 extended).
    pub data_off: usize,
    pub width: u16,
    pub height: u16,
    /// Pixel bytes: exactly `width * height * 4` (left_pad is 0 here).
    pub data_len: usize,
    /// Total request bytes (including any 4-byte alignment tail).
    pub total: usize,
    pub big_endian: bool,
}

/// Try to classify a complete request as a recodable PutImage. `None` means
/// "forward this request raw" — never an error.
pub(crate) fn classify_put_image(req: &[u8], big_endian: bool) -> Option<PutImageFrame> {
    if req[0] != OP_PUT_IMAGE || req[1] != FORMAT_ZPIXMAP {
        return None;
    }
    // The request length sits at bytes 2..4, in 4-byte units.
    let len_field = u16_at(req, 2, big_endian) as u32;
    let (data_off, total) = if len_field == 0 {
        // Extended request: the u32 pair at bytes 4..12 gives the byte count
        // that follows the 12-byte header.
        if req.len() < 16 {
            return None;
        }
        let bytes_after_lo = u32_at(req, 4, big_endian) as u64;
        let bytes_after_hi = u32_at(req, 8, big_endian) as u64;
        let total = 12 + bytes_after_lo + (bytes_after_hi << 32);
        if total > MAX_RECODE_REQUEST || total > req.len() as u64 {
            return None;
        }
        (28usize, total as usize)
    } else {
        let total = len_field as u64 * 4;
        if total > MAX_RECODE_REQUEST || total > req.len() as u64 {
            return None;
        }
        (20usize, total as usize)
    };
    let left_pad = req[data_off - 4];
    let depth = req[data_off - 3];
    if left_pad != 0 || !matches!(depth, 24 | 32) {
        return None;
    }
    let width = u16_at(req, data_off - 8, big_endian);
    let height = u16_at(req, data_off - 6, big_endian);
    if width == 0 || height == 0 {
        return None;
    }
    let data_len = width as usize * height as usize * 4;
    if data_off + data_len > total {
        return None;
    }
    Some(PutImageFrame {
        data_off,
        width,
        height,
        data_len,
        total,
        big_endian,
    })
}

/// Extract RGB24 rows from the 32bpp wire layout for the connection's byte
/// order (standard TrueColor masks 0xFF0000/0xFF00/0xFF).
pub(crate) fn unpack_rgb(frame: &PutImageFrame, pixels: &[u8]) -> Vec<u8> {
    let n = frame.width as usize * frame.height as usize;
    let mut rgb = Vec::with_capacity(n * 3);
    if frame.big_endian {
        // u32 0x00RRGGBB written big-endian: bytes [_, R, G, B].
        for px in pixels[..n * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[1], px[2], px[3]]);
        }
    } else {
        // Little-endian: bytes [B, G, R, _].
        for px in pixels[..n * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
    }
    rgb
}

/// Rebuild the 32bpp wire layout from RGB24 rows. Every byte of the pixel
/// block is regenerated (the X pad byte included — see tests).
pub(crate) fn pack_rgb(rgb: &[u8], width: u16, height: u16, big_endian: bool) -> Vec<u8> {
    let n = width as usize * height as usize;
    debug_assert!(rgb.len() >= n * 3);
    let mut out = Vec::with_capacity(n * 4);
    if big_endian {
        for px in rgb[..n * 3].as_chunks::<3>().0 {
            out.extend_from_slice(&[0, px[0], px[1], px[2]]);
        }
    } else {
        for px in rgb[..n * 3].as_chunks::<3>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], 0]);
        }
    }
    out
}

/// Container magic carried in the kind-1 unit payload.
pub(crate) const CONTAINER_MAGIC: [u8; 4] = *b"RXIC";
pub(crate) const CONTAINER_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    Jpeg,
    Png,
}

impl Codec {
    fn byte(self) -> u8 {
        match self {
            Codec::Jpeg => 1,
            Codec::Png => 2,
        }
    }
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Codec::Jpeg),
            2 => Some(Codec::Png),
            _ => None,
        }
    }
}

/// kind-1 unit body:
/// `MAGIC ver codec quality flags | u32 prefix_len | u32 total |
///  u32 w | u32 h | u32 data_len | u32 blob_len | prefix | blob`
pub(crate) fn encode_container(
    codec: Codec,
    quality: u8,
    frame: &PutImageFrame,
    req: &[u8],
    blob: &[u8],
) -> Bytes {
    let mut buf = BytesMut::with_capacity(32 + 8 + frame.data_off + blob.len());
    buf.extend_from_slice(&CONTAINER_MAGIC);
    buf.put_u8(CONTAINER_VERSION);
    buf.put_u8(codec.byte());
    buf.put_u8(quality);
    buf.put_u8(if frame.big_endian { 1 } else { 0 });
    buf.put_u32_le(frame.data_off as u32);
    buf.put_u32_le(frame.total as u32);
    buf.put_u32_le(frame.width as u32);
    buf.put_u32_le(frame.height as u32);
    buf.put_u32_le(frame.data_len as u32);
    buf.put_u32_le(blob.len() as u32);
    buf.extend_from_slice(&req[..frame.data_off]);
    buf.extend_from_slice(blob);
    buf.freeze()
}

pub(crate) struct Container<'a> {
    pub codec: Codec,
    pub big_endian: bool,
    pub prefix: &'a [u8],
    pub total: usize,
    pub width: u16,
    pub height: u16,
    pub data_len: usize,
    pub blob: &'a [u8],
}

/// Strict parse — the expander treats any failure as a protocol breach
/// (bytes of the X stream would be lost otherwise).
pub(crate) fn decode_container(body: &[u8]) -> Option<Container<'_>> {
    const FIXED: usize = 4 + 1 + 1 + 1 + 1 + 6 * 4;
    if body.len() < FIXED || body[0..4] != CONTAINER_MAGIC {
        return None;
    }
    if body[4] != CONTAINER_VERSION {
        return None;
    }
    let codec = Codec::from_byte(body[5])?;
    let big_endian = body[7] & 1 == 1;
    let rd = |i: usize| {
        u32::from_le_bytes([
            body[8 + i * 4],
            body[9 + i * 4],
            body[10 + i * 4],
            body[11 + i * 4],
        ]) as usize
    };
    let (prefix_len, total, width, height, data_len, blob_len) =
        (rd(0), rd(1), rd(2), rd(3), rd(4), rd(5));
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || data_len != width * height * 4
        || prefix_len + blob_len + FIXED > body.len()
        || total < prefix_len + data_len
    {
        return None;
    }
    let prefix = &body[FIXED..FIXED + prefix_len];
    // A PutImage request must keep its opcode/format after transport.
    if prefix.len() < 2 || prefix[0] != OP_PUT_IMAGE || prefix[1] != FORMAT_ZPIXMAP {
        return None;
    }
    let blob = &body[FIXED + prefix_len..FIXED + prefix_len + blob_len];
    Some(Container {
        codec,
        big_endian,
        prefix,
        total,
        width: width as u16,
        height: height as u16,
        data_len,
        blob,
    })
}

/// Rebuild the complete original request bytes from a decoded container.
pub(crate) fn rebuild_request(c: &Container<'_>, pixels: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(c.total);
    buf.extend_from_slice(c.prefix);
    buf.extend_from_slice(&pixels[..c.data_len]);
    // Alignment tail: spec says padding is undefined content; senders use
    // zero and so do we.
    buf.extend_from_slice(&vec![0u8; c.total - c.prefix.len() - c.data_len]);
    buf.freeze()
}

/// Fixed part of the X11 connection setup request, in bytes:
/// byte-order(1) unused(1) protocol-major(2) protocol-minor(2)
/// authorization-protocol-name length(2) authorization-data length(2)
/// unused(2).
pub(crate) const SETUP_HEADER_LEN: usize = 12;

/// The connection setup header the very first client message begins with:
/// [`SETUP_HEADER_LEN`] fixed bytes + padded name + padded auth. Returns the
/// byte order flag and total length, once the whole message is buffered.
pub(crate) fn setup_message_len(buf: &[u8]) -> Option<(bool, usize)> {
    if buf.len() < SETUP_HEADER_LEN {
        return None;
    }
    let big_endian = match buf[0] {
        b'B' => true,
        b'l' => false,
        _ => return None,
    };
    let name_len = u16_at(buf, 6, big_endian) as usize;
    let auth_len = u16_at(buf, 8, big_endian) as usize;
    Some((
        big_endian,
        SETUP_HEADER_LEN + pad4(name_len) + pad4(auth_len),
    ))
}
