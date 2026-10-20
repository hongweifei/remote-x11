//! Async TCP transport: buffers bytes until complete frames can be decoded,
//! resynchronizing on the magic bytes after protocol errors.

use bytes::{Buf, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

use crate::config::BufferDefaults;
use crate::error::{Result, Rx11Error};
use crate::protocol::{decode_frame, encode_frame, Frame, MAGIC_BYTES, MAX_FRAME_SIZE};

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
        if read_buf.len() >= crate::protocol::FRAME_HEADER_SIZE {
            match decode_frame(read_buf) {
                Ok(Some((frame, consumed))) => {
                    read_buf.advance(consumed);
                    return Ok(frame);
                }
                Ok(None) => {}
                Err(_) => {
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

pub struct Rx11Transport {
    read_buf: BytesMut,
    stream: TcpStream,
}

impl Rx11Transport {
    pub fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            read_buf: BytesMut::with_capacity(BufferDefaults::INITIAL_READ_BUF),
            stream,
        })
    }

    pub async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let data = encode_frame(frame)?;
        self.stream.write_all(&data).await.map_err(Rx11Error::Io)?;
        self.stream.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn recv_frame(&mut self) -> Result<Frame> {
        recv_frame_inner(&mut self.read_buf, &mut self.stream).await
    }

    pub fn split(self) -> (Rx11TransportReadHalf, Rx11TransportWriteHalf) {
        let (read_half, write_half) = tokio::io::split(self.stream);
        (
            Rx11TransportReadHalf {
                read_buf: self.read_buf,
                read_half,
            },
            Rx11TransportWriteHalf { write_half },
        )
    }
}

pub struct Rx11TransportReadHalf {
    read_buf: BytesMut,
    read_half: tokio::io::ReadHalf<TcpStream>,
}

impl Rx11TransportReadHalf {
    pub async fn recv_frame(&mut self) -> Result<Frame> {
        recv_frame_inner(&mut self.read_buf, &mut self.read_half).await
    }
}

pub struct Rx11TransportWriteHalf {
    write_half: tokio::io::WriteHalf<TcpStream>,
}

impl Rx11TransportWriteHalf {
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let data = encode_frame(frame)?;
        self.write_half
            .write_all(&data)
            .await
            .map_err(Rx11Error::Io)?;
        self.write_half.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.write_half.flush().await.map_err(Rx11Error::Io)
    }
}
