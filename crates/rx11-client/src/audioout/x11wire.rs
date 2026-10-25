//! The X11 wire bytes the client needs to read: the setup request (which
//! carries the authorization the local X server expects) and the setup
//! reply (which carries the resource range the connection owns).
//!
//! The X11 protocol library does the parsing and the serialization. That
//! matters for the request, whose authorization name and data are padded
//! to four bytes: the library knows the rule, so a request is measured
//! rather than guessed.
//!
//! The reply is read at the offsets the X11 protocol fixes, and in the
//! byte order the setup REQUEST announced. The reply carries no
//! byte-order byte of its own, but the X server answers in the order the
//! client asked in — and the client here is the remote application,
//! whose setup request the proxy forwards verbatim. A reply read in the
//! wrong order yields a wrong resource range, and a wrong range traces a
//! focused window to the wrong application.
//!
//! The bytes themselves are never modified: the client only takes copies
//! of what it needs and forwards the original stream untouched.

use x11rb_protocol::protocol::xproto::SetupRequest;
use x11rb_protocol::x11_utils::TryParse;

/// The byte order of an X11 connection.
///
/// The X server answers in the byte order the client announced in its
/// setup request, so every field of the setup reply must be read in the
/// order the request asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByteOrder {
    /// Little-endian: the setup request began with 'l'.
    LittleEndian,
    /// Big-endian: the setup request began with 'B'.
    BigEndian,
}

impl ByteOrder {
    /// The byte order a setup request announces, or `None` when the bytes
    /// are not a setup request at all.
    pub(crate) fn from_request(bytes: &[u8]) -> Option<Self> {
        match bytes.first() {
            Some(b'l') => Some(Self::LittleEndian),
            Some(b'B') => Some(Self::BigEndian),
            _ => None,
        }
    }

    /// Read a 16-bit field in this byte order.
    fn u16(self, bytes: &[u8], at: usize) -> Option<u16> {
        let pair = bytes.get(at..at + 2)?;
        Some(match self {
            Self::LittleEndian => u16::from_le_bytes([pair[0], pair[1]]),
            Self::BigEndian => u16::from_be_bytes([pair[0], pair[1]]),
        })
    }

    /// Read a 32-bit field in this byte order.
    fn u32(self, bytes: &[u8], at: usize) -> Option<u32> {
        let quad = bytes.get(at..at + 4)?;
        Some(match self {
            Self::LittleEndian => u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]),
            Self::BigEndian => u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]]),
        })
    }
}

/// The authorization a connection must present to the local X server.
///
/// The bytes are taken verbatim from the application's setup request: the
/// authorization protocol name and its data, exactly as the server will
/// read them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Authorization {
    /// The authorization protocol name (e.g. `MIT-MAGIC-COOKIE-1`).
    pub name: Vec<u8>,
    /// The authorization data.
    pub data: Vec<u8>,
}

impl Authorization {
    /// Whether there is anything to present.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Read the authorization out of an X11 setup request.
    ///
    /// The request must be COMPLETE: the header, the padded
    /// authorization name and the padded authorization data. A partial
    /// request yields `None`; the caller accumulates until it is whole.
    pub fn from_setup_request(request: &[u8]) -> Option<Self> {
        // The library reads the request in the machine's own byte order.
        // A request that announced the other order would have its two
        // length fields read as nonsense, so they are swapped into the
        // machine's order first. Everything else in the request is
        // length-delimited or opaque, so it needs no swapping: the
        // padding rule is the same in both orders.
        let order = ByteOrder::from_request(request)?;
        let normalized = if order == ByteOrder::LittleEndian {
            std::borrow::Cow::Borrowed(request)
        } else {
            let mut bytes = request.to_vec();
            swap_u16(&mut bytes, 6);
            swap_u16(&mut bytes, 8);
            std::borrow::Cow::Owned(bytes)
        };
        let (parsed, _) = SetupRequest::try_parse(&normalized).ok()?;
        Some(Self {
            name: parsed.authorization_protocol_name,
            data: parsed.authorization_protocol_data,
        })
    }
}

/// Swap the two bytes of the 16-bit field at the given offset.
fn swap_u16(bytes: &mut [u8], at: usize) {
    if let Some(pair) = bytes.get_mut(at..at + 2) {
        pair.swap(0, 1);
    }
}

/// The length of the setup request at the head of a stream.
///
/// The length comes from the request's own header: the authorization
/// name and the authorization data are each padded to four bytes, and
/// the padding belongs to the request. `None` while the header itself is
/// still missing.
pub(crate) fn setup_request_len(bytes: &[u8]) -> Option<usize> {
    const HEADER: usize = 12;
    if bytes.len() < HEADER {
        return None;
    }
    let order = ByteOrder::from_request(bytes)?;
    let name_len = order.u16(bytes, 6)? as usize;
    let data_len = order.u16(bytes, 8)? as usize;
    Some(HEADER + name_len.next_multiple_of(4) + data_len.next_multiple_of(4))
}

/// The length of the setup reply at the head of a stream.
///
/// The reply's length field counts the data after the eight-byte prefix
/// in four-byte units; the whole reply is the prefix plus that.
pub(crate) fn setup_reply_len(bytes: &[u8], order: ByteOrder) -> Option<usize> {
    const PREFIX: usize = 8;
    if bytes.len() < PREFIX {
        return None;
    }
    if bytes[0] != 1 {
        return None; // the setup was refused
    }
    let length = order.u16(bytes, 6)? as usize;
    Some(PREFIX + length * 4)
}

/// The resource range a connection owns, read from the local X server's
/// setup reply.
///
/// The reply's fixed part begins with the release number, the base of the
/// connection's resource id range and the mask that gives its size. The
/// base and the mask together give the range of ids this connection may
/// create, and a window id inside that range belongs to this connection.
///
/// The two numbers sit at fixed offsets in the reply's fixed part, so
/// they can be read as soon as the fixed part has arrived — the reply's
/// variable part (the vendor string, the pixmap formats, the screens)
/// says nothing about the range. The test pins these offsets against
/// the protocol library's own reader.
pub(crate) fn setup_reply_range(bytes: &[u8], order: ByteOrder) -> Option<(u32, u32)> {
    const PREFIX: usize = 8;
    // The fixed part: release number, resource id base, resource id mask.
    const BASE_AT: usize = PREFIX + 4;
    const MASK_AT: usize = PREFIX + 8;
    const NEEDED: usize = MASK_AT + 4;
    if bytes.len() < NEEDED {
        return None;
    }
    if bytes[0] != 1 {
        return None; // the setup was refused
    }
    let base = order.u32(bytes, BASE_AT)?;
    let mask = order.u32(bytes, MASK_AT)?;
    Some((base, mask))
}

/// Watches the head of a stream for the X11 setup request.
///
/// The request may arrive in pieces — the relay's frames do not respect
/// the request's boundaries — so the watcher accumulates until the
/// request is whole. Once it has been seen, the watcher stops
/// accumulating: the rest of the stream is ordinary requests.
pub(crate) struct SetupRequestWatcher {
    /// The bytes seen so far, until the request is complete.
    buffer: Vec<u8>,
    /// The authorization of the request, once it has been parsed.
    authorization: Option<Authorization>,
    /// The byte order the request announced.
    order: Option<ByteOrder>,
    /// Whether the request has been seen (or given up on).
    finished: bool,
}

impl SetupRequestWatcher {
    /// A watcher that has seen nothing yet.
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            authorization: None,
            order: None,
            finished: false,
        }
    }

    /// Feed a chunk of the stream.
    ///
    /// Returns the authorization once the setup request is whole.
    pub(crate) fn observe(&mut self, chunk: &[u8]) -> Option<&Authorization> {
        if self.finished {
            return None;
        }
        self.buffer.extend_from_slice(chunk);
        let Some(len) = setup_request_len(&self.buffer) else {
            // The header is not whole yet. A stream that is not an X11
            // connection at all would grow the buffer forever, so a
            // generous limit ends the watch.
            if self.buffer.len() > MAX_SETUP_BYTES {
                self.finished = true;
            }
            return None;
        };
        if self.buffer.len() < len {
            return None; // wait for the whole request
        }
        self.finished = true;
        let request = &self.buffer[..len];
        self.order = ByteOrder::from_request(request);
        self.authorization = Authorization::from_setup_request(request);
        self.authorization.as_ref()
    }

    /// The byte order the request announced.
    ///
    /// `None` until the request has been seen.
    pub(crate) fn order(&self) -> Option<ByteOrder> {
        self.order
    }
}

/// The largest setup request the watcher will wait for. A setup request
/// is a few dozen bytes; a megabyte of "setup" is not one.
const MAX_SETUP_BYTES: usize = 64 * 1024;

/// Watches the head of a stream for the local X server's setup reply.
///
/// The reply may arrive in pieces, so the watcher accumulates until the
/// reply is whole and then reports the resource range once.
pub(crate) struct SetupReplyWatcher {
    /// The bytes seen so far, until the reply is complete.
    buffer: Vec<u8>,
    /// The resource range, once the reply has been read.
    range: Option<(u32, u32)>,
    /// Whether the reply has been seen (or given up on).
    finished: bool,
}

impl SetupReplyWatcher {
    /// A watcher that has seen nothing yet.
    pub(crate) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            range: None,
            finished: false,
        }
    }

    /// Feed a chunk of the stream.
    ///
    /// Returns the resource range once the setup reply is whole.
    pub(crate) fn observe(&mut self, chunk: &[u8], order: ByteOrder) -> Option<(u32, u32)> {
        if self.finished {
            return None;
        }
        self.buffer.extend_from_slice(chunk);
        let Some(len) = setup_reply_len(&self.buffer, order) else {
            if self.buffer.len() > MAX_SETUP_BYTES {
                self.finished = true;
            }
            return None;
        };
        if self.buffer.len() < len {
            return None; // wait for the whole reply
        }
        self.finished = true;
        self.range = setup_reply_range(&self.buffer, order);
        self.range
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb_protocol::protocol::xproto::Setup;
    use x11rb_protocol::x11_utils::Serialize;

    /// The setup request of the real X clients: a name whose length is
    /// not a multiple of four, so the padding is exercised.
    const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

    fn setup_request_bytes(name: &[u8], data: &[u8]) -> Vec<u8> {
        // The library builds the request; the authorization it carries is
        // the one handed in.
        let (_connect, setup) =
            x11rb_protocol::connect::Connect::with_authorization(name.to_vec(), data.to_vec());
        setup
    }

    #[test]
    fn the_authorization_is_read_out_of_the_setup_request() {
        let request = setup_request_bytes(COOKIE_NAME, &[0xAB; 16]);
        let auth = Authorization::from_setup_request(&request).expect("an authorization");
        assert_eq!(auth.name, COOKIE_NAME);
        assert_eq!(auth.data.len(), 16);
        assert_eq!(auth.data[0], 0xAB);
    }

    #[test]
    fn a_setup_request_without_authorization_yields_an_empty_one() {
        let request = setup_request_bytes(b"", b"");
        let auth = Authorization::from_setup_request(&request).expect("parsed");
        assert!(auth.is_empty());
    }

    #[test]
    fn a_truncated_setup_request_yields_none() {
        assert!(Authorization::from_setup_request(&[b'l', 0, 11]).is_none());
    }

    #[test]
    fn the_setup_request_round_trips_through_the_authorization() {
        let original = setup_request_bytes(COOKIE_NAME, &[0x01; 16]);
        let auth = Authorization::from_setup_request(&original).expect("an authorization");
        assert_eq!(auth.name, COOKIE_NAME);
        assert_eq!(auth.data, vec![0x01; 16]);
    }

    #[test]
    fn a_big_endian_setup_request_is_read_too() {
        // The header announces big endian, and the lengths inside it are
        // big endian too.
        let mut original = vec![b'B', 0];
        original.extend_from_slice(&11u16.to_be_bytes());
        original.extend_from_slice(&0u16.to_be_bytes());
        original.extend_from_slice(&4u16.to_be_bytes());
        original.extend_from_slice(&2u16.to_be_bytes());
        original.extend_from_slice(&[0u8, 0]);
        original.extend_from_slice(b"TEST");
        original.extend_from_slice(&[0x09, 0x08]);
        original.extend_from_slice(&[0u8, 0]); // the data is padded to four
        let auth = Authorization::from_setup_request(&original).expect("an authorization");
        assert_eq!(auth.name, b"TEST");
        assert_eq!(auth.data, vec![0x09, 0x08]);
    }

    #[test]
    fn the_length_of_a_setup_request_is_measured_not_guessed() {
        let request = setup_request_bytes(COOKIE_NAME, &[0x01; 16]);
        let len = setup_request_len(&request).expect("a length");
        assert_eq!(
            len,
            request.len(),
            "the measured length is the whole request"
        );
        // A partial request measures short of the whole.
        assert!(setup_request_len(&request[..8]).is_none());
        // A header that is not an X11 setup request has no length.
        assert!(setup_request_len(&[0x00; 12]).is_none());
    }

    #[test]
    fn the_measured_length_agrees_with_the_protocol_library() {
        // The length the watcher waits for must be exactly the length
        // the library consumes: a request the library reads must not be
        // cut short, and nothing beyond it may be swallowed.
        for (name, data) in [
            (COOKIE_NAME.to_vec(), vec![0x01; 16]),
            (b"TEST".to_vec(), vec![0x09, 0x08]),
            (Vec::new(), Vec::new()),
            (b"X".to_vec(), vec![0x01; 5]),
        ] {
            let request = setup_request_bytes(&name, &data);
            let len = setup_request_len(&request).expect("a length");
            assert_eq!(
                len,
                request.len(),
                "the measured length is the whole request"
            );
            let (parsed, rest) = SetupRequest::try_parse(&request).expect("the library reads it");
            assert_eq!(
                parsed.authorization_protocol_name, name,
                "the library reads the same name"
            );
            assert_eq!(parsed.authorization_protocol_data, data);
            assert!(rest.is_empty(), "the library consumed exactly the request");
        }
    }

    /// A setup reply with the given resource range, built the way the
    /// X server builds it.
    ///
    /// The library's `Setup` covers the whole reply, prefix included:
    /// the status byte, the protocol version and the length are its
    /// first fields.
    fn setup_reply_bytes(base: u32, mask: u32) -> Vec<u8> {
        let mut setup = Setup {
            status: 1,
            protocol_major_version: 11,
            protocol_minor_version: 0,
            length: 0,
            release_number: 0,
            resource_id_base: base,
            resource_id_mask: mask,
            motion_buffer_size: 0,
            maximum_request_length: 0,
            image_byte_order: x11rb_protocol::protocol::xproto::ImageOrder::LSB_FIRST,
            bitmap_format_bit_order: x11rb_protocol::protocol::xproto::ImageOrder::LSB_FIRST,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 0,
            max_keycode: 0,
            vendor: Vec::new(),
            pixmap_formats: Vec::new(),
            roots: Vec::new(),
        };
        // The length field counts the data after the eight-byte prefix,
        // in four-byte units.
        setup.length = (setup.serialize().len() - 8).div_ceil(4) as u16;
        setup.serialize()
    }

    #[test]
    fn the_resource_range_is_read_out_of_the_setup_reply() {
        let reply = setup_reply_bytes(0x2000000, 0x1fffff);
        let (base, mask) = setup_reply_range(&reply, ByteOrder::LittleEndian).expect("a range");
        assert_eq!(base, 0x2000000);
        assert_eq!(mask, 0x1fffff);
    }

    #[test]
    fn the_resource_range_offsets_agree_with_the_protocol_library() {
        // The reply is read at fixed offsets; the library must read the
        // same numbers out of the same bytes.
        let reply = setup_reply_bytes(0x2000000, 0x1fffff);
        let (parsed, _) = Setup::try_parse(&reply).expect("the library reads it");
        let (base, mask) = setup_reply_range(&reply, ByteOrder::LittleEndian).expect("a range");
        assert_eq!(base, parsed.resource_id_base);
        assert_eq!(mask, parsed.resource_id_mask);
    }

    #[test]
    fn the_resource_range_needs_only_the_fixed_part_of_the_reply() {
        // The library's reader needs the WHOLE reply — the vendor
        // string, the pixmap formats and the screens. The range does
        // not: it sits in the fixed part, and the fixed part may be all
        // that has arrived. That is why the range is read at fixed
        // offsets.
        let head = &REAL_SETUP_REPLY_HEAD[..];
        assert!(
            Setup::try_parse(head).is_err(),
            "the library cannot read a reply without its variable part"
        );
        let (base, mask) = setup_reply_range(head, ByteOrder::LittleEndian).expect("a range");
        assert_eq!((base, mask), (0x00800000, 0x001fffff));
    }

    #[test]
    fn the_resource_range_is_read_in_the_orders_byte_order() {
        // The reply is read in the order the setup request announced. The
        // numbers must come out the same either way, so the range of a
        // big-endian reply is the same as the range of a little-endian
        // one.
        let reply = setup_reply_bytes(0x2000000, 0x1fffff);
        let (base, mask) = setup_reply_range(&reply, ByteOrder::LittleEndian).expect("a range");
        assert_eq!((base, mask), (0x2000000, 0x1fffff));

        // The same reply, with the fixed part's numbers in the other
        // order: the reader must still find the same range.
        let mut swapped = reply.clone();
        for at in (8..24).step_by(4) {
            swapped[at..at + 4].reverse();
        }
        let (base, mask) = setup_reply_range(&swapped, ByteOrder::BigEndian).expect("a range");
        assert_eq!((base, mask), (0x2000000, 0x1fffff));
    }

    #[test]
    fn a_refused_setup_has_no_resource_range() {
        let mut reply = setup_reply_bytes(0x2000000, 0x1fffff);
        reply[0] = 0; // the setup failed
        assert!(setup_reply_range(&reply, ByteOrder::LittleEndian).is_none());
        assert!(setup_reply_len(&reply, ByteOrder::LittleEndian).is_none());
    }

    #[test]
    fn the_length_of_a_setup_reply_is_measured() {
        let reply = setup_reply_bytes(0x2000000, 0x1fffff);
        let len = setup_reply_len(&reply, ByteOrder::LittleEndian).expect("a length");
        assert_eq!(len, reply.len(), "the measured length is the whole reply");
        // A reply cut short before the fixed part has no length yet.
        assert!(setup_reply_len(&reply[..4], ByteOrder::LittleEndian).is_none());
    }

    #[test]
    fn a_split_setup_request_is_recognized_once_it_is_whole() {
        let request = setup_request_bytes(COOKIE_NAME, &[0x01; 16]);
        let mut watcher = SetupRequestWatcher::new();
        // The first piece alone is not the request.
        assert!(watcher.observe(&request[..10]).is_none());
        assert!(watcher.order().is_none());
        // The rest completes it.
        let auth = watcher.observe(&request[10..]).expect("the authorization");
        assert_eq!(auth.name, COOKIE_NAME);
        assert_eq!(watcher.order(), Some(ByteOrder::LittleEndian));
        // A later chunk is not part of the request.
        assert!(watcher.observe(&[0xFF; 32]).is_none());
    }

    #[test]
    fn a_split_setup_reply_is_recognized_once_it_is_whole() {
        let reply = setup_reply_bytes(0x2000000, 0x1fffff);
        let mut watcher = SetupReplyWatcher::new();
        assert!(watcher
            .observe(&reply[..6], ByteOrder::LittleEndian)
            .is_none());
        let (base, mask) = watcher
            .observe(&reply[6..], ByteOrder::LittleEndian)
            .expect("a range");
        assert_eq!(base, 0x2000000);
        assert_eq!(mask, 0x1fffff);
        // A later chunk is not part of the reply.
        assert!(watcher
            .observe(&[0xFF; 32], ByteOrder::LittleEndian)
            .is_none());
    }

    #[test]
    fn the_byte_order_of_a_request_is_read_from_its_first_byte() {
        assert_eq!(
            ByteOrder::from_request(b"l......"),
            Some(ByteOrder::LittleEndian)
        );
        assert_eq!(
            ByteOrder::from_request(b"B......"),
            Some(ByteOrder::BigEndian)
        );
        assert_eq!(ByteOrder::from_request(b"?......"), None);
    }

    /// The first bytes of a REAL setup reply, captured from the local X
    /// server of this machine. The fixed part carries the resource range
    /// this connection owns.
    ///
    /// ```text
    /// 01 00 0b 00 00 00 0b 0a   success, protocol 11.0, length 2571 units
    /// 76 39 bd 00               release number
    /// 00 00 80 00               resource id base   = 0x00800000
    /// ff ff 1f 00               resource id mask   = 0x001fffff
    /// 00 00 01 00               motion buffer size
    /// 00 14 00 ff               vendor length, maximum request length
    /// ```
    const REAL_SETUP_REPLY_HEAD: [u8; 24] = [
        0x01, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x0b, 0x0a, 0x76, 0x39, 0xbd, 0x00, 0x00, 0x00, 0x80,
        0x00, 0xff, 0xff, 0x1f, 0x00, 0x00, 0x00, 0x01, 0x00,
    ];

    #[test]
    fn the_resource_range_of_a_real_setup_reply_is_read_right() {
        let (base, mask) =
            setup_reply_range(&REAL_SETUP_REPLY_HEAD, ByteOrder::LittleEndian).expect("a range");
        assert_eq!(base, 0x00800000);
        assert_eq!(mask, 0x001fffff);
    }

    #[test]
    fn the_length_of_a_real_setup_reply_is_measured_right() {
        // The real reply's length field counts 2571 units of four bytes
        // after the eight-byte prefix: 10292 bytes in all.
        let len =
            setup_reply_len(&REAL_SETUP_REPLY_HEAD, ByteOrder::LittleEndian).expect("a length");
        assert_eq!(len, 8 + 2571 * 4);
        assert_eq!(len, 10292, "the real reply's measured length");
    }
}
