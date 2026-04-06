use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub const MAX_DISPLAY_NUMBER: u16 = 255;
pub const MAX_TOKEN_LEN: usize = 256;
pub const MAX_SESSION_ID_LEN: usize = 256;
pub const MAX_AUTH_NAME_LEN: usize = 256;
pub const MAX_AUTH_DATA_LEN: usize = 4096;

macro_rules! impl_newtype {
    ($name:ident, $inner:ty, $max_len:expr, $err_prefix:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub $inner);

        impl $name {
            pub fn new(value: $inner) -> crate::error::Result<Self> {
                let s: &str = &value;
                if s.is_empty() || s.len() > $max_len {
                    return Err(crate::error::Rx11Error::Protocol(format!(
                        "{} length must be 1-{} bytes, got {}",
                        $err_prefix,
                        $max_len,
                        s.len()
                    )));
                }
                Ok(Self(value))
            }

            pub fn into_inner(self) -> $inner {
                self.0
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = crate::error::Rx11Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s.to_string())
            }
        }
    };
}

impl_newtype!(SessionId, String, MAX_SESSION_ID_LEN, "Session ID");
impl_newtype!(Token, String, MAX_TOKEN_LEN, "Token");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayNumber(pub u16);

impl DisplayNumber {
    pub fn new(disp: u16) -> crate::error::Result<Self> {
        if disp > MAX_DISPLAY_NUMBER {
            return Err(crate::error::Rx11Error::Protocol(format!(
                "Display number must be 0-{}, got {}",
                MAX_DISPLAY_NUMBER, disp
            )));
        }
        Ok(Self(disp))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for DisplayNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ":{}", self.0)
    }
}

impl FromStr for DisplayNumber {
    type Err = crate::error::Rx11Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let disp: u16 = s.parse().map_err(|_| {
            crate::error::Rx11Error::Protocol(format!("Invalid display number: {}", s))
        })?;
        Self::new(disp)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub u32);

impl ConnectionId {
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "conn#{}", self.0)
    }
}

pub fn validate_auth_fields(auth_name: &str, auth_data: &[u8]) -> crate::error::Result<()> {
    if auth_name.len() > MAX_AUTH_NAME_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "auth_name too long: {} bytes (max {})",
            auth_name.len(),
            MAX_AUTH_NAME_LEN
        )));
    }
    if auth_data.len() > MAX_AUTH_DATA_LEN {
        return Err(crate::error::Rx11Error::Protocol(format!(
            "auth_data too long: {} bytes (max {})",
            auth_data.len(),
            MAX_AUTH_DATA_LEN
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_number_valid() {
        assert_eq!(DisplayNumber::new(0).unwrap().get(), 0);
        assert_eq!(DisplayNumber::new(255).unwrap().get(), 255);
    }

    #[test]
    fn test_display_number_invalid() {
        assert!(DisplayNumber::new(256).is_err());
    }

    #[test]
    fn test_display_number_display() {
        assert_eq!(format!("{}", DisplayNumber::new(5).unwrap()), ":5");
    }

    #[test]
    fn test_connection_id() {
        let id = ConnectionId::new(42);
        assert_eq!(id.get(), 42);
        assert_eq!(format!("{}", id), "conn#42");
    }

    #[test]
    fn test_session_id_valid() {
        let sid = SessionId::new("abc".to_string()).unwrap();
        assert_eq!(sid.as_str(), "abc");
    }

    #[test]
    fn test_session_id_empty() {
        assert!(SessionId::new(String::new()).is_err());
    }

    #[test]
    fn test_session_id_too_long() {
        assert!(SessionId::new("a".repeat(257)).is_err());
    }

    #[test]
    fn test_token_valid() {
        let t = Token::new("my-token".to_string()).unwrap();
        assert_eq!(t.as_str(), "my-token");
    }

    #[test]
    fn test_validate_auth_fields_ok() {
        assert!(validate_auth_fields("MIT-MAGIC-COOKIE-1", &[1, 2, 3]).is_ok());
    }

    #[test]
    fn test_validate_auth_fields_name_too_long() {
        assert!(validate_auth_fields(&"x".repeat(257), &[1]).is_err());
    }

    #[test]
    fn test_validate_auth_fields_data_too_long() {
        assert!(validate_auth_fields("name", &[0u8; 4097]).is_err());
    }
}
