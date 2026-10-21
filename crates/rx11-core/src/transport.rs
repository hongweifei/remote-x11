//! Async TCP transport: buffers bytes until complete frames can be decoded,
//! resynchronizing on the magic bytes after protocol errors.

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

use crate::config::BufferDefaults;
use crate::error::{Result, Rx11Error};
use crate::protocol::{
    decode_frame_owned, peek_frame, Frame, FRAME_HEADER_SIZE, MAGIC_BYTES, MAX_FRAME_SIZE,
};

/// Absolute ceiling for the frame accumulation buffer; protects against a
/// peer that never sends a complete frame. Distinct from
/// [`BufferDefaults::PROXY_READ_BUF_MAX`], which bounds the raw X11 socket
/// read buffers in the proxy loops.
const READ_BUF_HARD_LIMIT: usize = MAX_FRAME_SIZE + 256;

/// Reserve hint for each incremental read into the accumulation buffer.
const READ_RESERVE: usize = 8192;

fn scan_for_magic(data: &[u8]) -> usize {
    if data.len() <= 4 {
        return 0;
    }
    for i in 1..data.len() - 3 {
        if data[i..i + 4] == MAGIC_BYTES {
            return i;
        }
    }
    0
}

async fn recv_frame_inner<R: AsyncReadExt + Unpin>(
    read_buf: &mut BytesMut,
    reader: &mut R,
) -> Result<Frame> {
    loop {
        match peek_frame(read_buf) {
            Ok(Some((msg_type, total))) if read_buf.len() >= total => {
                // Complete frame: alias the payload out of the accumulation
                // buffer (no memcpy) and hand back the remaining tail.
                let mut front = read_buf.split_to(total).freeze();
                front.advance(FRAME_HEADER_SIZE);
                // A payload that fails to parse inside a length-valid frame
                // means the peer broke the protocol — X11 is a strict byte
                // stream, so silently skipping the frame would corrupt it
                // forever. Fail the connection; session-resume reconnects.
                return decode_frame_owned(msg_type, front);
            }
            Ok(_) => {}
            Err(_) => {
                // Corrupt header: hunt for the next magic to resynchronize.
                let skipped = scan_for_magic(read_buf);
                if skipped == 0 {
                    return Err(Rx11Error::Protocol(
                        "Invalid frame and no recovery possible".into(),
                    ));
                }
                warn!("Skipped {} bytes to resync frame boundary", skipped);
                read_buf.advance(skipped);
                continue;
            }
        }

        if read_buf.len() > READ_BUF_HARD_LIMIT {
            return Err(Rx11Error::Protocol(
                "read buffer exceeded maximum size".into(),
            ));
        }

        read_buf.reserve(READ_RESERVE);
        let n = reader.read_buf(read_buf).await.map_err(Rx11Error::Io)?;
        if n == 0 {
            return Err(Rx11Error::ConnectionClosed);
        }
    }
}

/// Framed transport over any duplex byte stream. `S` defaults to `TcpStream`
/// but supports in-memory duplexes (tests) and, in the future, TLS or unix
/// sockets without touching protocol logic.
pub struct Rx11Transport<S = TcpStream> {
    read_buf: BytesMut,
    scratch: BytesMut,
    stream: S,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Rx11Transport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            read_buf: BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF),
            scratch: BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF),
            stream,
        }
    }

    pub async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        self.scratch.clear();
        frame.encode_into(&mut self.scratch)?;
        self.stream
            .write_all(&self.scratch)
            .await
            .map_err(Rx11Error::Io)?;
        self.stream.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn recv_frame(&mut self) -> Result<Frame> {
        recv_frame_inner(&mut self.read_buf, &mut self.stream).await
    }

    pub fn split(self) -> (Rx11TransportReadHalf<S>, Rx11TransportWriteHalf<S>) {
        let (read_half, write_half) = tokio::io::split(self.stream);
        (
            Rx11TransportReadHalf {
                read_buf: self.read_buf,
                read_half,
            },
            Rx11TransportWriteHalf {
                write_half,
                scratch: self.scratch,
            },
        )
    }
}

impl Rx11Transport<TcpStream> {
    /// Wrap an accepted/connected TCP stream with the socket options the
    /// relay depends on: no Nagle, plus an optional send-buffer cap (a
    /// kernel-side queue the size of the link's patience is the last
    /// bufferbloat lever on slow WANs; `None` keeps the OS default).
    pub fn connect_tcp(stream: TcpStream, send_buffer_bytes: Option<usize>) -> Result<Self> {
        stream.set_nodelay(true)?;
        if let Some(size) = send_buffer_bytes {
            // tokio only exposes buffer sizing on pre-connect TcpSocket;
            // socket2's SockRef borrows the live fd without taking it.
            let sock = socket2::SockRef::from(&stream);
            if let Err(e) = sock.set_send_buffer_size(size) {
                // Tuning nicety, never fatal: keep the connection going.
                warn!("set_send_buffer_size({size}) failed: {e}");
            }
        }
        Ok(Self::new(stream))
    }
}

pub struct Rx11TransportReadHalf<S = TcpStream> {
    read_buf: BytesMut,
    read_half: tokio::io::ReadHalf<S>,
}

impl<S: AsyncRead + Unpin> Rx11TransportReadHalf<S> {
    pub async fn recv_frame(&mut self) -> Result<Frame> {
        recv_frame_inner(&mut self.read_buf, &mut self.read_half).await
    }
}

pub struct Rx11TransportWriteHalf<S = TcpStream> {
    write_half: tokio::io::WriteHalf<S>,
    scratch: BytesMut,
}

impl<S: AsyncWrite + Unpin> Rx11TransportWriteHalf<S> {
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        self.scratch.clear();
        frame.encode_into(&mut self.scratch)?;
        self.write_half
            .write_all(&self.scratch)
            .await
            .map_err(Rx11Error::Io)?;
        self.write_half.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    /// Push pre-encoded bytes (a batch built with `Frame::encode_into`)
    /// as one write.
    pub async fn send_raw(&mut self, data: &[u8]) -> Result<()> {
        self.write_half
            .write_all(data)
            .await
            .map_err(Rx11Error::Io)?;
        self.write_half.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.write_half.flush().await.map_err(Rx11Error::Io)
    }
}

/// Drive a writer half from the byte-budgeted outbound queue, COALESCING
/// frames: when a frame arrives, drain whatever is already pending (bounded
/// by frame count and batch bytes) into one buffer and emit with a single
/// write. A quiescent queue keeps per-frame latency (the blocking `recv()`
/// returns immediately); a saturated queue gets batch syscalls instead of
/// one write(2) per frame. Popping a frame releases its share of the byte
/// budget, unblocking gated producers.
///
/// The real-time (priority) lane preempts bulk traffic at every scheduling
/// point: between batches (`recv_any` polls it first), mid-batch (the
/// coalescer stops growing when a packet is parked) and between socket
/// commit chunks inside one batch write. A real-time packet's queueing delay
/// is therefore bounded by one [`BufferDefaults::OUTBOUND_WRITE_CHUNK`] of
/// committed bulk bytes — whatever the kernel socket buffer already holds
/// is a physical ordering limit no queue layout can preempt. At ~50
/// packets/s the lane never starves bulk either.
pub async fn drain_frames<S: AsyncWrite + Unpin>(
    mut rx: crate::queue::OutboundReceiver,
    mut write_half: Rx11TransportWriteHalf<S>,
) {
    use crate::queue::LaneItem;
    let mut batch = BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF);
    loop {
        let first = match rx.recv_any().await {
            LaneItem::Priority(frame) => {
                batch.clear();
                let failed = frame.encode_into(&mut batch).is_err();
                if write_half.send_raw(&batch).await.is_err() || failed {
                    break;
                }
                continue;
            }
            LaneItem::Bulk(frame) => frame,
            LaneItem::BulkClosed => break,
        };
        {
            batch.clear();
            let mut failed = first.encode_into(&mut batch).is_err();
            let mut queued = 1usize;
            while !failed
                && queued < BufferDefaults::OUTBOUND_BATCH_FRAMES
                && batch.len() < BufferDefaults::OUTBOUND_BATCH_BYTES
            {
                if rx.priority_pending() {
                    break;
                }
                match rx.try_recv() {
                    Ok((next, _)) => {
                        if next.encode_into(&mut batch).is_err() {
                            failed = true;
                            break;
                        }
                        queued += 1;
                    }
                    Err(_) => break,
                }
            }
            // Freeze the coalesced bytes; `batch` becomes scratch for
            // priority frames interleaved between commit chunks.
            let committed = batch.split().freeze();
            // Flush whatever was coalesced before reacting to an encode
            // failure: bytes already accepted downstream must not be
            // withheld.
            let mut write_result = Ok(());
            for chunk in committed.chunks(BufferDefaults::OUTBOUND_WRITE_CHUNK) {
                while let Some(p) = rx.try_recv_priority() {
                    batch.clear();
                    if p.encode_into(&mut batch).is_err() {
                        failed = true;
                        break;
                    }
                    if write_half.send_raw(&batch).await.is_err() {
                        write_result = Err(Rx11Error::ConnectionClosed);
                        break;
                    }
                }
                if failed || write_result.is_err() {
                    break;
                }
                write_result = write_half.send_raw(chunk).await;
                if write_result.is_err() {
                    break;
                }
            }
            if write_result.is_err() || failed {
                // A frame that cannot be encoded is a protocol bug on
                // OUR side; dropping it would silently corrupt the
                // strict X11 byte stream, so tear the connection
                // instead — resume/rebuild is the established recovery
                // path.
                break;
            }
        }
    }
    let _ = write_half.flush().await;
}
