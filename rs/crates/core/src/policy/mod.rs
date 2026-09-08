//! Policy traits with open-source implementations (docs/modules-fanout-plan.md §2
//! "Free vs commercial", track G7): what the commercial crate swaps at build time.
//! First occupant: the artifact notarization verifier (macOS codesign/spctl + Team ID,
//! Windows Authenticode, Linux minisign) behind one trait, with the free build's
//! "source-compiled only" policy as the default.
//!
//! # The question this module answers
//!
//! Before the host runs a module binary, someone has to say whether the operating
//! system considers it *signed by someone*. That is three different questions on three
//! platforms, so it is one trait ([`Verifier`]) with three implementations
//! ([`macos::Spctl`], [`windows::Authenticode`], [`linux::Minisign`]), each answering
//! with a [`Verdict`] and never with a decision.
//!
//! The decision is separate and pure: [`decide`] maps (policy, verdict, source) to a
//! [`Decision`]. That split is deliberate — the verifiers shell out to the OS and
//! cannot be unit-tested cheaply, while the matrix that actually refuses to run code
//! is a table, and it is tested exhaustively.
//!
//! # Provenance beats signatures for source builds
//!
//! The marketplace's free path clones a tag, checks the clone is at the commit
//! `ls-remote` announced, and runs `cargo build` locally. The provenance of that
//! artifact *is* the commit, and nothing a publisher signs adds to it — so
//! [`Source::Built`] is [`Rule::Allow`] by default. [`Source::Prebuilt`] (a binary
//! someone else compiled) is [`Rule::RequireSignature`]: the host never watched it
//! being made, so the signature is all there is.
//!
//! # On disk
//!
//! `<modules root>/policy.json` ([`PolicyFile`]); absent means [`Policy::default`].
//! The verdict reached at install time is recorded next to the installed artifact as
//! `<binary>.notarization.json` ([`RecordedVerdict`]) so the spawn path can refuse a
//! binary that was installed under a refusal. See `docs/notarization.md`.

pub mod linux;
pub mod macos;
pub mod minisign;
pub mod windows;

use avada_module_sdk::ModuleId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use minisign::{MinisignError, PublicKey};

/// `<modules root>/policy.json`.
pub const POLICY_FILE: &str = "policy.json";
/// The suffix appended to an installed artifact's path for its recorded verdict.
pub const VERDICT_SUFFIX: &str = ".notarization.json";

// ---- what a verifier answers

/// What one platform verifier concluded about one file. A verdict is an observation,
/// never a decision: [`decide`] turns it into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The OS (or a publisher key) vouches for the binary. `by` names the authority:
    /// a macOS authority chain leaf, an Authenticode subject, a minisign trusted
    /// comment.
    Trusted {
        /// Who vouched.
        by: String,
    },
    /// No signature at all. Not an error — the common case for a locally built module.
    Unsigned,
    /// A signature exists and does not hold: wrong key, tampered file, revoked
    /// certificate, or an algorithm this build cannot check.
    Invalid {
        /// What went wrong, in words a user can act on.
        reason: String,
    },
    /// The verifier could not reach an answer: the OS tool is missing, it failed to
    /// run, or the policy named no publisher key to check against.
    Unavailable {
        /// Why no answer was reached.
        reason: String,
    },
}

impl Verdict {
    /// A short stable word for logs, the recorded verdict file and the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            Verdict::Trusted { .. } => "trusted",
            Verdict::Unsigned => "unsigned",
            Verdict::Invalid { .. } => "invalid",
            Verdict::Unavailable { .. } => "unavailable",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Trusted { by } => write!(f, "trusted (signed by {by})"),
            Verdict::Unsigned => f.write_str("unsigned"),
            Verdict::Invalid { reason } => write!(f, "invalid signature: {reason}"),
            Verdict::Unavailable { reason } => write!(f, "not verifiable: {reason}"),
        }
    }
}

/// Where the artifact under assessment came from. This, not the file, decides which
/// half of the [`Policy`] applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Compiled on this machine from a checkout of `commit`. The commit is the
    /// provenance.
    Built {
        /// The commit the clone was verified to be at.
        commit: String,
    },
    /// Downloaded already compiled from `url`. Nothing but a signature vouches for it.
    Prebuilt {
        /// Where the artifact was fetched from.
        url: String,
    },
}

impl Source {
    /// A short stable word for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Source::Built { .. } => "built",
            Source::Prebuilt { .. } => "prebuilt",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Built { commit } => write!(f, "built here from {commit}"),
            Source::Prebuilt { url } => write!(f, "prebuilt from {url}"),
        }
    }
}

/// Everything a verifier is told about the artifact besides its path.
#[derive(Debug, Clone)]
pub struct Context {
    /// Whose module it is.
    pub module: ModuleId,
    /// How the artifact came to exist.
    pub source: Source,
    /// The keys this module's publisher is allowed to sign with (minisign only).
    /// Empty means "no key configured", which [`linux::Minisign`] reports as
    /// [`Verdict::Unavailable`] rather than pretending the file is unsigned.
    pub publisher_keys: Vec<PublicKey>,
}

/// One platform's answer to "does this OS think the binary is signed by someone".
///
/// Implementations shell out; none of them mutate anything, and none of them panic on
/// hostile output. The commercial build swaps a stricter implementation in (Developer
/// ID Team ID pinning, EV certificate pinning) behind this same trait.
pub trait Verifier: fmt::Debug + Send + Sync {
    /// Assess `binary` in `ctx`. Never fails: an inability to answer is a
    /// [`Verdict::Unavailable`].
    fn verify(&self, binary: &Path, ctx: &Context) -> Verdict;

    /// A short name for logs (`spctl`, `authenticode`, `minisign`, `fake`).
    fn name(&self) -> &'static str;
}

/// The verifier for the platform this host was built for.
pub fn platform_verifier() -> Arc<dyn Verifier> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::Spctl::new())
    }
    #[cfg(windows)]
    {
        Arc::new(windows::Authenticode::new())
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        Arc::new(linux::Minisign::new())
    }
}

// ---- the policy

/// What to do with one class of artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    /// Run it whatever the verdict says — except that a broken signature still warns:
    /// a file carrying a signature that does not hold is news even when nothing
    /// required one.
    #[default]
    Allow,
    /// Run it, but say so when it is not trusted.
    Warn,
    /// Only a [`Verdict::Trusted`] runs.
    RequireSignature,
}

impl Rule {
    /// A short stable word for logs and `policy.json`.
    pub fn kind(&self) -> &'static str {
        match self {
            Rule::Allow => "allow",
            Rule::Warn => "warn",
            Rule::RequireSignature => "require-signature",
        }
    }
}

/// Where publisher keys come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KeysSource {
    /// The manifest's declared keys if it ever grows the field, then `policy.json`'s
    /// `publishers` map. Today the manifest has no such field — the SDK is frozen and
    /// `[distribution]` is `deny_unknown_fields` — so this is `policy.json` only. See
    /// the follow-up in `docs/notarization.md`.
    #[default]
    ManifestOrDefaults,
    /// `policy.json`'s `publishers` map and nothing else.
    PolicyFileOnly,
}

/// `policy.json` as it is written, before its keys are parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
pub struct PolicyFile {
    /// What to do with an artifact this host compiled.
    pub locally_built: Rule,
    /// What to do with an artifact someone else compiled.
    pub prebuilt: Rule,
    /// Where publisher keys come from.
    pub publisher_keys_source: KeysSource,
    /// `owner/repo` → minisign public keys (`RW…`, bare or with the two-line file
    /// wrapper). A key listed here may sign that module and no other.
    pub publishers: BTreeMap<String, Vec<String>>,
}

impl Default for PolicyFile {
    fn default() -> Self {
        PolicyFile {
            locally_built: Rule::Allow,
            prebuilt: Rule::RequireSignature,
            publisher_keys_source: KeysSource::ManifestOrDefaults,
            publishers: BTreeMap::new(),
        }
    }
}

/// The runtime policy: a [`PolicyFile`] whose publisher keys have been parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// What to do with an artifact this host compiled.
    pub locally_built: Rule,
    /// What to do with an artifact someone else compiled.
    pub prebuilt: Rule,
    /// Where publisher keys come from.
    pub publisher_keys_source: KeysSource,
    /// `owner/repo` → the keys allowed to sign it.
    pub publishers: BTreeMap<String, Vec<PublicKey>>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            locally_built: Rule::Allow,
            prebuilt: Rule::RequireSignature,
            publisher_keys_source: KeysSource::ManifestOrDefaults,
            publishers: BTreeMap::new(),
        }
    }
}

/// Why a policy could not be loaded. A malformed `policy.json` is never silently
/// ignored: the caller logs it and falls back to [`Policy::default`], which is the
/// strict one for prebuilt artifacts — and has no keys, so nothing prebuilt runs.
#[derive(Debug)]
pub enum PolicyError {
    /// The file could not be read.
    Io(std::io::Error),
    /// The file is not the JSON this version understands.
    Malformed(String),
    /// A `publishers` entry is not a minisign public key.
    BadKey {
        /// The `owner/repo` it was listed under.
        module: String,
        /// Why it did not parse.
        reason: MinisignError,
    },
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::Io(e) => write!(f, "cannot read the notarization policy: {e}"),
            PolicyError::Malformed(e) => write!(f, "notarization policy is malformed: {e}"),
            PolicyError::BadKey { module, reason } => {
                write!(f, "publisher key for {module}: {reason}")
            }
        }
    }
}
impl std::error::Error for PolicyError {}

impl Policy {
    /// `<modules root>/policy.json`.
    pub fn path_under(modules_root: &Path) -> PathBuf {
        modules_root.join(POLICY_FILE)
    }

    /// Read the policy under `modules_root`. A missing file is [`Policy::default`];
    /// a malformed one is an error, never a silent default.
    pub fn load(modules_root: &Path) -> Result<Policy, PolicyError> {
        let path = Self::path_under(modules_root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Policy::default()),
            Err(e) => return Err(PolicyError::Io(e)),
        };
        let file: PolicyFile =
            serde_json::from_str(&text).map_err(|e| PolicyError::Malformed(e.to_string()))?;
        Policy::from_file(file)
    }

    /// Read the policy, falling back to the default and returning the complaint so the
    /// caller can log it. Fail-closed: the default refuses every prebuilt artifact.
    pub fn load_or_default(modules_root: &Path) -> (Policy, Option<String>) {
        match Policy::load(modules_root) {
            Ok(p) => (p, None),
            Err(e) => (Policy::default(), Some(e.to_string())),
        }
    }

    /// Parse the keys of a [`PolicyFile`].
    pub fn from_file(file: PolicyFile) -> Result<Policy, PolicyError> {
        let mut publishers = BTreeMap::new();
        for (module, keys) in file.publishers {
            let mut parsed = Vec::with_capacity(keys.len());
            for key in keys {
                parsed.push(minisign::parse_public_key(&key).map_err(|reason| {
                    PolicyError::BadKey {
                        module: module.clone(),
                        reason,
                    }
                })?);
            }
            publishers.insert(module, parsed);
        }
        Ok(Policy {
            locally_built: file.locally_built,
            prebuilt: file.prebuilt,
            publisher_keys_source: file.publisher_keys_source,
            publishers,
        })
    }

    /// The rule that governs `source`.
    pub fn rule_for(&self, source: &Source) -> Rule {
        match source {
            Source::Built { .. } => self.locally_built,
            Source::Prebuilt { .. } => self.prebuilt,
        }
    }

    /// The keys `id`'s publisher may sign with. Empty when none is configured.
    pub fn keys_for(&self, id: &ModuleId) -> Vec<PublicKey> {
        self.publishers
            .get(id.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// The whole [`Context`] for one artifact.
    pub fn context(&self, id: &ModuleId, source: Source) -> Context {
        Context {
            publisher_keys: self.keys_for(id),
            module: id.clone(),
            source,
        }
    }
}

// ---- the decision

/// What the host does about one verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Install or spawn it, silently.
    Run,
    /// Do not. In the marketplace this fails the install job; at spawn time it makes
    /// the module `Broken`.
    Refuse {
        /// What to tell the user.
        reason: String,
    },
    /// Proceed, but say so in the job log and the recorded verdict.
    Warn {
        /// What to tell the user.
        reason: String,
    },
}

impl Decision {
    /// A short stable word for logs and the recorded verdict file.
    pub fn kind(&self) -> &'static str {
        match self {
            Decision::Run => "run",
            Decision::Refuse { .. } => "refuse",
            Decision::Warn { .. } => "warn",
        }
    }

    /// The user-facing half, empty for [`Decision::Run`].
    pub fn reason(&self) -> &str {
        match self {
            Decision::Run => "",
            Decision::Refuse { reason } | Decision::Warn { reason } => reason,
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Run => f.write_str("run"),
            Decision::Refuse { reason } => write!(f, "refuse: {reason}"),
            Decision::Warn { reason } => write!(f, "warn: {reason}"),
        }
    }
}

/// The whole matrix, in one place.
///
/// | rule \ verdict | `Trusted` | `Unsigned` | `Invalid` | `Unavailable` |
/// |---|---|---|---|---|
/// | `Allow` | run | run | **warn** | run |
/// | `Warn` | run | warn | warn | warn |
/// | `RequireSignature` | run | refuse | refuse | refuse |
///
/// The one cell worth arguing about is `Allow` × `Invalid`: nothing required a
/// signature, yet the file carries one that does not hold. Silence would be wrong —
/// somebody signed this and the signature broke — so it warns.
///
/// `RequireSignature` × `Unavailable` refuses. A verifier that could not run is not
/// evidence of trust, and a host that reads "I could not check" as "fine" has no
/// policy at all.
pub fn decide(policy: &Policy, verdict: &Verdict, source: &Source) -> Decision {
    let rule = policy.rule_for(source);
    let what = format!("{verdict} ({source})");
    match (rule, verdict) {
        (_, Verdict::Trusted { .. }) => Decision::Run,
        (Rule::Allow, Verdict::Invalid { .. }) => Decision::Warn { reason: what },
        (Rule::Allow, _) => Decision::Run,
        (Rule::Warn, _) => Decision::Warn { reason: what },
        (Rule::RequireSignature, _) => Decision::Refuse {
            reason: format!(
                "{what}; this host requires a signature for {} artifacts",
                source.kind()
            ),
        },
    }
}

// ---- what gets remembered

/// The verdict and decision reached when an artifact was installed, written next to
/// the installed binary.
///
/// It is deliberately *not* part of the MAC-signed install record: that record is
/// frozen SDK shape (`avada_module_sdk::rights::InstallRecord`) and this track may not
/// change it. The consequence is stated plainly in `docs/notarization.md` — deleting
/// this file only returns the artifact to the pre-G7 status quo (hash check only); it
/// can never turn a refusal into a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedVerdict {
    /// [`Verdict::kind`].
    pub verdict: String,
    /// The verdict in words.
    pub detail: String,
    /// [`Decision::kind`].
    pub decision: String,
    /// The decision in words; empty for `run`.
    pub reason: String,
    /// Which verifier answered.
    pub verifier: String,
    /// [`Source::kind`].
    pub source: String,
    /// Seconds since the epoch.
    pub at: u64,
}

impl RecordedVerdict {
    /// Freeze one assessment.
    pub fn new(verdict: &Verdict, decision: &Decision, source: &Source, verifier: &str) -> Self {
        RecordedVerdict {
            verdict: verdict.kind().to_string(),
            detail: verdict.to_string(),
            decision: decision.kind().to_string(),
            reason: decision.reason().to_string(),
            verifier: verifier.to_string(),
            source: source.kind().to_string(),
            at: now_secs(),
        }
    }

    /// Whether this records a refusal.
    pub fn refused(&self) -> bool {
        self.decision == "refuse"
    }
}

/// `<binary><VERDICT_SUFFIX>`.
pub fn verdict_path(binary: &Path) -> PathBuf {
    let mut name = binary.as_os_str().to_os_string();
    name.push(VERDICT_SUFFIX);
    PathBuf::from(name)
}

/// Write the assessment next to `binary`.
pub fn write_verdict(binary: &Path, verdict: &RecordedVerdict) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(verdict)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(verdict_path(binary), text)
}

/// Read the assessment beside `binary`, if there is a readable one.
pub fn read_verdict(binary: &Path) -> Option<RecordedVerdict> {
    let text = std::fs::read_to_string(verdict_path(binary)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The reason `binary` must not be spawned, when its install-time verdict was a
/// refusal. `None` means "nothing recorded, or nothing to complain about" — this
/// function can only ever add a refusal, never remove one.
pub fn recorded_refusal(binary: &Path) -> Option<String> {
    let recorded = read_verdict(binary)?;
    if !recorded.refused() {
        return None;
    }
    // The reason is the decision in words; the detail is the verdict in words. A
    // refusal always has the former, but fall back rather than return an empty string.
    Some(if recorded.reason.is_empty() {
        recorded.detail
    } else {
        recorded.reason
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
