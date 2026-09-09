//! The offline verifier: an EdDSA (Ed25519) JWT against an issuer's JWKS.
//!
//! The JWKS is RFC 7517 with the key type RFC 8037 gives Ed25519: `kty: OKP`,
//! `crv: Ed25519`, `x: <base64url public key>`, `kid`. Keys of any other type are carried
//! but never used; only the `kid` in the token's header picks a key.
//!
//! `jsonwebtoken` does the signature; the dates are checked here, because the SDK's
//! claims use `exp: 0` for "perpetual" and a library would reject that as expired.

use avada_module_sdk::license::LicenseClaims;
use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Header, Validation};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Seconds of clock skew tolerated on `nbf` and `exp`.
pub const DEFAULT_SKEW_SECS: u64 = 300;

/// One key of a JWKS. Fields of key types other than OKP are ignored; the key is kept
/// (an issuer may publish RSA keys for other purposes) but cannot verify anything here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Jwk {
    /// Key type: `OKP` for Ed25519.
    pub kty: String,
    /// Key id.
    #[serde(default)]
    pub kid: String,
    /// Curve: `Ed25519`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crv: Option<String>,
    /// The public key, base64url without padding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    /// Intended algorithm, when stated (`EdDSA`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    /// Intended use, when stated (`sig`).
    #[serde(default, rename = "use", skip_serializing_if = "Option::is_none")]
    pub use_: Option<String>,
}

impl Jwk {
    /// An Ed25519 signing key as a JWK, from its 32 public bytes.
    pub fn ed25519(kid: &str, public: &[u8; 32]) -> Self {
        Jwk {
            kty: "OKP".into(),
            kid: kid.into(),
            crv: Some("Ed25519".into()),
            x: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public)),
            alg: Some("EdDSA".into()),
            use_: Some("sig".into()),
        }
    }

    /// Whether this is an Ed25519 key.
    pub fn is_ed25519(&self) -> bool {
        self.kty == "OKP" && self.crv.as_deref() == Some("Ed25519") && self.x.is_some()
    }

    fn decoding_key(&self) -> Result<DecodingKey, VerifyError> {
        if !self.is_ed25519() {
            return Err(VerifyError::BadKey(format!(
                "key {} is not an Ed25519 OKP key",
                self.kid
            )));
        }
        let x = self.x.as_deref().unwrap_or("");
        // Check the shape ourselves so a bad key is reported as such, not as a signature
        // failure.
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(x)
            .map_err(|e| VerifyError::BadKey(format!("key {}: {e}", self.kid)))?;
        if bytes.len() != 32 {
            return Err(VerifyError::BadKey(format!(
                "key {}: {} bytes, want 32",
                self.kid,
                bytes.len()
            )));
        }
        DecodingKey::from_ed_components(x)
            .map_err(|e| VerifyError::BadKey(format!("key {}: {e}", self.kid)))
    }
}

/// An issuer's key set (RFC 7517 §5).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Jwks {
    /// The keys.
    pub keys: Vec<Jwk>,
}

impl Jwks {
    /// The key with this id.
    pub fn find(&self, kid: &str) -> Option<&Jwk> {
        self.keys.iter().find(|k| k.kid == kid)
    }

    /// Whether a key with this id is present.
    pub fn has(&self, kid: &str) -> bool {
        self.find(kid).is_some()
    }
}

/// Why a token did not verify. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Not a JWT, or the claims are not the expected shape.
    Malformed(String),
    /// The header names no `kid`; there is nothing to look up.
    NoKid,
    /// The header's `kid` is not in the key set.
    UnknownKey(String),
    /// The signature did not verify with the named key.
    BadSignature,
    /// The named key is unusable.
    BadKey(String),
    /// The header's `alg` is not `EdDSA`.
    Algorithm(String),
    /// `nbf` is in the future (beyond skew).
    NotYet,
    /// `exp` is in the past (beyond skew).
    Expired,
    /// The claims name a different product.
    WrongProduct {
        /// The product asked for.
        expected: String,
        /// The product in the claims.
        actual: String,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Malformed(e) => write!(f, "malformed token: {e}"),
            VerifyError::NoKid => write!(f, "token header names no key id"),
            VerifyError::UnknownKey(kid) => write!(f, "unknown signing key {kid:?}"),
            VerifyError::BadSignature => write!(f, "signature does not verify"),
            VerifyError::BadKey(e) => write!(f, "unusable signing key: {e}"),
            VerifyError::Algorithm(a) => write!(f, "unsupported algorithm {a:?}"),
            VerifyError::NotYet => write!(f, "not valid yet"),
            VerifyError::Expired => write!(f, "expired"),
            VerifyError::WrongProduct { expected, actual } => {
                write!(f, "license is for {actual}, not {expected}")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// The header, without verifying anything. Used to pick the key.
pub fn peek_header(token: &str) -> Result<Header, VerifyError> {
    jsonwebtoken::decode_header(token).map_err(|e| VerifyError::Malformed(e.to_string()))
}

/// The claims that route a token before it is verified: which product, which issuer.
/// Trust nothing here; the verified claims are the ones that count.
#[derive(Debug, Clone, Deserialize)]
pub struct PeekClaims {
    /// `product`.
    pub product: String,
    /// `iss`, when the issuer put one in.
    #[serde(default)]
    pub iss: Option<String>,
    /// `kid`.
    #[serde(default)]
    pub kid: Option<String>,
}

/// The payload, decoded but not verified.
pub fn peek_claims(token: &str) -> Result<PeekClaims, VerifyError> {
    let mut parts = token.split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(VerifyError::Malformed(
            "not three dot-separated parts".into(),
        ));
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| VerifyError::Malformed(format!("payload: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| VerifyError::Malformed(format!("claims: {e}")))
}

/// The verifier: a skew, and the rules above.
#[derive(Debug, Clone)]
pub struct Verifier {
    /// Seconds of clock skew tolerated on `nbf` and `exp`.
    pub skew_secs: u64,
}

impl Default for Verifier {
    fn default() -> Self {
        Verifier {
            skew_secs: DEFAULT_SKEW_SECS,
        }
    }
}

impl Verifier {
    /// Verify the signature and decode the payload as `T`. No date, audience or issuer
    /// checks: the caller applies whatever its claims mean.
    pub fn decode<T: DeserializeOwned>(
        &self,
        token: &str,
        keys: &Jwks,
    ) -> Result<(Header, T), VerifyError> {
        let header = peek_header(token)?;
        if header.alg != Algorithm::EdDSA {
            return Err(VerifyError::Algorithm(format!("{:?}", header.alg)));
        }
        let kid = header.kid.clone().ok_or(VerifyError::NoKid)?;
        let key = keys
            .find(&kid)
            .ok_or_else(|| VerifyError::UnknownKey(kid.clone()))?
            .decoding_key()?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        let data = jsonwebtoken::decode::<T>(token, &key, &validation).map_err(|e| {
            use jsonwebtoken::errors::ErrorKind;
            match e.kind() {
                ErrorKind::InvalidSignature => VerifyError::BadSignature,
                ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
                    VerifyError::Algorithm(format!("{:?}", header.alg))
                }
                _ => VerifyError::Malformed(e.to_string()),
            }
        })?;
        Ok((data.header, data.claims))
    }

    /// Signature and shape only: the claims of a license token signed by one of `keys`,
    /// for `expected_product` when given. Dates are not checked (see
    /// [`verify_license`](Self::verify_license)); the state machine handles them so a
    /// stored license that expired is reported as expired, not as invalid.
    pub fn verify_signature(
        &self,
        token: &str,
        keys: &Jwks,
        expected_product: Option<&str>,
    ) -> Result<LicenseClaims, VerifyError> {
        let (header, claims): (Header, LicenseClaims) = self.decode(token, keys)?;
        if !claims.kid.is_empty() && header.kid.as_deref() != Some(claims.kid.as_str()) {
            return Err(VerifyError::Malformed(
                "header kid and claims kid disagree".into(),
            ));
        }
        if let Some(expected) = expected_product {
            if claims.product != expected {
                return Err(VerifyError::WrongProduct {
                    expected: expected.to_string(),
                    actual: claims.product,
                });
            }
        }
        Ok(claims)
    }

    /// A full verification at `now`: signature, product, `nbf`/`exp` with skew.
    pub fn verify_license(
        &self,
        token: &str,
        keys: &Jwks,
        expected_product: Option<&str>,
        now: u64,
    ) -> Result<LicenseClaims, VerifyError> {
        let claims = self.verify_signature(token, keys, expected_product)?;
        self.check_dates(&claims, now)?;
        Ok(claims)
    }

    /// `nbf`/`exp` against `now`, with skew. `exp == 0` is perpetual.
    pub fn check_dates(&self, claims: &LicenseClaims, now: u64) -> Result<(), VerifyError> {
        if now.saturating_add(self.skew_secs) < claims.nbf {
            return Err(VerifyError::NotYet);
        }
        if claims.exp != 0 && now >= claims.exp.saturating_add(self.skew_secs) {
            return Err(VerifyError::Expired);
        }
        Ok(())
    }
}

/// An in-memory Ed25519 signing key for the stub issuer and the tests. The private half
/// never leaves the process and has no `Debug`.
pub struct SigningKey {
    encoding: jsonwebtoken::EncodingKey,
    public: [u8; 32],
    kid: String,
}

impl SigningKey {
    /// A fresh random key with a random `kid`.
    pub fn generate() -> Self {
        let kid = uuid::Uuid::new_v4().simple().to_string();
        Self::generate_with_kid(&kid[..12])
    }

    /// A fresh random key with this `kid`.
    pub fn generate_with_kid(kid: &str) -> Self {
        Self::from_seed(kid, &rand::random())
    }

    /// The key an issuer already has, rather than one invented on the spot.
    ///
    /// [`generate_with_kid`](Self::generate_with_kid) is right for a stub that lives and
    /// dies with the test around it, and wrong for anything that has signed a licence
    /// somebody paid for: a key generated at start means every restart repudiates every
    /// licence in the field. A real issuer reads its seed from wherever it is kept ---
    /// for `avada-license`, a vault, at start, never a file --- and hands it here.
    ///
    /// The 32 bytes are the Ed25519 seed (RFC 8032 §5.1.5), not the expanded key, and not
    /// the public half. They are consumed into the key and never given back: `SigningKey`
    /// has no accessor for its private half and no `Debug` that could leak one.
    pub fn from_seed(kid: &str, seed: &[u8; 32]) -> Self {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let seed: [u8; 32] = *seed;
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let der = signing
            .to_pkcs8_der()
            .expect("ed25519 key encodes as pkcs8");
        let encoding = jsonwebtoken::EncodingKey::from_ed_der(der.as_bytes());
        SigningKey {
            encoding,
            public,
            kid: kid.to_string(),
        }
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public half as a JWK.
    pub fn jwk(&self) -> Jwk {
        Jwk::ed25519(&self.kid, &self.public)
    }

    /// Sign `claims` as a JWT with `typ` in the header.
    pub fn sign<T: Serialize>(&self, claims: &T, typ: &str) -> Result<String, VerifyError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        header.typ = Some(typ.to_string());
        jsonwebtoken::encode(&header, claims, &self.encoding)
            .map_err(|e| VerifyError::Malformed(format!("encode: {e}")))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigningKey(kid={})", self.kid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(product: &str, kid: &str, nbf: u64, exp: u64) -> LicenseClaims {
        LicenseClaims {
            jti: "j1".into(),
            product: product.into(),
            licensee: "dev@example.test".into(),
            seats: 1,
            nbf,
            exp,
            max_major: None,
            kid: kid.into(),
            download_url: None,
            checkin_interval_days: 7,
        }
    }

    fn keyset(k: &SigningKey) -> Jwks {
        Jwks {
            keys: vec![k.jwk()],
        }
    }

    #[test]
    fn jwk_round_trips_as_rfc_8037_okp() {
        let k = SigningKey::generate_with_kid("k1");
        let jwk = k.jwk();
        assert_eq!(jwk.kty, "OKP");
        assert_eq!(jwk.crv.as_deref(), Some("Ed25519"));
        assert!(jwk.is_ed25519());
        let json = serde_json::to_string(&Jwks { keys: vec![jwk] }).unwrap();
        assert!(json.contains("\"use\":\"sig\""), "{json}");
        let back: Jwks = serde_json::from_str(&json).unwrap();
        assert!(back.has("k1"));
        assert!(!back.has("k2"));
        // Other key types parse and are simply unusable.
        let mixed: Jwks =
            serde_json::from_str(r#"{"keys":[{"kty":"RSA","kid":"r","n":"x","e":"AQAB"}]}"#)
                .unwrap();
        assert!(!mixed.find("r").unwrap().is_ed25519());
        assert!(matches!(
            mixed.find("r").unwrap().decoding_key(),
            Err(VerifyError::BadKey(_))
        ));
    }

    #[test]
    fn a_good_token_verifies_and_the_claims_come_back() {
        let k = SigningKey::generate_with_kid("k1");
        let c = claims("acme/widget", "k1", 1_000, 0);
        let tok = k.sign(&c, "JWT").unwrap();
        let v = Verifier::default();
        let got = v
            .verify_license(&tok, &keyset(&k), Some("acme/widget"), 5_000)
            .unwrap();
        assert_eq!(got, c);
        let header = peek_header(&tok).unwrap();
        assert_eq!(header.kid.as_deref(), Some("k1"));
        assert_eq!(header.alg, Algorithm::EdDSA);
        let peek = peek_claims(&tok).unwrap();
        assert_eq!(peek.product, "acme/widget");
        assert_eq!(peek.kid.as_deref(), Some("k1"));
        assert!(peek.iss.is_none());
    }

    /// The property a licence server is bought for: restarting it does not repudiate
    /// what it signed yesterday.
    ///
    /// `generate_with_kid` mints a new key every call, so a server that generated its key
    /// at start would hand out licences that its own next boot could not verify --- and
    /// the failure would look, from the customer's side, exactly like a forged licence.
    /// Seeding from a value kept elsewhere is what makes the key outlive the process, so
    /// what this proves is that the *same* seed really does reproduce the same key, and
    /// that a token signed before a restart still verifies against the key set after one.
    #[test]
    fn a_seeded_key_survives_the_restart_that_a_generated_one_would_repudiate() {
        let seed = [7u8; 32];
        let before = SigningKey::from_seed("k1", &seed);
        let tok = before
            .sign(&claims("acme/widget", "k1", 0, 0), "JWT")
            .unwrap();

        // The restart: a second process, the same seed out of the vault, nothing shared.
        let after = SigningKey::from_seed("k1", &seed);
        assert_eq!(after.jwk(), before.jwk(), "the same seed is the same key");
        let v = Verifier::default();
        v.verify_license(&tok, &keyset(&after), None, 10)
            .expect("yesterday's licence still verifies after a restart");

        // And the seed is the whole of the difference: a different one is a different
        // key even under the same `kid`, which is the case that must *not* verify.
        let other = SigningKey::from_seed("k1", &[8u8; 32]);
        assert_ne!(other.jwk(), before.jwk());
        assert_eq!(
            v.verify_license(&tok, &keyset(&other), None, 10)
                .unwrap_err(),
            VerifyError::BadSignature
        );
    }

    #[test]
    fn the_wrong_key_unknown_kid_and_tampering_are_refused() {
        let k1 = SigningKey::generate_with_kid("k1");
        let k2 = SigningKey::generate_with_kid("k1");
        let c = claims("acme/widget", "k1", 0, 0);
        let tok = k1.sign(&c, "JWT").unwrap();
        let v = Verifier::default();
        assert_eq!(
            v.verify_license(&tok, &keyset(&k2), None, 10).unwrap_err(),
            VerifyError::BadSignature
        );
        let other = SigningKey::generate_with_kid("k9");
        assert_eq!(
            v.verify_license(&tok, &keyset(&other), None, 10)
                .unwrap_err(),
            VerifyError::UnknownKey("k1".into())
        );
        assert_eq!(
            v.verify_license(&tok, &Jwks::default(), None, 10)
                .unwrap_err(),
            VerifyError::UnknownKey("k1".into())
        );
        // Flip the payload: the signature no longer matches.
        let mut parts: Vec<String> = tok.split('.').map(str::to_string).collect();
        let forged = claims("acme/widget", "k1", 0, 0);
        let mut forged = forged;
        forged.seats = 500;
        parts[1] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&forged).unwrap());
        let tampered = parts.join(".");
        assert_eq!(
            v.verify_license(&tampered, &keyset(&k1), None, 10)
                .unwrap_err(),
            VerifyError::BadSignature
        );
        assert!(matches!(
            v.verify_license("not.a.jwt", &keyset(&k1), None, 10)
                .unwrap_err(),
            VerifyError::Malformed(_)
        ));
        assert!(matches!(
            peek_claims("only-one-part").unwrap_err(),
            VerifyError::Malformed(_)
        ));
    }

    #[test]
    fn a_token_without_kid_or_with_another_alg_is_refused() {
        let k = SigningKey::generate_with_kid("k1");
        let c = claims("acme/widget", "", 0, 0);
        let header = Header::new(Algorithm::EdDSA);
        let tok = jsonwebtoken::encode(&header, &c, &k.encoding).unwrap();
        let v = Verifier::default();
        assert_eq!(
            v.verify_license(&tok, &keyset(&k), None, 10).unwrap_err(),
            VerifyError::NoKid
        );
        // An HMAC token with the same kid: the alg is wrong before any key is touched.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let hs = jsonwebtoken::encode(
            &header,
            &c,
            &jsonwebtoken::EncodingKey::from_secret(b"not-a-secret-just-a-test"),
        )
        .unwrap();
        assert!(matches!(
            v.verify_license(&hs, &keyset(&k), None, 10).unwrap_err(),
            VerifyError::Algorithm(_)
        ));
    }

    #[test]
    fn header_kid_must_match_the_claims_kid() {
        let k = SigningKey::generate_with_kid("k1");
        let c = claims("acme/widget", "k-other", 0, 0);
        let tok = k.sign(&c, "JWT").unwrap();
        let v = Verifier::default();
        assert!(matches!(
            v.verify_license(&tok, &keyset(&k), None, 10).unwrap_err(),
            VerifyError::Malformed(_)
        ));
    }

    #[test]
    fn product_must_match_when_asked() {
        let k = SigningKey::generate_with_kid("k1");
        let tok = k.sign(&claims("acme/widget", "k1", 0, 0), "JWT").unwrap();
        let v = Verifier::default();
        assert_eq!(
            v.verify_license(&tok, &keyset(&k), Some("acme/other"), 10)
                .unwrap_err(),
            VerifyError::WrongProduct {
                expected: "acme/other".into(),
                actual: "acme/widget".into(),
            }
        );
        assert!(v.verify_license(&tok, &keyset(&k), None, 10).is_ok());
    }

    #[test]
    fn nbf_and_exp_honour_the_skew_and_zero_exp_is_perpetual() {
        let k = SigningKey::generate_with_kid("k1");
        let keys = keyset(&k);
        let v = Verifier { skew_secs: 60 };
        let tok = k
            .sign(&claims("acme/widget", "k1", 1_000, 2_000), "JWT")
            .unwrap();
        assert_eq!(
            v.verify_license(&tok, &keys, None, 900).unwrap_err(),
            VerifyError::NotYet
        );
        assert!(
            v.verify_license(&tok, &keys, None, 940).is_ok(),
            "within skew"
        );
        assert!(v.verify_license(&tok, &keys, None, 1_500).is_ok());
        assert!(
            v.verify_license(&tok, &keys, None, 2_059).is_ok(),
            "within skew"
        );
        assert_eq!(
            v.verify_license(&tok, &keys, None, 2_060).unwrap_err(),
            VerifyError::Expired
        );
        // Signature-only verification ignores the dates.
        assert!(v.verify_signature(&tok, &keys, None).is_ok());
        let perpetual = k.sign(&claims("acme/widget", "k1", 0, 0), "JWT").unwrap();
        assert!(v
            .verify_license(&perpetual, &keys, None, u64::MAX / 2)
            .is_ok());
    }

    #[test]
    fn errors_and_debug_never_carry_the_token() {
        let k = SigningKey::generate_with_kid("k1");
        let tok = k.sign(&claims("acme/widget", "k1", 0, 0), "JWT").unwrap();
        let v = Verifier::default();
        let e = v
            .verify_license(&tok, &Jwks::default(), None, 10)
            .unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(&tok[..20]), "{shown}");
        let shown = format!("{k:?}");
        assert_eq!(shown, "SigningKey(kid=k1)");
    }
}
