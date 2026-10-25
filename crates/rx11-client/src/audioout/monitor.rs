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
//! The monitor connection RESIDES. It is opened once and asked again and
//! again; a broken one is replaced. Opening a connection per question
//! would be wrong twice over: every connection takes a resource range
//! from the server (ranges are only recycled slowly), and a connection
//! every poll interval is a storm against the local X server.
//!
//! The monitor's own resource range is deliberately NOT registered: it
//! owns no application window, and a window id in its range must not be
//! traced to an application.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tracing::debug;

use x11rb_protocol::connect::Connect;
use x11rb_protocol::connection::{Connection, ReplyFdKind};
use x11rb_protocol::packet_reader::PacketReader;
use x11rb_protocol::protocol::xproto::{GetInputFocusReply, GetInputFocusRequest};
use x11rb_protocol::x11_utils::TryParse;
use x11rb_protocol::SequenceNumber;

use super::x11wire::Authorization;

/// How long to wait for the server's answer before giving up. The answer
/// is a single reply; a local server answers immediately.
const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);

/// The stream to the local X server.
///
/// The X server is reachable in two ways: over TCP (the form the
/// product is configured with) and over a unix socket (the form the
/// server itself opens). Both are regular entrances, so both are
/// supported: the address is a socket path when it starts with a slash,
/// and a `host:port` pair otherwise.
enum XStream {
    /// The server over TCP.
    Tcp(TcpStream),
    /// The server over a unix socket.
    Unix(UnixStream),
}

impl XStream {
    /// Connect to the X server at this address.
    async fn connect(address: &str) -> std::io::Result<Self> {
        if address.starts_with('/') {
            Ok(Self::Unix(UnixStream::connect(address).await?))
        } else {
            let stream = TcpStream::connect(address).await?;
            stream.set_nodelay(true)?;
            Ok(Self::Tcp(stream))
        }
    }
}

impl AsyncRead for XStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for XStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// The monitor connection to the local X server.
///
/// It holds the protocol state of one connection: the sequence-number
/// bookkeeping and the framing of the server's replies. The connection
/// itself lives in the task that owns it.
pub(crate) struct Monitor {
    /// The stream to the local X server.
    stream: XStream,
    /// The protocol state: which replies are still outstanding.
    connection: Connection,
    /// The framing of the server's stream: a reply is 32 bytes plus four
    /// per length unit, and events may arrive in between.
    reader: PacketReader,
}

impl Monitor {
    /// Open a monitor connection to the local X server.
    ///
    /// The authorization is the one the application's setup request named:
    /// the local server expects the same one from every client.
    pub(crate) async fn connect(
        address: &str,
        authorization: &Authorization,
    ) -> std::io::Result<Self> {
        let mut stream = XStream::connect(address).await?;

        // The library builds the setup request and knows its length; the
        // authorization is presented exactly as the application presents
        // it.
        let (connect, setup) =
            Connect::with_authorization(authorization.name.clone(), authorization.data.clone());
        stream.write_all(&setup).await?;
        stream.flush().await?;

        // The setup reply: the library reads it and hands back the
        // connection's resource range, which the monitor does not need.
        let reply = read_setup_reply(&mut stream, connect)
            .await
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the local X server did not answer the monitor's setup",
                )
            })?;
        debug!(
            "audio: monitor connection to the local X server established \
             (resource base {:#x}, mask {:#x})",
            reply.resource_id_base, reply.resource_id_mask
        );

        Ok(Self {
            stream,
            connection: Connection::new(),
            reader: PacketReader::new(),
        })
    }

    /// Ask which window has the input focus.
    ///
    /// `None` when the server did not answer, or answered with an error.
    /// The connection stays usable either way: a lost reply is not a lost
    /// connection.
    pub(crate) async fn ask_focus(&mut self) -> Option<u32> {
        // The library serializes the request and knows its length field
        // (GetInputFocus takes no arguments, so the length is one unit).
        let (bufs, _fds) = GetInputFocusRequest.serialize();
        let buf: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();

        // The sequence number is taken BEFORE the request goes on the
        // wire: the library counts the requests it is told about, and
        // the server's reply carries the number of the request it
        // answers. Registering the request after writing it would leave
        // the library's counter one behind the wire, and the reply would
        // never match.
        let sequence = self.connection.send_request(ReplyFdKind::ReplyWithoutFDs)?;

        self.stream.write_all(&buf).await.ok()?;
        self.stream.flush().await.ok()?;

        // The reply arrives among the server's events; the framing
        // separates them.
        let reply = tokio::time::timeout(FOCUS_TIMEOUT, self.read_reply(sequence))
            .await
            .ok()??;
        Some(reply.focus)
    }

    /// Read packets from the server until the reply arrives.
    async fn read_reply(&mut self, sequence: SequenceNumber) -> Option<GetInputFocusReply> {
        loop {
            let packet = self.read_packet().await?;
            self.connection.enqueue_packet(packet);
            match self.connection.poll_for_reply(sequence) {
                x11rb_protocol::connection::PollReply::Reply(reply) => {
                    let (reply, _) = GetInputFocusReply::try_parse(&reply).ok()?;
                    return Some(reply);
                }
                x11rb_protocol::connection::PollReply::NoReply => {
                    debug!("audio: the focus request was answered with an error");
                    return None;
                }
                x11rb_protocol::connection::PollReply::TryAgain => {}
            }
        }
    }

    /// Read one framed packet from the server.
    async fn read_packet(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.reader.buffer().is_empty() {
                debug!("audio: the monitor's read buffer is empty");
                return None;
            }
            let read = tokio::time::timeout(FOCUS_TIMEOUT, self.stream.read(self.reader.buffer()))
                .await
                .ok()?
                .ok()?;
            if read == 0 {
                debug!("audio: the local X server closed the monitor connection");
                return None; // the server closed the connection
            }
            if let Some(packet) = self.reader.advance(read) {
                return Some(packet);
            }
        }
    }
}

/// Read the setup reply and parse it with the protocol library.
async fn read_setup_reply(
    stream: &mut XStream,
    mut connect: Connect,
) -> Option<x11rb_protocol::protocol::xproto::Setup> {
    loop {
        let buffer = connect.buffer();
        if buffer.is_empty() {
            return None;
        }
        let read = tokio::time::timeout(FOCUS_TIMEOUT, stream.read(buffer))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        connect.advance(read);
        if connect.buffer().is_empty() {
            break;
        }
    }
    connect.into_setup().ok()
}

/// The resident monitor: one connection to the local X server, asked
/// again and again.
///
/// The connection is opened when it is first needed and kept. Opening one
/// per question would be wrong twice over: every connection takes a
/// resource range from the X server, and the ranges are recycled slowly,
/// so a connection per poll would wear them out; and a connection per
/// poll is a storm against the local X server.
pub(crate) struct ResidentMonitor {
    /// The address of the local X server.
    address: String,
    /// The authorization it expects.
    authorization: Authorization,
    /// The connection, while it is alive.
    connection: Option<Monitor>,
}

impl ResidentMonitor {
    /// A monitor for the local X server at this address.
    pub(crate) fn new(address: &str, authorization: Authorization) -> Self {
        Self {
            address: address.to_string(),
            authorization,
            connection: None,
        }
    }

    /// Replace the authorization, and drop the connection: the server may
    /// want a different one now.
    pub(crate) fn set_authorization(&mut self, authorization: Authorization) {
        if authorization != self.authorization {
            self.authorization = authorization;
            self.connection = None;
        }
    }

    /// Which window has the input focus.
    ///
    /// `None` when the server did not answer. The connection is replaced
    /// when it breaks, so the next question opens a fresh one.
    pub(crate) async fn focus_window(&mut self) -> Option<u32> {
        if self.authorization.is_empty() {
            // The authorization is learned from the application's setup
            // request. Without it the local X server may refuse the
            // connection, so there is nothing to ask.
            debug!("audio: no authorization for the local X server; focus unknown");
            return None;
        }
        if self.connection.is_none() {
            match Monitor::connect(&self.address, &self.authorization).await {
                Ok(connection) => self.connection = Some(connection),
                Err(e) => {
                    debug!("audio: cannot open the monitor connection: {e}");
                    return None;
                }
            }
        }
        let connection = self.connection.as_mut()?;
        match connection.ask_focus().await {
            Some(focus) => Some(focus),
            None => {
                // The connection is broken or the server did not answer.
                // Drop it: the next question opens a fresh one.
                self.connection = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setup_request_of_the_library_matches_the_authorization() {
        // The library builds the request; the authorization it carries is
        // the one that was handed in.
        let auth = Authorization {
            name: b"MIT-MAGIC-COOKIE-1".to_vec(),
            data: vec![0xAB; 16],
        };
        let (_connect, request) = x11rb_protocol::connect::Connect::with_authorization(
            auth.name.clone(),
            auth.data.clone(),
        );
        let parsed = Authorization::from_setup_request(&request).expect("an authorization");
        assert_eq!(parsed, auth, "the authorization survives the round trip");
    }

    #[test]
    fn the_focus_request_is_four_bytes_with_a_length_of_one() {
        // GetInputFocus takes no arguments, so its length field is one
        // four-byte unit. A length of two is refused by the server.
        let (bufs, _fds) = GetInputFocusRequest.serialize();
        let buf: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(buf.len(), 4);
        assert_eq!(buf[0], 43, "the opcode of GetInputFocus");
        assert_eq!(
            u16::from_le_bytes([buf[2], buf[3]]),
            1,
            "the length field counts one unit"
        );
    }
}
