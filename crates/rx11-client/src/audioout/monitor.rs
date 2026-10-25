//! The monitor connection: asks the local X server which window has the
//! input focus.
//!
//! The question must not be asked on an application connection. X11
//! matches replies to requests by sequence number, and every connection
//! has its own counter; asking on the application's connection would
//! advance that counter and the application would see a sequence error.
//!
//! So the client opens a connection of its own and asks there. The focus
//! is the server's global state, so any connection may ask it. The
//! monitor connection needs the local server's authorization, which the
//! client's byte stream already carries: the application's setup request
//! names the authorization protocol and its data.
//!
//! The monitor's own resource range is deliberately NOT registered: it
//! owns no application window, and a window id in its range must not be
//! traced to an application.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

/// How long to wait for the focus answer before giving up. The answer is
/// a single reply; a local server answers immediately.
const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);

/// The authorization a connection must present to the local X server.
///
/// The bytes are taken verbatim from the application's setup request: the
/// authorization protocol name and its data, exactly as the server will
/// read them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Authorization {
    /// The authorization protocol name (e.g. `MIT-MAGIC-COOKIE-1`).
    pub name: Vec<u8>,
    /// The authorization data.
    pub data: Vec<u8>,
}

impl Authorization {
    /// Whether there is anything to present.
    pub(crate) fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Read the authorization out of an X11 setup request.
    ///
    /// The setup request is:
    ///
    /// ```text
    ///  1  byte-order ('B' or 'l')
    ///  1  unused
    ///  2  protocol-major
    ///  2  protocol-minor
    ///  2  authorization-protocol-name length
    ///  2  authorization-protocol-data length
    ///  1  unused
    ///  *  padded name
    ///  *  padded data
    /// ```
    pub(crate) fn from_setup_request(request: &[u8]) -> Option<Self> {
        const HEADER: usize = 12;
        if request.len() < HEADER {
            return None;
        }
        let big_endian = match request[0] {
            b'B' => true,
            b'l' => false,
            _ => return None,
        };
        let name_len = u16_at(request, 6, big_endian) as usize;
        let data_len = u16_at(request, 8, big_endian) as usize;
        let padded_name = name_len.next_multiple_of(4);
        let padded_data = data_len.next_multiple_of(4);
        let end = HEADER + padded_name + padded_data;
        if request.len() < end {
            return None;
        }
        let name = request[HEADER..HEADER + name_len].to_vec();
        let data_start = HEADER + padded_name;
        let data = request[data_start..data_start + data_len].to_vec();
        Some(Self { name, data })
    }

    /// The setup request for a connection that presents this
    /// authorization.
    pub(crate) fn setup_request(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.name.len() + self.data.len());
        out.push(b'l'); // little-endian
        out.push(0); // unused
        out.extend_from_slice(&11u16.to_le_bytes()); // protocol major
        out.extend_from_slice(&0u16.to_le_bytes()); // protocol minor
        out.extend_from_slice(&(self.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8, 0]); // the trailing unused pair
        out.extend_from_slice(&self.name);
        let name_pad = self.name.len().next_multiple_of(4) - self.name.len();
        out.extend(std::iter::repeat_n(0u8, name_pad));
        out.extend_from_slice(&self.data);
        let data_pad = self.data.len().next_multiple_of(4) - self.data.len();
        out.extend(std::iter::repeat_n(0u8, data_pad));
        out
    }
}

/// Read a 16-bit field in the connection's byte order.
fn u16_at(bytes: &[u8], at: usize, big_endian: bool) -> u16 {
    let pair = [bytes[at], bytes[at + 1]];
    if big_endian {
        u16::from_be_bytes(pair)
    } else {
        u16::from_le_bytes(pair)
    }
}

/// Ask the local X server which window has the input focus.
///
/// Returns the window id, or `None` when the server does not answer or
/// answers with something that is not a reply.
pub(crate) async fn ask_focus(address: &str, authorization: &Authorization) -> Option<u32> {
    let mut stream = TcpStream::connect(address).await.ok()?;
    stream.set_nodelay(true).ok()?;
    stream
        .write_all(&authorization.setup_request())
        .await
        .ok()?;
    stream.flush().await.ok()?;

    // The setup reply: 8 bytes of prefix, then the fixed part, then the
    // variable part. The fixed part carries the resource range, which
    // the monitor does not need — only the success byte matters.
    let mut head = [0u8; 8];
    tokio::time::timeout(FOCUS_TIMEOUT, stream.read_exact(&mut head))
        .await
        .ok()?
        .ok()?;
    if head[0] != 1 {
        return None; // the setup was refused
    }
    let extra = u16::from_le_bytes([head[6], head[7]]) as usize * 4;
    if extra > 0 {
        let mut skip = vec![0u8; extra];
        tokio::time::timeout(FOCUS_TIMEOUT, stream.read_exact(&mut skip))
            .await
            .ok()?
            .ok()?;
    }

    // GetInputFocus: opcode 43, length 1 (in 4-byte units).
    stream.write_all(&[43, 0, 1, 0]).await.ok()?;
    stream.flush().await.ok()?;

    // The reply: 32 bytes plus 4 per length unit. The focused window id
    // sits at bytes 8..12.
    let mut reply = [0u8; 32];
    tokio::time::timeout(FOCUS_TIMEOUT, stream.read_exact(&mut reply))
        .await
        .ok()?
        .ok()?;
    if reply[0] != 1 {
        return None; // an error packet
    }
    Some(u32::from_le_bytes([
        reply[8], reply[9], reply[10], reply[11],
    ]))
}

/// The focus of the local X server, or `None` when it cannot be read.
pub(crate) async fn focus_window(address: &str, authorization: &Authorization) -> Option<u32> {
    if authorization.is_empty() {
        debug!("audio: no authorization for the local X server; focus unknown");
        return None;
    }
    match ask_focus(address, authorization).await {
        Some(focus) => Some(focus),
        None => {
            debug!("audio: the local X server did not answer the focus query");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_request(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = vec![b'l', 0];
        out.extend_from_slice(&11u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8, 0]); // the trailing unused pair
        out.extend_from_slice(name);
        // The padding brings the name to a multiple of four.
        let name_padded = name.len().next_multiple_of(4);
        out.resize(out.len() + (name_padded - name.len()), 0);
        out.extend_from_slice(data);
        let data_padded = data.len().next_multiple_of(4);
        out.resize(out.len() + (data_padded - data.len()), 0);
        out
    }

    /// The setup request of the real X clients: a name whose length is
    /// not a multiple of four, so the padding is exercised.
    const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

    #[test]
    fn the_authorization_is_read_out_of_the_setup_request() {
        let request = setup_request(COOKIE_NAME, &[0xAB; 16]);
        let auth = Authorization::from_setup_request(&request).expect("an authorization");
        assert_eq!(auth.name, COOKIE_NAME);
        assert_eq!(auth.data.len(), 16);
        assert_eq!(auth.data[0], 0xAB);
    }

    #[test]
    fn a_setup_request_without_authorization_yields_an_empty_one() {
        let request = setup_request(b"", b"");
        let auth = Authorization::from_setup_request(&request).expect("parsed");
        assert!(auth.is_empty());
    }

    #[test]
    fn a_truncated_setup_request_yields_none() {
        assert!(Authorization::from_setup_request(&[b'l', 0, 11]).is_none());
    }

    #[test]
    fn the_setup_request_round_trips_through_the_authorization() {
        let original = setup_request(COOKIE_NAME, &[0x01; 16]);
        let auth = Authorization::from_setup_request(&original).expect("an authorization");
        let rebuilt = auth.setup_request();
        assert_eq!(rebuilt, original, "the rebuilt request is byte-identical");
    }

    #[test]
    fn a_big_endian_setup_request_is_read_too() {
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
}
