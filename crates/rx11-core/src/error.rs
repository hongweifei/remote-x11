use std::io::ErrorKind;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Rx11Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Timeout")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, Rx11Error>;

impl Rx11Error {
    pub fn is_retriable(&self) -> bool {
        match self {
            Rx11Error::Io(e) => matches!(
                e.kind(),
                ErrorKind::ConnectionRefused
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::Interrupted
                    | ErrorKind::UnexpectedEof
            ),
            Rx11Error::Timeout | Rx11Error::ConnectionClosed => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_refused_is_retriable() {
        let err = Rx11Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(err.is_retriable());
    }

    #[test]
    fn test_connection_reset_is_retriable() {
        let err = Rx11Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(err.is_retriable());
    }

    #[test]
    fn test_broken_pipe_is_retriable() {
        let err = Rx11Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe",
        ));
        assert!(err.is_retriable());
    }

    #[test]
    fn test_timed_out_is_retriable() {
        let err = Rx11Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ));
        assert!(err.is_retriable());
    }

    #[test]
    fn test_timeout_is_retriable() {
        assert!(Rx11Error::Timeout.is_retriable());
    }

    #[test]
    fn test_connection_closed_is_retriable() {
        assert!(Rx11Error::ConnectionClosed.is_retriable());
    }

    #[test]
    fn test_permission_denied_is_not_retriable() {
        let err = Rx11Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert!(!err.is_retriable());
    }

    #[test]
    fn test_protocol_is_not_retriable() {
        assert!(!Rx11Error::Protocol("bad".into()).is_retriable());
    }

    #[test]
    fn test_auth_is_not_retriable() {
        assert!(!Rx11Error::Auth("bad".into()).is_retriable());
    }
}
