//! The Linux verifier: a detached minisign signature beside the artifact.
//!
//! Linux has no OS-wide notion of a signed executable to ask, so the host supplies
//! one. A publisher ships `<binary>.minisig` next to the binary and lists the matching
//! public key in `policy.json`; this verifier checks the pair.
//!
//! The module is compiled on every platform, not just Linux. It has no Linux-specific
//! code — it is file reads and Ed25519 — and compiling it everywhere means its tests
//! run everywhere, which is the whole point of having written the format parser by
//! hand. Only [`platform_verifier`](super::platform_verifier) is `cfg`-gated.

use super::minisign::{self, MinisignError};
use super::{Context, Verdict, Verifier};
use std::path::Path;

/// Verifies a detached `<binary>.minisig` against the publisher keys in the policy.
#[derive(Debug, Default, Clone, Copy)]
pub struct Minisign;

impl Minisign {
    /// A verifier with no state — the keys arrive per call in the [`Context`].
    pub fn new() -> Self {
        Minisign
    }
}

impl Verifier for Minisign {
    fn name(&self) -> &'static str {
        "minisign"
    }

    fn verify(&self, binary: &Path, ctx: &Context) -> Verdict {
        let sidecar = minisign::sidecar_path(binary);
        let text = match std::fs::read_to_string(&sidecar) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Verdict::Unsigned,
            Err(e) => {
                return Verdict::Unavailable {
                    reason: format!("cannot read {}: {e}", sidecar.display()),
                }
            }
        };
        let data = match std::fs::read(binary) {
            Ok(d) => d,
            Err(e) => {
                return Verdict::Unavailable {
                    reason: format!("cannot read {}: {e}", binary.display()),
                }
            }
        };
        let sig = match minisign::parse_signature(&text) {
            Ok(s) => s,
            Err(e) => {
                return Verdict::Invalid {
                    reason: format!("{}: {e}", sidecar.display()),
                }
            }
        };
        match minisign::verify(&data, &sig, &ctx.publisher_keys) {
            Ok(key) => Verdict::Trusted {
                by: format!(
                    "minisign key {} ({})",
                    key.key_id_hex(),
                    sig.trusted_comment
                ),
            },
            // "Nobody told me which key to trust" is not the same claim as "this
            // signature is forged", and the policy treats them differently only
            // because they are reported differently here.
            Err(MinisignError::NoKeys) => Verdict::Unavailable {
                reason: format!(
                    "{} is signed by minisign key {}, but no publisher key is configured for {}",
                    binary.display(),
                    sig.key_id_hex(),
                    ctx.module
                ),
            },
            Err(e) => Verdict::Invalid {
                reason: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::minisign::testkit::Signer32;
    use super::super::{Context, Source, Verdict, Verifier};
    use super::Minisign;
    use avada_module_sdk::ModuleId;
    use std::path::{Path, PathBuf};

    const DATA: &[u8] = b"an artifact\n";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avada-policy-linux-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn ctx(keys: Vec<super::super::PublicKey>) -> Context {
        Context {
            module: ModuleId::new("acme/avada-files").expect("id"),
            source: Source::Prebuilt {
                url: "https://example.invalid/a.tgz".into(),
            },
            publisher_keys: keys,
        }
    }

    fn lay_out(dir: &Path, sig: Option<&str>) -> PathBuf {
        let binary = dir.join("avada-files");
        std::fs::write(&binary, DATA).expect("binary");
        if let Some(text) = sig {
            std::fs::write(dir.join("avada-files.minisig"), text).expect("sidecar");
        }
        binary
    }

    #[test]
    fn a_signed_artifact_is_trusted_and_quotes_the_comment() {
        let dir = scratch("trusted");
        let s = Signer32::new(3, 0xAABB_CCDD_EEFF_0011);
        let binary = lay_out(&dir, Some(&s.sign(DATA, "file:avada-files")));
        match Minisign::new().verify(&binary, &ctx(vec![s.public()])) {
            Verdict::Trusted { by } => {
                assert!(by.contains("AABBCCDDEEFF0011"), "{by}");
                assert!(by.contains("file:avada-files"), "{by}");
            }
            other => panic!("expected trusted, got {other:?}"),
        }
    }

    #[test]
    fn no_sidecar_is_unsigned_not_an_error() {
        let dir = scratch("unsigned");
        let binary = lay_out(&dir, None);
        let s = Signer32::new(3, 1);
        assert_eq!(
            Minisign::new().verify(&binary, &ctx(vec![s.public()])),
            Verdict::Unsigned
        );
    }

    #[test]
    fn a_tampered_artifact_is_invalid() {
        let dir = scratch("tampered");
        let s = Signer32::new(3, 1);
        let binary = lay_out(&dir, Some(&s.sign(DATA, "file:avada-files")));
        std::fs::write(&binary, b"something else\n").expect("tamper");
        match Minisign::new().verify(&binary, &ctx(vec![s.public()])) {
            Verdict::Invalid { reason } => assert!(reason.contains("does not match"), "{reason}"),
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_signature_with_no_configured_key_is_unavailable_not_invalid() {
        // The artifact may be perfectly signed; the host simply has nothing to check
        // it against. Under RequireSignature that still refuses — but the message has
        // to send the operator to policy.json, not to the publisher.
        let dir = scratch("nokeys");
        let s = Signer32::new(3, 1);
        let binary = lay_out(&dir, Some(&s.sign(DATA, "file:avada-files")));
        match Minisign::new().verify(&binary, &ctx(vec![])) {
            Verdict::Unavailable { reason } => {
                assert!(reason.contains("acme/avada-files"), "{reason}")
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_sidecar_is_invalid() {
        let dir = scratch("corrupt");
        let s = Signer32::new(3, 1);
        let binary = lay_out(&dir, Some("untrusted comment: x\nnot base64 at all\n"));
        match Minisign::new().verify(&binary, &ctx(vec![s.public()])) {
            Verdict::Invalid { reason } => assert!(reason.contains("base64"), "{reason}"),
            other => panic!("expected invalid, got {other:?}"),
        }
    }
}
