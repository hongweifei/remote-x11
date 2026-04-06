use rand::random;

pub fn generate_token() -> String {
    let random_bytes: [u8; 32] = random();
    hex::encode(random_bytes)
}

pub fn verify_token(provided: &str, expected: &str) -> bool {
    if provided.is_empty() || expected.is_empty() {
        return false;
    }
    if provided.len() != expected.len() {
        let dummy: u8 = provided.bytes().fold(0u8, |acc, b| acc.wrapping_add(b));
        let _ = dummy;
        return false;
    }
    let mismatch: u8 = provided
        .bytes()
        .zip(expected.bytes())
        .fold(0, |acc, (a, b)| acc | (a ^ b));
    mismatch == 0
}

pub fn generate_display_cookie() -> Vec<u8> {
    let cookie: [u8; 16] = random();
    cookie.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_token_length() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
    }

    #[test]
    fn test_generate_token_is_hex() {
        let token = generate_token();
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_tokens_are_unique() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
    }

    #[test]
    fn test_verify_token_match() {
        let token = generate_token();
        assert!(verify_token(&token, &token));
    }

    #[test]
    fn test_verify_token_mismatch() {
        assert!(!verify_token("abcdef1234567890", "abcdef1234567891"));
    }

    #[test]
    fn test_verify_token_different_length() {
        assert!(!verify_token("short", "muchlongerstring"));
    }

    #[test]
    fn test_verify_token_empty() {
        assert!(!verify_token("", ""));
        assert!(!verify_token("", "a"));
        assert!(!verify_token("a", ""));
    }

    #[test]
    fn test_verify_token_timing_safe() {
        assert!(verify_token("aabbccdd", "aabbccdd"));
        assert!(!verify_token("aabbccdd", "aabbccde"));
    }

    #[test]
    fn test_generate_display_cookie_length() {
        let cookie = generate_display_cookie();
        assert_eq!(cookie.len(), 16);
    }

    #[test]
    fn test_generate_display_cookies_are_unique() {
        let c1 = generate_display_cookie();
        let c2 = generate_display_cookie();
        assert_ne!(c1, c2);
    }
}
