//! Connection establishment: `Hello` / `HelloAck` / `AuthRequest` /
//! `AuthResponse` negotiation, shared by the relay server and the client.

use std::time::Duration;

use crate::auth;
use crate::compress::CompressionAlgo;
use crate::error::{Result, Rx11Error};
use crate::protocol::{
    AuthRequestMessage, AuthResponseMessage, ConnectionMode, Frame, HelloAckMessage, HelloMessage,
    PROTOCOL_VERSION,
};
use crate::transport::Rx11Transport;
use crate::types::{SessionId, Token};

pub struct HandshakeResult {
    pub session_id: SessionId,
    pub compression: Option<CompressionAlgo>,
    pub resume_session_id: Option<SessionId>,
}

/// Placeholder session id for handshake failures, when no session exists yet.
fn placeholder_session_id() -> SessionId {
    SessionId::new("rx11-invalid".to_string()).expect("static placeholder id is valid")
}

fn failed_hello_ack(error_msg: String) -> Frame {
    Frame::HelloAck(HelloAckMessage {
        version: PROTOCOL_VERSION,
        session_id: placeholder_session_id(),
        success: false,
        error_msg: Some(error_msg),
        compression: None,
    })
}

/// Server side of the handshake: validate Hello, negotiate compression, then
/// verify the client's auth token.
pub async fn server_handshake(
    transport: &mut Rx11Transport,
    auth_token: &str,
    timeout: Duration,
) -> Result<HandshakeResult> {
    let hello_frame = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for Hello".into()))?;

    let hello = match hello_frame {
        Frame::Hello(h) => h,
        _ => return Err(Rx11Error::Protocol("Expected Hello frame".into())),
    };

    if hello.version != PROTOCOL_VERSION {
        let msg = format!(
            "Version mismatch: got {} expected {}",
            hello.version, PROTOCOL_VERSION
        );
        transport.send_frame(&failed_hello_ack(msg.clone())).await?;
        return Err(Rx11Error::Protocol(msg));
    }

    if !matches!(hello.mode, ConnectionMode::Client) {
        transport
            .send_frame(&failed_hello_ack("Expected Client mode".into()))
            .await?;
        return Err(Rx11Error::Protocol("Expected Client mode".into()));
    }

    let compression = CompressionAlgo::negotiate(&hello.compression_algos, &CompressionAlgo::ALL);
    let session_id = SessionId::new(uuid::Uuid::new_v4().to_string())?;

    if let Some(ref sid) = hello.resume_session_id {
        tracing::info!("Client requests session resume: {}", sid);
    }

    transport
        .send_frame(&Frame::HelloAck(HelloAckMessage {
            version: PROTOCOL_VERSION,
            session_id: session_id.clone(),
            success: true,
            error_msg: None,
            compression,
        }))
        .await?;

    let auth_frame = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for AuthRequest".into()))?;

    match auth_frame {
        Frame::AuthRequest(auth_req) => {
            if let Err(e) = Token::new(auth_req.token.0.clone()) {
                send_auth_response(transport, false, Some(format!("Invalid token: {}", e))).await?;
                return Err(Rx11Error::Auth("Invalid token format".into()));
            }
            if !auth::verify_token(auth_req.token.as_str(), auth_token) {
                send_auth_response(transport, false, Some("Invalid token".into())).await?;
                return Err(Rx11Error::Auth("Token mismatch".into()));
            }
            send_auth_response(transport, true, None).await?;
        }
        _ => return Err(Rx11Error::Protocol("Expected AuthRequest frame".into())),
    }

    Ok(HandshakeResult {
        session_id,
        compression,
        resume_session_id: hello.resume_session_id,
    })
}

async fn send_auth_response(
    transport: &mut Rx11Transport,
    success: bool,
    error_msg: Option<String>,
) -> Result<()> {
    transport
        .send_frame(&Frame::AuthResponse(AuthResponseMessage {
            success,
            error_msg,
        }))
        .await
}

/// Client side of the handshake: send Hello with our capabilities, then the
/// auth token, validating each server response along the way.
pub async fn client_handshake(
    transport: &mut Rx11Transport,
    auth_token: &Token,
    resume_session_id: Option<&SessionId>,
    timeout: Duration,
) -> Result<HandshakeResult> {
    transport
        .send_frame(&Frame::Hello(HelloMessage {
            version: PROTOCOL_VERSION,
            mode: ConnectionMode::Client,
            resume_session_id: resume_session_id.cloned(),
            compression_algos: CompressionAlgo::ALL.to_vec(),
        }))
        .await?;

    let ack = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for HelloAck".into()))?;

    let hello_ack = match ack {
        Frame::HelloAck(h) => h,
        _ => return Err(Rx11Error::Protocol("Expected HelloAck frame".into())),
    };

    if !hello_ack.success {
        return Err(Rx11Error::Protocol(format!(
            "Handshake failed: {}",
            hello_ack.error_msg.as_deref().unwrap_or("unknown error")
        )));
    }

    if hello_ack.version != PROTOCOL_VERSION {
        return Err(Rx11Error::Protocol(format!(
            "Protocol version mismatch: server {} client {}",
            hello_ack.version, PROTOCOL_VERSION
        )));
    }

    transport
        .send_frame(&Frame::AuthRequest(AuthRequestMessage {
            token: auth_token.clone(),
        }))
        .await?;

    let auth_resp = tokio::time::timeout(timeout, transport.recv_frame())
        .await
        .map_err(|_| Rx11Error::Timeout)?
        .map_err(|_| Rx11Error::Protocol("Handshake timed out waiting for AuthResponse".into()))?;

    match auth_resp {
        Frame::AuthResponse(resp) => {
            if !resp.success {
                return Err(Rx11Error::Auth(format!(
                    "Auth failed: {}",
                    resp.error_msg.as_deref().unwrap_or("unknown error")
                )));
            }
        }
        _ => return Err(Rx11Error::Protocol("Expected AuthResponse frame".into())),
    }

    Ok(HandshakeResult {
        session_id: hello_ack.session_id,
        compression: hello_ack.compression,
        resume_session_id: resume_session_id.cloned(),
    })
}
