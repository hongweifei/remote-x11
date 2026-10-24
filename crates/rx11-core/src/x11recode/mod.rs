//! X11 channel recode: protocol-aware re-encoding of large `PutImage`
//! payloads (JPEG / lossless PNG) between the two rx11 endpoints.
//!
//! Why this lives in the *x11 channel implementation* and not the DataPlane:
//! a photo on screen is a core PutImage request whose pixel block is raw
//! RGB — the most bandwidth-hungry traffic there is, and invisible to any
//! byte-diff or history-based compressor (every frame's pixels differ).
//! Only recognizing the request makes re-encoding possible.
//!
//! Contract (capability-gated `x11_recode`; on by default, either end can
//! switch it off with `[runtime] image_recode = false`):
//! - Downstream direction (app → relay → local X server): the server-side
//!   [`DownstreamReencoder`] tracks X request framing over the byte stream
//!   and emits *units*: `[kind u8] + body`; kind 0 = verbatim bytes, kind 1
//!   = container carrying the original request prefix plus a JPEG/PNG blob.
//!   Adjacent raw requests coalesce into one unit.
//! - The client-side [`DownstreamExpander`] turns each unit back into the
//!   exact original request bytes (PNG mode is byte-exact; JPEG mode keeps
//!   framing exact and approximates pixels — that is the whole trade).
//! - Uplink direction (X server replies) is never framed or touched.
//! - Any parse doubt on the server degrades to raw passthrough (loss of
//!   optimization, not of correctness). A container the client cannot
//!   expand is a protocol breach: bytes would be lost, so the connection
//!   must fail (the same strict-stream rule as everywhere else).
//!
//! Scope: ZPixmap, depth 24/32, left_pad 0, standard TrueColor masks.
//! Everything else — XYPixmap, 15/16bpp, XYBitmap, extension requests, GL
//! paths that never emit PutImage — passes through untouched.

mod frame;
mod png;

use bytes::Bytes;

/// Which transform the encoder applies to qualifying PutImage payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecodeCodec {
    /// Lossy RGB → JPEG (chroma 4:2:0), the bandwidth winner for photos/video.
    Jpeg,
    /// Lossless: pixel bytes carried as an opaque PNG stream. Byte-exact
    /// reconstruction, useful as the format's safe tier and in tests.
    Png,
}

/// Server-side encoder parameters; the container is self-describing, so the
/// expander follows whatever the encoder actually chose per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecodeSpec {
    pub codec: RecodeCodec,
    pub quality: u8,
    /// Requests whose pixel block is smaller than this are not worth a
    /// codec round trip (also keeps small blits byte-exact).
    pub min_bytes: usize,
}

impl Default for RecodeSpec {
    fn default() -> Self {
        Self {
            codec: RecodeCodec::Jpeg,
            quality: 80,
            min_bytes: 16 * 1024,
        }
    }
}

const KIND_RAW: u8 = 0;
const KIND_CONTAINER: u8 = 1;

/// Server-side stream machine: X11 request bytes in, recode units out.
pub struct DownstreamReencoder {
    spec: RecodeSpec,
    buf: Vec<u8>,
    raw_acc: Vec<u8>,
    stage: Stage,
    big_endian: bool,
    /// Any doubt (unparseable header, absurd length, oversized setup)
    /// permanently degrades the machine to raw passthrough: the stream
    /// stays correct, only the optimization is lost.
    disabled: bool,
}

enum Stage {
    /// Waiting for / buffering the connection setup request (it is not
    /// length-framed like requests; it also latches the byte order).
    Setup,
    Requests,
}

impl DownstreamReencoder {
    pub fn new(spec: RecodeSpec) -> Self {
        Self {
            spec,
            buf: Vec::new(),
            raw_acc: Vec::new(),
            stage: Stage::Setup,
            big_endian: false,
            disabled: false,
        }
    }

    /// Feed one read chunk; resulting unit payloads are appended to `out`
    /// in stream order. Units only break at request boundaries, so the
    /// bytes the far side reassembles are identical in content and order.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Bytes>) {
        self.buf.extend_from_slice(chunk);
        match self.stage {
            Stage::Setup => self.consume_setup(out),
            Stage::Requests => self.consume_requests(out),
        }
        self.flush_raw(out);
    }

    fn flush_raw(&mut self, out: &mut Vec<Bytes>) {
        if !self.raw_acc.is_empty() {
            let raw = std::mem::take(&mut self.raw_acc);
            let mut unit = Vec::with_capacity(raw.len() + 1);
            unit.push(KIND_RAW);
            unit.extend_from_slice(&raw);
            out.push(Bytes::from(unit));
        }
    }

    fn consume_setup(&mut self, out: &mut Vec<Bytes>) {
        // Fail fast on traffic that cannot be an X11 connection: the setup
        // request always begins 'B' or 'l'. Holding a first small write
        // "until the header completes" would stall non-X streams.
        if let Some(&b) = self.buf.first() {
            if b != b'B' && b != b'l' {
                self.disable();
                self.raw_acc.extend(std::mem::take(&mut self.buf));
                return;
            }
        }
        match frame::setup_message_len(&self.buf) {
            None => {
                if self.buf.len() > 64 * 1024 {
                    self.disable();
                    self.raw_acc.extend(std::mem::take(&mut self.buf));
                }
            }
            Some((big_endian, len)) => {
                if self.buf.len() < len {
                    return; // wait for the whole setup request
                }
                self.big_endian = big_endian;
                self.raw_acc.extend_from_slice(&self.buf[..len]);
                self.buf.drain(..len);
                self.stage = Stage::Requests;
                self.consume_requests(out);
            }
        }
    }

    fn consume_requests(&mut self, out: &mut Vec<Bytes>) {
        loop {
            if self.disabled {
                self.raw_acc.extend(std::mem::take(&mut self.buf));
                return;
            }
            if self.buf.len() < 4 {
                return; // request header (opcode, data, length) incomplete
            }
            // The request length is at bytes 2..4, in 4-byte units; 0 marks an
            // extended request, whose 32-bit length follows the header and
            // counts the WHOLE request (the 8-byte header included).
            let len_field = frame::u16_at(&self.buf, 2, self.big_endian) as u64;
            let total: u64 = if len_field == 0 {
                if self.buf.len() < 8 {
                    return;
                }
                frame::u32_at(&self.buf, 4, self.big_endian) as u64 * 4
            } else {
                len_field * 4
            };
            if !(4..=frame::MAX_RECODE_REQUEST).contains(&total) {
                self.disable();
                continue;
            }
            let total = total as usize;
            if self.buf.len() < total {
                return; // partial request: wait for the rest
            }
            let candidate = if self.disabled {
                None
            } else {
                frame::classify_put_image(&self.buf[..total], self.big_endian)
                    .filter(|f| f.data_len >= self.spec.min_bytes)
            };
            let recoded = match candidate {
                Some(f) => self.try_recode_at(total, &f, out),
                None => false,
            };
            if !recoded {
                self.raw_acc.extend_from_slice(&self.buf[..total]);
            }
            self.buf.drain(..total);
        }
    }

    /// Returns true when a container unit was emitted for this request.
    /// The request lives in `self.buf[..total]`; the frame description is
    /// plain data, so field-level borrows keep this method legal.
    fn try_recode_at(
        &mut self,
        total: usize,
        f: &frame::PutImageFrame,
        out: &mut Vec<Bytes>,
    ) -> bool {
        let pixels = &self.buf[f.data_off..f.data_off + f.data_len];
        let (codec_tag, blob) = match self.spec.codec {
            RecodeCodec::Png => (frame::Codec::Png, png::encode_raw_png(pixels)),
            RecodeCodec::Jpeg => match encode_jpeg(f, pixels, self.spec.quality) {
                Some(b) => (frame::Codec::Jpeg, b),
                None => return false,
            },
        };
        if blob.is_empty() {
            return false; // encoder declined: stay raw, never ship a dead container
        }
        // Unit overhead: kind(1) + fixed container header(32) + prefix.
        let overhead = 1 + 32 + f.data_off;
        if blob.len() + overhead >= f.data_len {
            return false; // not a win: keep the original bytes
        }
        let container =
            frame::encode_container(codec_tag, self.spec.quality, f, &self.buf[..total], &blob);
        // Stream order: everything queued before this request goes first.
        self.flush_raw(out);
        let mut unit = Vec::with_capacity(1 + container.len());
        unit.push(KIND_CONTAINER);
        unit.extend_from_slice(&container);
        out.push(Bytes::from(unit));
        true
    }

    fn disable(&mut self) {
        tracing::warn!("x11 recode: framing anomaly, degrading to raw passthrough");
        self.disabled = true;
    }
}

/// Client-side unit → X-server bytes machine. Stateless: every unit is
/// self-contained by construction.
pub struct DownstreamExpander;

impl DownstreamExpander {
    /// `None` = a container could not be expanded: application bytes would
    /// be lost, so the caller must fail the connection (never skip).
    pub fn expand(payload: Bytes) -> Option<Bytes> {
        if payload.is_empty() {
            return Some(payload);
        }
        match payload[0] {
            KIND_RAW => Some(payload.slice(1..)),
            KIND_CONTAINER => {
                let c = frame::decode_container(&payload[1..])?;
                let pixels = match c.codec {
                    frame::Codec::Png => png::decode_raw_png(c.blob)?,
                    frame::Codec::Jpeg => {
                        let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(c.blob));
                        let rgb = dec.decode().ok()?;
                        if dec.info()?.components != 3 {
                            // Our encoder only ever emits 3-channel RGB;
                            // anything else is a container we cannot vouch for.
                            return None;
                        }
                        frame::pack_rgb(&rgb, c.width, c.height, c.big_endian)
                    }
                };
                if pixels.len() != c.data_len {
                    return None;
                }
                Some(frame::rebuild_request(&c, &pixels))
            }
            other => {
                tracing::warn!("x11 recode: unknown unit kind {other}");
                None
            }
        }
    }
}

fn encode_jpeg(f: &frame::PutImageFrame, pixels: &[u8], quality: u8) -> Option<Vec<u8>> {
    let rgb = frame::unpack_rgb(f, pixels);
    let mut out = Vec::new();
    {
        let mut enc = jpeg_encoder::Encoder::new(&mut out, quality);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
        enc.encode(&rgb, f.width, f.height, jpeg_encoder::ColorType::Rgb)
            .ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w16(x: u16, be: bool) -> [u8; 2] {
        if be {
            x.to_be_bytes()
        } else {
            x.to_le_bytes()
        }
    }
    fn w32(x: u32, be: bool) -> [u8; 4] {
        if be {
            x.to_be_bytes()
        } else {
            x.to_le_bytes()
        }
    }
    const fn pad4(n: usize) -> usize {
        (n + 3) & !3
    }

    /// A spec-faithful ZPixmap PutImage request (depth 24, left_pad 0),
    /// pixels a constant color packed per connection byte order.
    fn put_image(width: u16, height: u16, be: bool, fill: u8) -> Vec<u8> {
        let data_len = width as usize * height as usize * 4;
        let total = 24 + data_len;
        let mut req = Vec::with_capacity(total);
        req.push(72); // opcode: X_PutImage
        req.push(2); // request data: format ZPixmap
        req.extend_from_slice(&w16((total / 4) as u16, be)); // length in b32
        req.extend_from_slice(&w32(1, be)); // drawable
        req.extend_from_slice(&w32(2, be)); // gc
        req.extend_from_slice(&w16(width, be));
        req.extend_from_slice(&w16(height, be));
        req.extend_from_slice(&w16(0, be)); // dst x
        req.extend_from_slice(&w16(0, be)); // dst y
        req.push(0); // left pad
        req.push(24); // depth
        req.extend_from_slice(&[0, 0]);
        let (r, g, b) = (fill, fill / 2 + 1, fill / 4 + 2);
        for _ in 0..width as usize * height as usize {
            if be {
                req.extend_from_slice(&[0, r, g, b]);
            } else {
                req.extend_from_slice(&[b, g, r, 0]);
            }
        }
        req
    }

    /// A spec-faithful non-image request: opcode, request data, length in b32
    /// at bytes 2..4, then the payload.
    fn request(opcode: u8, data: u8, payload: &[u8], be: bool) -> Vec<u8> {
        let total = 4 + payload.len();
        let mut req = Vec::with_capacity(total);
        req.push(opcode);
        req.push(data);
        req.extend_from_slice(&w16((total / 4) as u16, be));
        req.extend_from_slice(payload);
        req
    }

    fn setup_bytes(be: bool, name: &str, auth: &str) -> Vec<u8> {
        // X11 connection setup: byte-order(1) unused(1) major(2) minor(2)
        // auth-name length(2) auth-data length(2) unused(2), then the padded
        // authorization protocol name and the padded authorization data.
        let mut v = vec![if be { b'B' } else { b'l' }, 0];
        v.extend_from_slice(&w16(11, be));
        v.extend_from_slice(&w16(0, be));
        v.extend_from_slice(&w16(name.len() as u16, be));
        v.extend_from_slice(&w16(auth.len() as u16, be));
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(name.as_bytes());
        v.resize(frame::SETUP_HEADER_LEN + pad4(name.len()), 0);
        v.extend_from_slice(auth.as_bytes());
        v.resize(
            frame::SETUP_HEADER_LEN + pad4(name.len()) + pad4(auth.len()),
            0,
        );
        v
    }

    fn expand_all(units: Vec<Bytes>) -> Vec<u8> {
        let mut got = Vec::new();
        for u in units {
            got.extend_from_slice(&DownstreamExpander::expand(u).expect("expand"));
        }
        got
    }

    #[test]
    fn machine_passes_non_image_stream_byte_identical() {
        let mut m = DownstreamReencoder::new(RecodeSpec {
            min_bytes: 8,
            ..Default::default()
        });
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "0123456789abcdef");
        // A 16-byte request (opcode 1, length 4 in b32 units).
        let mut other = vec![1u8, 0];
        other.extend_from_slice(&w16(4, false));
        other.extend_from_slice(&[0u8; 12]);
        let mut input = setup.clone();
        input.extend_from_slice(&other);
        let mut out = Vec::new();
        m.push(&input, &mut out);
        assert!(out.iter().all(|u| u[0] == KIND_RAW));
        assert_eq!(expand_all(out), input);
    }

    /// The setup request of a real X client carries a 12-byte fixed header.
    /// Reading it as 10 bytes truncates the header and the machine then waits
    /// forever for a request that can never be framed, so the window never
    /// appears on the far side. The measured header is pinned here.
    #[test]
    fn setup_header_is_twelve_bytes() {
        let setup = setup_bytes(false, "", "");
        // byte-order 'l', unused, major 11, minor 0, name len 0, auth len 0.
        assert_eq!(
            &setup[..frame::SETUP_HEADER_LEN],
            &[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(setup.len(), frame::SETUP_HEADER_LEN);
        assert_eq!(
            frame::setup_message_len(&setup),
            Some((false, frame::SETUP_HEADER_LEN))
        );
    }

    /// The machine must hand the whole setup request over, not a prefix of it.
    #[test]
    fn machine_forwards_the_complete_setup_request() {
        let mut m = DownstreamReencoder::new(RecodeSpec::default());
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "0123456789abcdef");
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        assert_eq!(expand_all(out), setup);
    }

    /// The exact byte stream a real X client sends after the setup: a
    /// QueryExtension whose length field is at bytes 2..4. Reading the length
    /// from the wrong offset makes the machine wait for bytes that never
    /// come, and the window never appears on the far side.
    #[test]
    fn real_client_request_stream_is_framed_correctly() {
        // Captured from a live X client against the local X server.
        let setup: Vec<u8> = vec![
            0x6c, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let query: Vec<u8> = vec![
            0x62, 0x00, 0x05, 0x00, 0x0c, 0x00, 0x00, 0x00, b'B', b'I', b'G', b'-', b'R', b'E',
            b'Q', b'U', b'E', b'S', b'T', b'S',
        ];

        let mut m = DownstreamReencoder::new(RecodeSpec::default());
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        m.push(&query, &mut out);

        // The query is 20 bytes: length 5 in 4-byte units.
        assert_eq!(query.len(), 20);
        let mut want = setup.clone();
        want.extend_from_slice(&query);
        assert_eq!(
            expand_all(out),
            want,
            "the request stream must survive the machine byte for byte"
        );
    }

    /// A request's length lives at bytes 2..4, in 4-byte units.
    #[test]
    fn request_length_is_read_from_the_spec_offset() {
        // QueryExtension with a 12-byte name: 4 header + 12 name = 16 bytes.
        let req = request(0x62, 0x00, b"BIG-REQUESTS", false);
        assert_eq!(req.len(), 16);
        assert_eq!(&req[..4], &[0x62, 0x00, 0x04, 0x00]);

        let mut m = DownstreamReencoder::new(RecodeSpec::default());
        let mut out = Vec::new();
        m.push(&req, &mut out);
        assert_eq!(expand_all(out), req);
    }

    /// The real PutImage request a client sends, captured off the wire.
    ///
    /// It pins three things at once: the opcode (72 = `X_PutImage`, not 53 =
    /// `X_CreatePixmap`), the header layout (width at 12, height at 14,
    /// left-pad at 20, depth at 21, image data at 24), and the length field
    /// (in 4-byte units, at bytes 2..4).
    #[test]
    fn real_put_image_request_is_recognized() {
        // A 900x72 PutImage, the first chunk of a larger image.
        let mut req = Vec::new();
        req.extend_from_slice(&[72, 2, 0x26, 0xfd]); // opcode, format, length
        req.extend_from_slice(&[0x01, 0x00, 0x00, 0x01]); // drawable
        req.extend_from_slice(&[0x02, 0x00, 0x00, 0x01]); // gc
        req.extend_from_slice(&[0x84, 0x03]); // width = 900
        req.extend_from_slice(&[0x48, 0x00]); // height = 72
        req.extend_from_slice(&[0x00, 0x00]); // dst x
        req.extend_from_slice(&[0x00, 0x00]); // dst y
        req.push(0); // left pad
        req.push(24); // depth
        req.extend_from_slice(&[0x00, 0x00]); // unused
        let data_len = 900 * 72 * 4;
        req.extend(std::iter::repeat_n(0u8, data_len));

        // The length field counts the whole request in 4-byte units. The
        // connection is little-endian, so the bytes 26 fd are 0xfd26.
        assert_eq!(req.len(), 24 + data_len);
        assert_eq!(req.len(), 0xfd26 * 4);

        let frame = frame::classify_put_image(&req, false).expect("PutImage");
        assert_eq!(frame.width, 900);
        assert_eq!(frame.height, 72);
        assert_eq!(frame.data_off, 24);
        assert_eq!(frame.data_len, data_len);
        assert_eq!(frame.total, req.len());
    }

    #[test]
    fn png_recode_reconstructs_requests_byte_exact() {
        let spec = RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 64,
            quality: 80,
        };
        for (w, h, be) in [
            (64u16, 4u16, false),
            (8, 20, true),
            (70, 9, false),
            (1, 64, true),
        ] {
            let setup = setup_bytes(be, "MIT-MAGIC-COOKIE-1", "abcd");
            let img = put_image(w, h, be, 200);
            let mut m = DownstreamReencoder::new(spec);
            let mut out = Vec::new();
            m.push(&setup, &mut out);
            m.push(&img, &mut out);
            assert_eq!(out.len(), 2, "setup raw + container: {out:?}");
            assert_eq!(out[0][0], KIND_RAW);
            assert_eq!(out[1][0], KIND_CONTAINER);
            let rebuilt = DownstreamExpander::expand(out[1].clone()).expect("expand");
            assert_eq!(&rebuilt[..], &img[..], "png mode must be byte-exact");
        }
    }

    #[test]
    fn jpeg_recode_keeps_framing_exact_and_color_approx() {
        let spec = RecodeSpec {
            codec: RecodeCodec::Jpeg,
            min_bytes: 64,
            quality: 90,
        };
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "abcd");
        // solid color image: JPEG 4:2:0 preserves constant chroma exactly.
        let img = put_image(16, 16, false, 180);
        let mut m = DownstreamReencoder::new(spec);
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        m.push(&img, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1][0], KIND_CONTAINER);
        let rebuilt = DownstreamExpander::expand(out[1].clone()).expect("expand");
        assert_eq!(rebuilt.len(), img.len());
        assert_eq!(&rebuilt[..24], &img[..24], "request header must survive");
        // constant fill: every pixel within a hair of the original B,G,R
        let (b0, g0, r0) = (img[24], img[25], img[26]);
        for px in rebuilt[24..].as_chunks::<4>().0 {
            assert!((px[0] as i32 - b0 as i32).abs() <= 2, "B {}", px[0]);
            assert!((px[1] as i32 - g0 as i32).abs() <= 2, "G {}", px[1]);
            assert!((px[2] as i32 - r0 as i32).abs() <= 2, "R {}", px[2]);
        }
    }

    #[test]
    fn requests_split_across_chunks_reassemble_in_order() {
        let spec = RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 32,
            quality: 80,
        };
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "abcd");
        let small = put_image(2, 2, false, 10);
        let big = put_image(8, 8, false, 200);
        let mut stream = Vec::new();
        stream.extend_from_slice(&setup);
        stream.extend_from_slice(&small);
        stream.extend_from_slice(&big);

        let mut m = DownstreamReencoder::new(spec);
        let mut out = Vec::new();
        for byte in stream.iter() {
            m.push(&[*byte], &mut out);
        }
        assert_eq!(expand_all(out), stream);
    }

    #[test]
    fn extended_length_requests_are_recoded() {
        let (w, h) = (65u16, 700u16); // 182000 pixel bytes
        let standard = put_image(w, h, false, 90);
        // Re-emit the same request in extended framing: the b32 length field
        // is 0, and the 32-bit length that follows counts the WHOLE request
        // (the 8-byte extended header included), in 4-byte units.
        let body = &standard[4..]; // drawable..depth + data
        let mut ext = Vec::new();
        ext.extend_from_slice(&standard[..2]); // opcode + request data
        ext.extend_from_slice(&w16(0, false)); // length 0 = extended
        ext.extend_from_slice(&w32(((8 + body.len()) / 4) as u32, false));
        ext.extend_from_slice(body);
        let spec = RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 1024,
            quality: 80,
        };
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "ab");
        let mut m = DownstreamReencoder::new(spec);
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        m.push(&ext, &mut out);
        assert!(
            out.iter().any(|u| u[0] == KIND_CONTAINER),
            "extended request should be recoded"
        );
        let mut want = setup.clone();
        want.extend_from_slice(&ext);
        assert_eq!(expand_all(out), want);
    }

    #[test]
    fn absurd_length_disables_machine_without_losing_bytes() {
        let mut m = DownstreamReencoder::new(RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 1,
            quality: 80,
        });
        let setup = setup_bytes(false, "n", "a");
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        // A request claiming ~64 GiB via extended length.
        let mut bogus = vec![72u8, 2];
        bogus.extend_from_slice(&w16(0, false)); // length 0 = extended
        bogus.extend_from_slice(&w32(0xFFFF_FFF0, false));
        bogus.extend_from_slice(&w32(0x0000_000F, false));
        bogus.extend_from_slice(&[9u8; 64]);
        m.push(&bogus, &mut out);
        let img = put_image(8, 8, false, 123);
        m.push(&img, &mut out);
        let mut want = setup.clone();
        want.extend_from_slice(&bogus);
        want.extend_from_slice(&img);
        assert_eq!(expand_all(out), want);
    }

    #[test]
    fn below_threshold_images_stay_raw() {
        let spec = RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 4096,
            quality: 80,
        };
        let setup = setup_bytes(false, "MIT-MAGIC-COOKIE-1", "ab");
        let img = put_image(16, 16, false, 77); // 1024 bytes < 4096
        let mut m = DownstreamReencoder::new(spec);
        let mut out = Vec::new();
        m.push(&setup, &mut out);
        m.push(&img, &mut out);
        assert!(out.iter().all(|u| u[0] == KIND_RAW));
        let mut want = setup.clone();
        want.extend_from_slice(&img);
        assert_eq!(expand_all(out), want);
    }

    #[test]
    fn non_x11_first_byte_flushes_immediately() {
        // A single non-'B'/'l' byte must come back as a raw unit in the
        // SAME push (no "wait for more" holdback).
        let mut m = DownstreamReencoder::new(RecodeSpec::default());
        let mut out = Vec::new();
        m.push(b"x", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..], &[KIND_RAW, b'x']);
        // and the machine stays raw for everything after
        m.push(b"yz", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[1][..], &[KIND_RAW, b'y', b'z']);
    }

    #[test]
    fn expander_rejects_garbage_container() {
        let mut unit = vec![KIND_CONTAINER];
        unit.extend_from_slice(b"RXICgarbage-that-cannot-be-parsed-at-all");
        assert!(DownstreamExpander::expand(Bytes::from(unit)).is_none());
    }

    #[test]
    fn incompressible_pixels_fall_back_to_raw() {
        // High-entropy pixel block: the lossless tier cannot shrink it, so
        // the machine must keep the request raw (container is not a win).
        let mut rng_state = 0x12345678u64;
        let mut fixed = put_image(64, 64, false, 0);
        for q in fixed[24..].as_chunks_mut::<4>().0 {
            rng_state = rng_state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            q.copy_from_slice(&((rng_state >> 32) as u32).to_le_bytes());
        }
        let spec = RecodeSpec {
            codec: RecodeCodec::Png,
            min_bytes: 1,
            quality: 80,
        };
        let mut m = DownstreamReencoder::new(spec);
        let mut out = Vec::new();
        m.push(&setup_bytes(false, "n", "a"), &mut out);
        m.push(&fixed, &mut out);
        assert!(
            out.iter().all(|u| u[0] == KIND_RAW),
            "high-entropy must not gain from deflate"
        );
    }
}
