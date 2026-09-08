//! The macOS verifier: ask Gatekeeper, then ask who signed it.
//!
//! Two questions, two tools. `spctl --assess --type execute` is the OS's own verdict —
//! it knows about notarization, revocation and quarantine, and none of that can be
//! reconstructed from a certificate alone. `codesign -dv --verbose=2` then supplies
//! the *name* to put in [`Verdict::Trusted`], because "accepted" without an authority
//! tells a user nothing about who they are trusting.
//!
//! Both tools write their interesting output to **stderr**, and both are parsed by the
//! pure functions below so the parsing can be tested on captured fixtures on any
//! platform. Running the tools is `cfg`-free too; only
//! [`platform_verifier`](super::platform_verifier) picks this one on macOS.

use super::{Context, Verdict, Verifier};
use std::path::Path;
use std::process::Command;

/// Gatekeeper's answer, separated from the text it arrived in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// Gatekeeper would let this run.
    Accepted {
        /// The `source=` line, e.g. `Notarized Developer ID`.
        source: Option<String>,
    },
    /// Gatekeeper would not.
    Rejected {
        /// Its stated reason.
        reason: String,
    },
    /// There is no signature at all — a different thing from a rejected one.
    Unsigned,
}

/// What `codesign -dv` disclosed about the signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Codesign {
    /// The `Authority=` chain, leaf first, exactly as `codesign` ordered it.
    pub authorities: Vec<String>,
    /// The `TeamIdentifier=`, absent when unset or ad-hoc.
    pub team_id: Option<String>,
    /// The `Identifier=` (the signed bundle/binary identifier).
    pub identifier: Option<String>,
    /// Whether the signature is ad-hoc (`Signature=adhoc`) — cryptographically real
    /// but vouched for by nobody.
    pub adhoc: bool,
    /// Whether `codesign` said the object is not signed at all.
    pub unsigned: bool,
}

impl Codesign {
    /// The leaf authority — the name a human recognises.
    pub fn signer(&self) -> Option<&str> {
        self.authorities.first().map(String::as_str)
    }
}

/// Parse `spctl --assess --type execute -vv` output together with its exit status.
///
/// `spctl` exits non-zero for both "rejected" and "unsigned", so the text is the only
/// thing that separates them, and the difference matters: an unsigned locally built
/// module is normal, a rejected one is not.
pub fn parse_spctl(exit_ok: bool, output: &str) -> Assessment {
    let lower = output.to_ascii_lowercase();
    // Only ever read as unsigned on a failed assessment, and only on the phrase the
    // tool actually prints — a *path* containing the word "unsigned" must not decide
    // this.
    if !exit_ok && (lower.contains("not signed at all") || lower.contains("code object is not")) {
        return Assessment::Unsigned;
    }
    if exit_ok {
        let source = output
            .lines()
            .find_map(|l| l.trim().strip_prefix("source="))
            .map(|s| s.trim().to_string());
        return Assessment::Accepted { source };
    }
    // "path: rejected" / "path: rejected (the code is valid but does not seem to be
    // an app)" — keep whatever follows the colon, else the whole thing.
    let reason = output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.split_once(": ").map(|(_, rest)| rest.to_string()))
        .unwrap_or_else(|| output.trim().to_string());
    Assessment::Rejected {
        reason: if reason.is_empty() {
            "rejected by Gatekeeper".to_string()
        } else {
            reason
        },
    }
}

/// Parse `codesign -dv --verbose=2` output.
pub fn parse_codesign(output: &str) -> Codesign {
    let mut out = Codesign::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Authority=") {
            out.authorities.push(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("TeamIdentifier=") {
            let v = v.trim();
            if !v.is_empty() && v != "not set" {
                out.team_id = Some(v.to_string());
            }
        } else if let Some(v) = line.strip_prefix("Identifier=") {
            out.identifier = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Signature=") {
            out.adhoc = v.trim() == "adhoc";
        } else if line.to_ascii_lowercase().contains("not signed at all") {
            out.unsigned = true;
        }
    }
    out
}

/// Turn both tools' answers into one verdict.
pub fn combine(assessment: &Assessment, signature: &Codesign) -> Verdict {
    match assessment {
        Assessment::Unsigned => Verdict::Unsigned,
        Assessment::Accepted { source } => {
            // An ad-hoc signature can never be accepted by Gatekeeper, so if we are
            // here the chain is real; name its leaf.
            let by = signature
                .signer()
                .map(|s| match &signature.team_id {
                    Some(team) => format!("{s} [{team}]"),
                    None => s.to_string(),
                })
                .or_else(|| source.clone())
                .unwrap_or_else(|| "Gatekeeper".to_string());
            Verdict::Trusted { by }
        }
        Assessment::Rejected { reason } => {
            if signature.unsigned || (signature.authorities.is_empty() && !signature.adhoc) {
                Verdict::Unsigned
            } else if signature.adhoc {
                Verdict::Invalid {
                    reason: "ad-hoc signed: the signature is vouched for by no authority"
                        .to_string(),
                }
            } else {
                Verdict::Invalid {
                    reason: match signature.signer() {
                        Some(who) => format!("{reason} (signed by {who})"),
                        None => reason.clone(),
                    },
                }
            }
        }
    }
}

/// Asks Gatekeeper via `spctl`, and `codesign` for the authority chain.
#[derive(Debug, Default, Clone, Copy)]
pub struct Spctl;

impl Spctl {
    /// A verifier with no state.
    pub fn new() -> Self {
        Spctl
    }
}

impl Verifier for Spctl {
    fn name(&self) -> &'static str {
        "spctl"
    }

    fn verify(&self, binary: &Path, _ctx: &Context) -> Verdict {
        let assessed = Command::new("spctl")
            .args(["--assess", "--type", "execute", "-vv", "--"])
            .arg(binary)
            .output();
        let assessed = match assessed {
            Ok(o) => o,
            Err(e) => {
                return Verdict::Unavailable {
                    reason: format!("cannot run spctl: {e}"),
                }
            }
        };
        // Both tools speak on stderr; join anyway so a future macOS that moves the
        // text does not silently turn every module unsigned.
        let spctl_text = merge(&assessed.stdout, &assessed.stderr);
        let assessment = parse_spctl(assessed.status.success(), &spctl_text);

        let signature = match Command::new("codesign")
            .args(["-dv", "--verbose=2", "--"])
            .arg(binary)
            .output()
        {
            Ok(o) => parse_codesign(&merge(&o.stdout, &o.stderr)),
            // spctl already answered; a missing codesign only costs us the name.
            Err(_) => Codesign::default(),
        };
        combine(&assessment, &signature)
    }
}

fn merge(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stderr).into_owned();
    if !stdout.is_empty() {
        s.push('\n');
        s.push_str(&String::from_utf8_lossy(stdout));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::ModuleId;

    // Captured from a real macOS 15 host. Kept verbatim: the parser exists to survive
    // this exact shape, so paraphrasing it would test nothing.
    const ACCEPTED: &str = "\
/Applications/Avada.app: accepted
source=Notarized Developer ID
origin=Developer ID Application: Acme Software Ltd (AB12CD34EF)";

    const REJECTED: &str = "/tmp/avada-files: rejected\nsource=no usable signature";

    const NOT_SIGNED: &str = "/tmp/avada-files: code object is not signed at all";

    const CODESIGN_SIGNED: &str = "\
Executable=/Applications/Avada.app/Contents/MacOS/avada
Identifier=to.avada.terminal
Format=app bundle with Mach-O universal (x86_64 arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=30+7 location=embedded
Signature size=9061
Authority=Developer ID Application: Acme Software Ltd (AB12CD34EF)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=1 Sep 2026 at 10:11:12
Info.plist entries=24
TeamIdentifier=AB12CD34EF
Runtime Version=15.0.0
Sealed Resources version=2 rules=13 files=48
Internal requirements count=1 size=180";

    const CODESIGN_ADHOC: &str = "\
Executable=/tmp/avada-files
Identifier=avada-files
CodeDirectory v=20400 size=600 flags=0x2(adhoc) hashes=14+2 location=embedded
Signature=adhoc
Info.plist=not bound
TeamIdentifier=not set";

    const CODESIGN_UNSIGNED: &str = "/tmp/avada-files: code object is not signed at all";

    fn ctx() -> Context {
        Context {
            module: ModuleId::new("acme/avada-files").expect("id"),
            source: super::super::Source::Built {
                commit: "0".repeat(40),
            },
            publisher_keys: vec![],
        }
    }

    #[test]
    fn an_accepted_assessment_keeps_its_source() {
        assert_eq!(
            parse_spctl(true, ACCEPTED),
            Assessment::Accepted {
                source: Some("Notarized Developer ID".into())
            }
        );
    }

    #[test]
    fn a_rejection_keeps_its_reason() {
        assert_eq!(
            parse_spctl(false, REJECTED),
            Assessment::Rejected {
                reason: "rejected".into()
            }
        );
    }

    #[test]
    fn an_unsigned_object_is_not_a_rejection() {
        // spctl exits non-zero either way; only the text distinguishes them, and
        // conflating the two would make every locally built module look forged.
        assert_eq!(parse_spctl(false, NOT_SIGNED), Assessment::Unsigned);
    }

    #[test]
    fn the_authority_chain_is_read_leaf_first() {
        let cs = parse_codesign(CODESIGN_SIGNED);
        assert_eq!(
            cs.signer(),
            Some("Developer ID Application: Acme Software Ltd (AB12CD34EF)")
        );
        assert_eq!(cs.authorities.len(), 3);
        assert_eq!(cs.authorities[2], "Apple Root CA");
        assert_eq!(cs.team_id.as_deref(), Some("AB12CD34EF"));
        assert_eq!(cs.identifier.as_deref(), Some("to.avada.terminal"));
        assert!(!cs.adhoc);
    }

    #[test]
    fn an_adhoc_signature_has_no_authority_and_no_team() {
        let cs = parse_codesign(CODESIGN_ADHOC);
        assert!(cs.adhoc);
        assert!(cs.authorities.is_empty());
        assert_eq!(cs.team_id, None, "`not set` must not become a team id");
    }

    #[test]
    fn accepted_plus_a_chain_names_the_signer_and_the_team() {
        let v = combine(
            &parse_spctl(true, ACCEPTED),
            &parse_codesign(CODESIGN_SIGNED),
        );
        assert_eq!(
            v,
            Verdict::Trusted {
                by: "Developer ID Application: Acme Software Ltd (AB12CD34EF) [AB12CD34EF]".into()
            }
        );
    }

    #[test]
    fn an_unsigned_binary_is_unsigned_from_both_tools() {
        assert_eq!(
            combine(
                &parse_spctl(false, NOT_SIGNED),
                &parse_codesign(CODESIGN_UNSIGNED)
            ),
            Verdict::Unsigned
        );
    }

    #[test]
    fn an_adhoc_signature_is_invalid_rather_than_unsigned() {
        // It really is signed — just by nobody. Calling it unsigned would let it
        // through a policy that tolerates unsigned local builds while quietly
        // accepting a forged-looking artifact.
        match combine(
            &parse_spctl(false, REJECTED),
            &parse_codesign(CODESIGN_ADHOC),
        ) {
            Verdict::Invalid { reason } => assert!(reason.contains("ad-hoc"), "{reason}"),
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_but_signed_binary_names_who_signed_it() {
        match combine(
            &parse_spctl(false, REJECTED),
            &parse_codesign(CODESIGN_SIGNED),
        ) {
            Verdict::Invalid { reason } => {
                assert!(reason.contains("rejected"), "{reason}");
                assert!(reason.contains("Acme Software Ltd"), "{reason}");
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "needs a real macOS host; run with --ignored"]
    fn the_real_spctl_assesses_a_real_binary() {
        // /bin/ls is signed by Apple on every macOS, so this asserts the whole
        // shell-out path, not just the parser.
        let v = Spctl::new().verify(Path::new("/bin/ls"), &ctx());
        assert!(
            matches!(v, Verdict::Trusted { .. }),
            "expected /bin/ls to be trusted, got {v:?}"
        );
    }
}
