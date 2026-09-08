//! The per-module token: 32 random bytes, hex, minted at every spawn and handed to the
//! module inside the host hello. The host keeps it only to recognise the module later
//! (`control` introspection, §9 of the contract); it is never logged — `Debug` redacts
//! it and there is no `Display`.

use rand::Rng;
use subtle::ConstantTimeEq;

/// One module's secret for one run.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// 32 bytes from the OS-seeded generator, as 64 lowercase hex digits.
    pub fn mint() -> Token {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        Token(hex(&bytes))
    }

    /// Constant-time comparison against a value a caller presented.
    pub fn matches(&self, presented: &str) -> bool {
        self.0.as_bytes().ct_eq(presented.as_bytes()).into()
    }

    /// The secret itself, for the one place that puts it on the wire.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_digits_and_unique() {
        let a = Token::mint();
        let b = Token::mint();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.matches(b.expose()));
        assert!(a.matches(a.expose()));
        assert!(!a.matches(""));
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let t = Token::mint();
        let shown = format!("{t:?}");
        assert_eq!(shown, "Token(<redacted>)");
        assert!(!shown.contains(t.expose()));
    }
}
