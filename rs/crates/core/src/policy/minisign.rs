//! The minisign v2 detached-signature format, implemented here rather than pulled in.
//!
//! There is no minisign crate in this workspace's offline registry and no Cargo.toml
//! edits are allowed on this track, so the format is parsed and verified directly on
//! top of `ed25519-dalek`. It is a small format and the whole of it is below.
//!
//! # The format
//!
//! A public key file is two lines: an `untrusted comment:` header and one base64 blob
//! of **42 bytes** — the two ASCII bytes `Ed`, an 8-byte key id, and the 32-byte
//! Ed25519 public key.
//!
//! ```text
//! untrusted comment: minisign public key 5A5B1E4A0F0C0D0E
//! RWQOD...
//! ```
//!
//! A detached signature (`<file>.minisig`) is four lines:
//!
//! ```text
//! untrusted comment: signature from minisign secret key
//! <base64: alg(2) ++ key_id(8) ++ signature(64)>
//! trusted comment: timestamp:… file:avada-files
//! <base64: global_signature(64)>
//! ```
//!
//! The first signature covers the file's bytes. The **global** signature covers
//! `signature ++ trusted_comment` and is what makes the trusted comment trustworthy —
//! it is the only part of the file an attacker cannot rewrite, which is why this
//! implementation checks it and why [`Verdict::Trusted`](super::Verdict::Trusted)
//! quotes it as the authority.
//!
//! # `ED` (prehashed) is refused, on purpose
//!
//! Minisign's `-H` mode signs a BLAKE2b-512 hash of the file instead of the file and
//! writes `ED` as the algorithm. No BLAKE2b implementation is available offline here,
//! so rather than guess, [`MinisignError::Prehashed`] is returned and the caller turns
//! it into [`Verdict::Invalid`](super::Verdict::Invalid) — never into a pass. Signing
//! without `-H` produces a signature this build can check. See the follow-up in
//! `docs/notarization.md`.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, VerifyingKey};
use std::fmt;
use std::path::{Path, PathBuf};

/// The algorithm bytes of a plain (not prehashed) minisign signature.
pub const ALG_ED25519: [u8; 2] = *b"Ed";
/// The algorithm bytes of a prehashed (BLAKE2b) minisign signature, which this build
/// refuses.
pub const ALG_PREHASHED: [u8; 2] = *b"ED";
/// The extension minisign appends for a detached signature.
pub const SIG_SUFFIX: &str = ".minisig";

const KEY_ID_LEN: usize = 8;
const PUBLIC_KEY_BLOB_LEN: usize = 2 + KEY_ID_LEN + 32;
const SIGNATURE_BLOB_LEN: usize = 2 + KEY_ID_LEN + 64;
const UNTRUSTED_PREFIX: &str = "untrusted comment:";
const TRUSTED_PREFIX: &str = "trusted comment:";

/// A minisign public key: the 8-byte key id it is filed under and the Ed25519 key.
///
/// `Debug` deliberately prints only the key id. A public key is not a secret, but a
/// log line full of base64 is noise, and the key id is what a user actually compares
/// against what the publisher published.
#[derive(Clone, Copy)]
pub struct PublicKey {
    key_id: [u8; KEY_ID_LEN],
    key: VerifyingKey,
}

impl PublicKey {
    /// Build from an already-decoded key id and Ed25519 public key.
    pub fn from_parts(key_id: [u8; KEY_ID_LEN], key: &[u8; 32]) -> Result<Self, MinisignError> {
        let key = VerifyingKey::from_bytes(key).map_err(|_| MinisignError::BadPublicKey)?;
        Ok(PublicKey { key_id, key })
    }

    /// The raw 8-byte key id.
    pub fn key_id(&self) -> [u8; KEY_ID_LEN] {
        self.key_id
    }

    /// The key id the way minisign prints it: the little-endian `u64` in uppercase hex.
    pub fn key_id_hex(&self) -> String {
        format!("{:016X}", u64::from_le_bytes(self.key_id))
    }

    /// The 42-byte blob's base64, i.e. the second line of a `.pub` file.
    pub fn to_base64(&self) -> String {
        let mut blob = Vec::with_capacity(PUBLIC_KEY_BLOB_LEN);
        blob.extend_from_slice(&ALG_ED25519);
        blob.extend_from_slice(&self.key_id);
        blob.extend_from_slice(&self.key.to_bytes());
        STANDARD.encode(blob)
    }
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.key_id == other.key_id && self.key.to_bytes() == other.key.to_bytes()
    }
}
impl Eq for PublicKey {}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.key_id_hex())
    }
}

/// A parsed `.minisig`.
#[derive(Clone, PartialEq, Eq)]
pub struct Signature {
    /// `Ed` or `ED`; only `Ed` can be checked here.
    pub algorithm: [u8; 2],
    /// The key id the signer claims.
    pub key_id: [u8; KEY_ID_LEN],
    /// The signature over the file's bytes.
    signature: [u8; 64],
    /// The text after `trusted comment: `, exactly as it was signed.
    pub trusted_comment: String,
    /// The signature over `signature ++ trusted_comment`.
    global_signature: [u8; 64],
}

impl Signature {
    /// The key id the way minisign prints it.
    pub fn key_id_hex(&self) -> String {
        format!("{:016X}", u64::from_le_bytes(self.key_id))
    }

    /// Whether this is a prehashed (`-H`) signature, which this build cannot check.
    pub fn is_prehashed(&self) -> bool {
        self.algorithm == ALG_PREHASHED
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signature")
            .field("algorithm", &String::from_utf8_lossy(&self.algorithm))
            .field("key_id", &self.key_id_hex())
            .field("trusted_comment", &self.trusted_comment)
            .finish()
    }
}

/// Everything that can go wrong reading or checking a minisign signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinisignError {
    /// The text held no base64 payload line at all.
    NoPayload,
    /// A payload line is not valid base64.
    NotBase64,
    /// A decoded blob is the wrong size.
    BadLength {
        /// Which blob.
        what: &'static str,
        /// How many bytes the format requires.
        expected: usize,
        /// How many were there.
        actual: usize,
    },
    /// The 32 bytes are not a valid Ed25519 point.
    BadPublicKey,
    /// The signature is minisign's prehashed (`-H`) form, which needs BLAKE2b.
    Prehashed,
    /// The algorithm field is neither `Ed` nor `ED`.
    UnsupportedAlgorithm(String),
    /// The `trusted comment:` line is missing or malformed.
    MissingTrustedComment,
    /// The global signature line is missing.
    MissingGlobalSignature,
    /// No configured publisher key has the key id the signature names.
    UnknownKey {
        /// The key id the signature claimed.
        key_id: String,
        /// The key ids that were configured.
        configured: Vec<String>,
    },
    /// No publisher key is configured at all, so nothing can be checked.
    NoKeys,
    /// The signature does not match the file's bytes under the named key.
    BadSignature,
    /// The file's bytes verified but the trusted comment's global signature did not —
    /// somebody edited the comment after it was signed.
    BadTrustedComment,
}

impl fmt::Display for MinisignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MinisignError::NoPayload => f.write_str("no base64 payload line"),
            MinisignError::NotBase64 => f.write_str("payload line is not valid base64"),
            MinisignError::BadLength {
                what,
                expected,
                actual,
            } => write!(f, "{what} is {actual} bytes, expected {expected}"),
            MinisignError::BadPublicKey => f.write_str("not a valid ed25519 public key"),
            MinisignError::Prehashed => f.write_str(
                "prehashed (minisign -H) signatures use BLAKE2b, which this build cannot check; \
                 re-sign without -H",
            ),
            MinisignError::UnsupportedAlgorithm(alg) => {
                write!(f, "unsupported signature algorithm {alg:?}")
            }
            MinisignError::MissingTrustedComment => {
                f.write_str("no `trusted comment:` line in the signature")
            }
            MinisignError::MissingGlobalSignature => {
                f.write_str("no global signature line after the trusted comment")
            }
            MinisignError::UnknownKey { key_id, configured } => {
                if configured.is_empty() {
                    write!(f, "signed by key {key_id}, which is not a publisher key")
                } else {
                    write!(
                        f,
                        "signed by key {key_id}, but this module's publisher keys are {}",
                        configured.join(", ")
                    )
                }
            }
            MinisignError::NoKeys => f.write_str("no publisher key is configured for this module"),
            MinisignError::BadSignature => {
                f.write_str("the signature does not match the file's contents")
            }
            MinisignError::BadTrustedComment => {
                f.write_str("the trusted comment was altered after it was signed")
            }
        }
    }
}
impl std::error::Error for MinisignError {}

/// `<binary>.minisig`.
pub fn sidecar_path(binary: &Path) -> PathBuf {
    let mut name = binary.as_os_str().to_os_string();
    name.push(SIG_SUFFIX);
    PathBuf::from(name)
}

/// Parse a public key: either a whole `.pub` file or the bare base64 line from
/// `policy.json`.
pub fn parse_public_key(text: &str) -> Result<PublicKey, MinisignError> {
    let payload = lines(text)
        .into_iter()
        .find(|l| !l.trim().is_empty() && !is_comment(l))
        .ok_or(MinisignError::NoPayload)?;
    let blob = decode(payload.trim())?;
    if blob.len() != PUBLIC_KEY_BLOB_LEN {
        return Err(MinisignError::BadLength {
            what: "public key",
            expected: PUBLIC_KEY_BLOB_LEN,
            actual: blob.len(),
        });
    }
    let alg = [blob[0], blob[1]];
    if alg != ALG_ED25519 {
        return Err(MinisignError::UnsupportedAlgorithm(
            String::from_utf8_lossy(&alg).into_owned(),
        ));
    }
    let mut key_id = [0u8; KEY_ID_LEN];
    key_id.copy_from_slice(&blob[2..2 + KEY_ID_LEN]);
    let mut key = [0u8; 32];
    key.copy_from_slice(&blob[2 + KEY_ID_LEN..]);
    PublicKey::from_parts(key_id, &key)
}

/// Parse a `.minisig`.
///
/// The trusted comment is taken byte-for-byte after `trusted comment: `, because those
/// exact bytes are what the global signature covers — trimming them would silently
/// break verification of a comment that legitimately ends in a space.
pub fn parse_signature(text: &str) -> Result<Signature, MinisignError> {
    let all = lines(text);
    let mut it = all.iter().copied().filter(|l| !l.trim().is_empty());

    let mut first = it.next().ok_or(MinisignError::NoPayload)?;
    if is_comment(first) {
        first = it.next().ok_or(MinisignError::NoPayload)?;
    }
    let blob = decode(first.trim())?;
    if blob.len() != SIGNATURE_BLOB_LEN {
        return Err(MinisignError::BadLength {
            what: "signature",
            expected: SIGNATURE_BLOB_LEN,
            actual: blob.len(),
        });
    }
    let algorithm = [blob[0], blob[1]];
    let mut key_id = [0u8; KEY_ID_LEN];
    key_id.copy_from_slice(&blob[2..2 + KEY_ID_LEN]);
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&blob[2 + KEY_ID_LEN..]);

    let tc_line = it.next().ok_or(MinisignError::MissingTrustedComment)?;
    let rest = tc_line
        .strip_prefix(TRUSTED_PREFIX)
        .ok_or(MinisignError::MissingTrustedComment)?;
    // minisign writes exactly one space after the colon and does not sign it.
    let trusted_comment = rest.strip_prefix(' ').unwrap_or(rest).to_string();

    let global = it.next().ok_or(MinisignError::MissingGlobalSignature)?;
    let global = decode(global.trim())?;
    if global.len() != 64 {
        return Err(MinisignError::BadLength {
            what: "global signature",
            expected: 64,
            actual: global.len(),
        });
    }
    let mut global_signature = [0u8; 64];
    global_signature.copy_from_slice(&global);

    Ok(Signature {
        algorithm,
        key_id,
        signature,
        trusted_comment,
        global_signature,
    })
}

/// Check `sig` over `data` against the publisher's `keys`.
///
/// Returns the key that vouched, so the caller can name it. Every failure mode is a
/// distinct error rather than a bare bool: "wrong key id" and "the bytes changed" send
/// a user to two entirely different places.
pub fn verify(
    data: &[u8],
    sig: &Signature,
    keys: &[PublicKey],
) -> Result<PublicKey, MinisignError> {
    if sig.is_prehashed() {
        return Err(MinisignError::Prehashed);
    }
    if sig.algorithm != ALG_ED25519 {
        return Err(MinisignError::UnsupportedAlgorithm(
            String::from_utf8_lossy(&sig.algorithm).into_owned(),
        ));
    }
    if keys.is_empty() {
        return Err(MinisignError::NoKeys);
    }
    let key = keys
        .iter()
        .find(|k| k.key_id == sig.key_id)
        .ok_or_else(|| MinisignError::UnknownKey {
            key_id: sig.key_id_hex(),
            configured: keys.iter().map(PublicKey::key_id_hex).collect(),
        })?;

    let signature = EdSignature::from_bytes(&sig.signature);
    key.key
        .verify_strict(data, &signature)
        .map_err(|_| MinisignError::BadSignature)?;

    // The trusted comment is only worth quoting once its own signature holds.
    let mut global_message = Vec::with_capacity(64 + sig.trusted_comment.len());
    global_message.extend_from_slice(&sig.signature);
    global_message.extend_from_slice(sig.trusted_comment.as_bytes());
    let global = EdSignature::from_bytes(&sig.global_signature);
    key.key
        .verify_strict(&global_message, &global)
        .map_err(|_| MinisignError::BadTrustedComment)?;

    Ok(*key)
}

fn lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.trim_end_matches('\r')).collect()
}

fn is_comment(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with(UNTRUSTED_PREFIX) || l.starts_with(TRUSTED_PREFIX)
}

fn decode(payload: &str) -> Result<Vec<u8>, MinisignError> {
    STANDARD
        .decode(payload.as_bytes())
        .map_err(|_| MinisignError::NotBase64)
}

/// Minting signatures for tests. Not compiled into the shipped host: nothing outside
/// a test ever holds a minisign secret key, and the host has no reason to sign.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// A deterministic test signer. The seed is a test constant, not a secret.
    pub(crate) struct Signer32 {
        pub(crate) signing: SigningKey,
        pub(crate) key_id: [u8; KEY_ID_LEN],
    }

    impl Signer32 {
        /// Mint a key pair from a one-byte seed pattern and an arbitrary key id.
        pub(crate) fn new(seed: u8, key_id: u64) -> Self {
            Signer32 {
                signing: SigningKey::from_bytes(&[seed; 32]),
                key_id: key_id.to_le_bytes(),
            }
        }

        pub(crate) fn public(&self) -> PublicKey {
            PublicKey {
                key_id: self.key_id,
                key: self.signing.verifying_key(),
            }
        }

        /// A well-formed `.minisig` over `data`.
        pub(crate) fn sign(&self, data: &[u8], trusted_comment: &str) -> String {
            let sig = self.signing.sign(data).to_bytes();
            let mut global_message = Vec::new();
            global_message.extend_from_slice(&sig);
            global_message.extend_from_slice(trusted_comment.as_bytes());
            let global = self.signing.sign(&global_message).to_bytes();
            self.assemble(ALG_ED25519, &sig, trusted_comment, &global)
        }

        /// A `.minisig` whose algorithm byte pair is `alg` — used to build the `ED`
        /// (prehashed) case that must be refused.
        pub(crate) fn sign_with_alg(
            &self,
            alg: [u8; 2],
            data: &[u8],
            trusted_comment: &str,
        ) -> String {
            let sig = self.signing.sign(data).to_bytes();
            let mut global_message = Vec::new();
            global_message.extend_from_slice(&sig);
            global_message.extend_from_slice(trusted_comment.as_bytes());
            let global = self.signing.sign(&global_message).to_bytes();
            self.assemble(alg, &sig, trusted_comment, &global)
        }

        fn assemble(
            &self,
            alg: [u8; 2],
            sig: &[u8; 64],
            trusted_comment: &str,
            global: &[u8; 64],
        ) -> String {
            let mut blob = Vec::with_capacity(SIGNATURE_BLOB_LEN);
            blob.extend_from_slice(&alg);
            blob.extend_from_slice(&self.key_id);
            blob.extend_from_slice(sig);
            format!(
                "untrusted comment: signature from minisign secret key\n{}\n\
                 trusted comment: {}\n{}\n",
                STANDARD.encode(&blob),
                trusted_comment,
                STANDARD.encode(global)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::Signer32;
    use super::*;

    const DATA: &[u8] = b"#!/bin/sh\necho a module binary\n";
    const COMMENT: &str = "timestamp:1757000000\tfile:avada-files\thashed";

    fn signer() -> Signer32 {
        Signer32::new(7, 0x5A5B_1E4A_0F0C_0D0E)
    }

    #[test]
    fn a_good_signature_names_the_key_that_vouched() {
        let s = signer();
        let sig = parse_signature(&s.sign(DATA, COMMENT)).expect("parses");
        let who = verify(DATA, &sig, &[s.public()]).expect("verifies");
        assert_eq!(who, s.public());
        assert_eq!(sig.trusted_comment, COMMENT);
    }

    #[test]
    fn a_public_key_round_trips_through_base64() {
        let s = signer();
        let parsed = parse_public_key(&s.public().to_base64()).expect("parses");
        assert_eq!(parsed, s.public());
        assert_eq!(parsed.key_id_hex(), "5A5B1E4A0F0C0D0E");
    }

    #[test]
    fn a_public_key_parses_out_of_a_whole_pub_file() {
        let s = signer();
        let file = format!(
            "untrusted comment: minisign public key {}\n{}\n",
            s.public().key_id_hex(),
            s.public().to_base64()
        );
        assert_eq!(parse_public_key(&file).expect("parses"), s.public());
    }

    #[test]
    fn a_tampered_file_fails() {
        let s = signer();
        let sig = parse_signature(&s.sign(DATA, COMMENT)).expect("parses");
        let mut tampered = DATA.to_vec();
        tampered.push(b'!');
        assert_eq!(
            verify(&tampered, &sig, &[s.public()]),
            Err(MinisignError::BadSignature)
        );
    }

    #[test]
    fn the_wrong_key_under_the_right_key_id_fails() {
        // Same key id, different secret: the id is a filing label, not proof.
        let signed = Signer32::new(7, 42);
        let impostor = Signer32::new(9, 42);
        let sig = parse_signature(&signed.sign(DATA, COMMENT)).expect("parses");
        assert_eq!(
            verify(DATA, &sig, &[impostor.public()]),
            Err(MinisignError::BadSignature)
        );
    }

    #[test]
    fn an_unconfigured_key_id_is_named_in_the_error() {
        let signed = Signer32::new(7, 0x1111_1111_1111_1111);
        let configured = Signer32::new(9, 0x2222_2222_2222_2222);
        let sig = parse_signature(&signed.sign(DATA, COMMENT)).expect("parses");
        let err = verify(DATA, &sig, &[configured.public()]).expect_err("wrong key id");
        assert_eq!(
            err,
            MinisignError::UnknownKey {
                key_id: "1111111111111111".into(),
                configured: vec!["2222222222222222".into()],
            }
        );
        assert!(err.to_string().contains("1111111111111111"));
    }

    #[test]
    fn no_configured_key_is_its_own_error() {
        let s = signer();
        let sig = parse_signature(&s.sign(DATA, COMMENT)).expect("parses");
        assert_eq!(verify(DATA, &sig, &[]), Err(MinisignError::NoKeys));
    }

    #[test]
    fn prehashed_signatures_are_refused_rather_than_guessed_at() {
        let s = signer();
        let text = s.sign_with_alg(ALG_PREHASHED, DATA, COMMENT);
        let sig = parse_signature(&text).expect("parses");
        assert!(sig.is_prehashed());
        assert_eq!(
            verify(DATA, &sig, &[s.public()]),
            Err(MinisignError::Prehashed)
        );
        assert!(verify(DATA, &sig, &[s.public()])
            .unwrap_err()
            .to_string()
            .contains("BLAKE2b"));
    }

    #[test]
    fn editing_the_trusted_comment_is_caught_by_the_global_signature() {
        let s = signer();
        let text = s.sign(DATA, COMMENT);
        let forged = text.replace(COMMENT, "timestamp:1757000000\tfile:something-else");
        let sig = parse_signature(&forged).expect("parses");
        // The file's own signature still holds; only the comment moved.
        assert_eq!(
            verify(DATA, &sig, &[s.public()]),
            Err(MinisignError::BadTrustedComment)
        );
    }

    #[test]
    fn a_missing_trusted_comment_line_is_rejected() {
        let s = signer();
        let text = s.sign(DATA, COMMENT);
        let stripped: String = text
            .lines()
            .filter(|l| !l.starts_with(TRUSTED_PREFIX))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_signature(&stripped),
            Err(MinisignError::MissingTrustedComment)
        );
    }

    #[test]
    fn a_truncated_signature_blob_is_rejected() {
        let short = format!(
            "untrusted comment: x\n{}\ntrusted comment: t\n{}\n",
            STANDARD.encode([0u8; 10]),
            STANDARD.encode([0u8; 64])
        );
        assert_eq!(
            parse_signature(&short),
            Err(MinisignError::BadLength {
                what: "signature",
                expected: SIGNATURE_BLOB_LEN,
                actual: 10,
            })
        );
    }

    #[test]
    fn garbage_is_not_mistaken_for_a_key() {
        assert_eq!(
            parse_public_key("not base64 !!!"),
            Err(MinisignError::NotBase64)
        );
        assert_eq!(parse_public_key(""), Err(MinisignError::NoPayload));
        assert_eq!(
            parse_public_key(&STANDARD.encode([0u8; 42])),
            Err(MinisignError::UnsupportedAlgorithm("\0\0".into()))
        );
    }

    #[test]
    fn a_trusted_comment_with_a_trailing_space_still_verifies() {
        // The bytes after `trusted comment: ` are signed verbatim, so the parser must
        // not tidy them.
        let s = signer();
        let comment = "file:avada-files ";
        let sig = parse_signature(&s.sign(DATA, comment)).expect("parses");
        assert_eq!(sig.trusted_comment, comment);
        assert!(verify(DATA, &sig, &[s.public()]).is_ok());
    }

    #[test]
    fn the_sidecar_sits_next_to_the_binary() {
        assert_eq!(
            sidecar_path(Path::new("/m/bin/avada-files")),
            PathBuf::from("/m/bin/avada-files.minisig")
        );
    }
}
