//! The Windows verifier: Authenticode, asked through PowerShell for now.
//!
//! # Why a shell-out and not `WinVerifyTrust`
//!
//! The right implementation calls `WinVerifyTrust` from the `windows` crate, which
//! lives behind its `Win32_Security_WinTrust` feature. That feature is not enabled in
//! this workspace's `Cargo.toml`, and this track may not edit any `Cargo.toml`.
//! Rather than ship nothing on Windows, this asks `Get-AuthenticodeSignature`, which
//! is the same trust provider reached through a different door: it calls
//! `WinVerifyTrust` itself and reports the same `Status` values.
//!
//! The shell-out costs a process launch per verification and depends on PowerShell
//! being present. Both are acceptable at install time, which is the only place this
//! runs. Replacing it is a tracked follow-up in `docs/notarization.md`.
//!
//! The parsing is pure and tested on captured output on every platform; only
//! [`platform_verifier`](super::platform_verifier) picks this verifier on Windows.

use super::{Context, Verdict, Verifier};
use std::path::Path;
use std::process::Command;

/// Reads `Status=`, `StatusMessage=` and `Signer=` out of the script's output.
///
/// The script is written to emit exactly those three lines so that nothing here has to
/// parse PowerShell's default object formatting, which changes between versions.
pub fn parse_authenticode(output: &str) -> Verdict {
    let field = |name: &str| -> Option<String> {
        output.lines().find_map(|l| {
            l.trim()
                .strip_prefix(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let status = field("Status=").unwrap_or_default();
    let message = field("StatusMessage=");
    let signer = field("Signer=").map(|s| common_name(&s));

    let detail = || match &message {
        Some(m) => format!("{status}: {m}"),
        None => status.clone(),
    };

    match status.as_str() {
        "Valid" => Verdict::Trusted {
            by: signer.unwrap_or_else(|| "an Authenticode certificate".to_string()),
        },
        "NotSigned" => Verdict::Unsigned,
        // A file format Authenticode cannot carry a signature in is not an unsigned
        // file — it is a question that could not be asked.
        "NotSupportedFileFormat" | "Incompatible" => Verdict::Unavailable { reason: detail() },
        "" => Verdict::Unavailable {
            reason: "Get-AuthenticodeSignature reported no status".to_string(),
        },
        // HashMismatch, NotTrusted, UnknownError: a signature exists and does not hold.
        _ => Verdict::Invalid {
            reason: match signer {
                Some(who) => format!("{} (signed by {who})", detail()),
                None => detail(),
            },
        },
    }
}

/// The `CN=` of an X.509 subject, or the whole subject when it has none.
fn common_name(subject: &str) -> String {
    subject
        .split(", ")
        .find_map(|part| part.trim().strip_prefix("CN="))
        .map(|cn| cn.trim().to_string())
        .unwrap_or_else(|| subject.to_string())
}

/// Verifies an Authenticode signature via `Get-AuthenticodeSignature`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Authenticode;

impl Authenticode {
    /// A verifier with no state.
    pub fn new() -> Self {
        Authenticode
    }

    /// The PowerShell one-liner for `binary`.
    ///
    /// The path is passed as a single-quoted PowerShell literal with `'` doubled, and
    /// `-LiteralPath` stops PowerShell from treating `[`, `]` or `?` in the path as
    /// wildcards. A module directory name comes from an `owner/repo` id, so it cannot
    /// contain a quote today — but the escaping is here rather than in a comment
    /// because the day that stops being true must not be the day this becomes command
    /// injection.
    pub fn script(binary: &Path) -> String {
        let quoted = binary.to_string_lossy().replace('\'', "''");
        format!(
            "$ErrorActionPreference='Stop'; \
             $s = Get-AuthenticodeSignature -LiteralPath '{quoted}'; \
             Write-Output \"Status=$($s.Status)\"; \
             Write-Output \"StatusMessage=$($s.StatusMessage)\"; \
             Write-Output \"Signer=$($s.SignerCertificate.Subject)\""
        )
    }
}

impl Verifier for Authenticode {
    fn name(&self) -> &'static str {
        "authenticode"
    }

    fn verify(&self, binary: &Path, _ctx: &Context) -> Verdict {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(Self::script(binary))
            .output();
        match out {
            Ok(o) if o.status.success() => parse_authenticode(&String::from_utf8_lossy(&o.stdout)),
            Ok(o) => Verdict::Unavailable {
                reason: format!(
                    "Get-AuthenticodeSignature failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            },
            Err(e) => Verdict::Unavailable {
                reason: format!("cannot run powershell: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::ModuleId;

    // Captured from the three-line script above on Windows 11 / PowerShell 5.1.
    const VALID: &str = "\
Status=Valid
StatusMessage=Signature verified.
Signer=CN=Acme Software Ltd, O=Acme Software Ltd, L=Reading, C=GB";

    const NOT_SIGNED: &str = "\
Status=NotSigned
StatusMessage=The file C:\\avada\\avada-files.exe is not digitally signed. \
The file is not trusted on the current system.
Signer=";

    const HASH_MISMATCH: &str = "\
Status=HashMismatch
StatusMessage=The contents of file C:\\avada\\avada-files.exe might have been changed \
by an unauthorized user or process.
Signer=CN=Acme Software Ltd, O=Acme Software Ltd, C=GB";

    const NOT_TRUSTED: &str = "\
Status=NotTrusted
StatusMessage=A certificate chain processed, but terminated in a root certificate \
which is not trusted by the trust provider.
Signer=CN=Acme Test Signing, O=Acme, C=GB";

    const BAD_FORMAT: &str = "\
Status=NotSupportedFileFormat
StatusMessage=The form specified for the subject is not one supported or known by the \
specified trust provider.
Signer=";

    fn ctx() -> Context {
        Context {
            module: ModuleId::new("acme/avada-files").expect("id"),
            source: super::super::Source::Prebuilt {
                url: "https://example.invalid/a.zip".into(),
            },
            publisher_keys: vec![],
        }
    }

    #[test]
    fn a_valid_signature_is_named_by_its_common_name() {
        assert_eq!(
            parse_authenticode(VALID),
            Verdict::Trusted {
                by: "Acme Software Ltd".into()
            }
        );
    }

    #[test]
    fn an_unsigned_file_is_unsigned() {
        assert_eq!(parse_authenticode(NOT_SIGNED), Verdict::Unsigned);
    }

    #[test]
    fn a_changed_file_is_invalid_and_says_who_signed_it() {
        match parse_authenticode(HASH_MISMATCH) {
            Verdict::Invalid { reason } => {
                assert!(reason.contains("HashMismatch"), "{reason}");
                assert!(reason.contains("Acme Software Ltd"), "{reason}");
            }
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn an_untrusted_root_is_invalid_not_unsigned() {
        match parse_authenticode(NOT_TRUSTED) {
            Verdict::Invalid { reason } => assert!(reason.contains("NotTrusted"), "{reason}"),
            other => panic!("expected invalid, got {other:?}"),
        }
    }

    #[test]
    fn a_format_authenticode_cannot_sign_is_unavailable() {
        // Refusing this as "invalid" would be a lie: nothing was checked.
        match parse_authenticode(BAD_FORMAT) {
            Verdict::Unavailable { reason } => {
                assert!(reason.contains("NotSupportedFileFormat"), "{reason}")
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn empty_output_is_unavailable_rather_than_trusted() {
        match parse_authenticode("") {
            Verdict::Unavailable { .. } => {}
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_quote_in_the_path_cannot_close_the_powershell_literal() {
        let script = Authenticode::script(Path::new("C:\\a'; Remove-Item C:\\ #\\avada.exe"));
        // Anchored on the character before the quote: the escaped form is `a''; `, so
        // the un-escaped `a'; ` — the one that would end the literal early — is absent.
        assert!(script.contains("'C:\\a''; Remove-Item"), "{script}");
        assert!(!script.contains("C:\\a'; Remove-Item"), "{script}");
        assert!(script.contains("-LiteralPath"), "{script}");
        // Every quote is either the pair around the literal or half of a doubled one,
        // so the total is even; an odd count would mean the literal closed somewhere.
        assert_eq!(script.matches('\'').count() % 2, 0, "{script}");
    }

    #[test]
    #[ignore = "needs a real Windows host with PowerShell; run with --ignored"]
    fn the_real_powershell_assesses_a_real_binary() {
        // notepad.exe is Microsoft-signed on every Windows install.
        let v = Authenticode::new().verify(Path::new("C:\\Windows\\notepad.exe"), &ctx());
        assert!(
            matches!(v, Verdict::Trusted { .. }),
            "expected notepad.exe to be trusted, got {v:?}"
        );
    }
}
