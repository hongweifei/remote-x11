use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::warn;

use crate::error::Rx11Error;
use crate::protocol::{decode_frame, encode_frame, MAGIC_BYTES, MAX_FRAME_SIZE, Frame};

const MAX_READ_BUF: usize = MAX_FRAME_SIZE + 256;

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

pub struct Rx11Transport {
    read_buf: BytesMut,
    stream: TcpStream,
}

impl Rx11Transport {
    pub fn new(stream: TcpStream) -> crate::error::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            read_buf: BytesMut::with_capacity(64 * 1024),
            stream,
        })
    }

    pub async fn send_frame(&mut self, frame: &Frame) -> crate::error::Result<()> {
        let data = encode_frame(frame)?;
        self.stream
            .write_all(&data)
            .await
            .map_err(Rx11Error::Io)?;
        self.stream.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn recv_frame(&mut self) -> crate::error::Result<Frame> {
        loop {
            if self.read_buf.len() >= 9 {
                match decode_frame(&self.read_buf) {
                    Ok(Some((frame, consumed))) => {
                        let _ = self.read_buf.split_to(consumed);
                        return Ok(frame);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let skipped = scan_for_magic(&self.read_buf);
                        if skipped == 0 {
                            return Err(Rx11Error::Protocol(
                                "Invalid frame and no recovery possible".into(),
                            ));
                        }
                        warn!("Skipped {} bytes to resync frame boundary", skipped);
                        let _ = self.read_buf.split_to(skipped);
                        continue;
                    }
                }
            }

            if self.read_buf.len() > MAX_READ_BUF {
                return Err(Rx11Error::Protocol(
                    "read buffer exceeded maximum size".into(),
                ));
            }

            self.read_buf.reserve(8192);
            let n = self
                .stream
                .read_buf(&mut self.read_buf)
                .await
                .map_err(Rx11Error::Io)?;
            if n == 0 {
                return Err(Rx11Error::ConnectionClosed);
            }
        }
    }

    pub fn split(
        self,
    ) -> (
        Rx11TransportReadHalf,
        Rx11TransportWriteHalf,
    ) {
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
    pub async fn recv_frame(&mut self) -> crate::error::Result<Frame> {
        loop {
            if self.read_buf.len() >= 9 {
                match decode_frame(&self.read_buf) {
                    Ok(Some((frame, consumed))) => {
                        let _ = self.read_buf.split_to(consumed);
                        return Ok(frame);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        let skipped = scan_for_magic(&self.read_buf);
                        if skipped == 0 {
                            return Err(Rx11Error::Protocol(
                                "Invalid frame and no recovery possible".into(),
                            ));
                        }
                        warn!("Skipped {} bytes to resync frame boundary", skipped);
                        let _ = self.read_buf.split_to(skipped);
                        continue;
                    }
                }
            }

            if self.read_buf.len() > MAX_READ_BUF {
                return Err(Rx11Error::Protocol(
                    "read buffer exceeded maximum size".into(),
                ));
            }

            self.read_buf.reserve(8192);
            let n = self
                .read_half
                .read_buf(&mut self.read_buf)
                .await
                .map_err(Rx11Error::Io)?;
            if n == 0 {
                return Err(Rx11Error::ConnectionClosed);
            }
        }
    }
}

pub struct Rx11TransportWriteHalf {
    write_half: tokio::io::WriteHalf<TcpStream>,
}

impl Rx11TransportWriteHalf {
    pub async fn send_frame(&mut self, frame: &Frame) -> crate::error::Result<()> {
        let data = encode_frame(frame)?;
        self.write_half
            .write_all(&data)
            .await
            .map_err(Rx11Error::Io)?;
        self.write_half.flush().await.map_err(Rx11Error::Io)?;
        Ok(())
    }

    pub async fn flush(&mut self) -> crate::error::Result<()> {
        self.write_half.flush().await.map_err(Rx11Error::Io)
    }
}
