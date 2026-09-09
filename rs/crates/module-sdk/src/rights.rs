//! The trust chain: what the user accepted, signed so a module cannot widen it by
//! editing a file.
//!
//! At install the host mints an [`InstallRecord`] from the manifest and the user's
//! choices, then wraps it in a [`SignedInstallRecord`] using an HMAC-SHA256 key that
//! lives in the OS keychain (never on disk in the clear). At every launch the host
//! verifies the signature, compares the module's hello manifest against the record,
//! and passes only the accepted capabilities to the policy layer.
//!
//! An update whose manifest adds capabilities is not applied; it is parked as a
//! [`HeldUpdate`] until the user accepts the new set.

use crate::caps::{Capability, RightValue};
use crate::manifest::{DistributionKind, Manifest, ModuleId};
use hmac::{Hmac, Mac};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// Why a module was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallKind {
    /// The user asked for it.
    Manual,
    /// Pulled in by another module's `[dependencies]`; removed when nothing needs it.
    Dependency,
}

/// The facts fixed at install time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecord {
    /// `owner/repo`.
    pub module_id: ModuleId,
    /// The clone URL that was used.
    pub repo: String,
    /// The git tag (`v1.2.0`).
    pub tag: String,
    /// The commit the tag pointed at. Tags move; commits do not.
    pub commit: String,
    /// The manifest version, equal to the tag without its `v`.
    pub version: Version,
    /// SHA-256 (hex) of the built or downloaded binary.
    pub artifact_sha256: String,
    /// SHA-256 (hex) over the staged skill tree — every file the manifest's `[skills]
    /// paths` brought into the install, keyed by its path as well as its bytes.
    ///
    /// The artifact hash covers the binary and nothing else, but a module's skills are
    /// instructions handed to an agent: editing one after install changes what the agent
    /// is told to do without touching a single byte the host was checking. This is the
    /// hash that closes that, and it is inside the signed payload, so widening it needs
    /// the keychain key rather than a text editor.
    ///
    /// Empty means nothing is pinned — either the module ships no skills, or the record
    /// was minted before this field existed. Old records stay verifiable rather than
    /// turning into a wall of broken installs on upgrade; a *forged* empty is not a way
    /// in, because clearing the field means re-signing the record.
    #[serde(default)]
    pub skills_sha256: String,
    /// Source or binary.
    pub source: DistributionKind,
    /// The capabilities the user accepted. A subset of the manifest's request.
    pub accepted: BTreeSet<Capability>,
    /// The manifest as installed, so hellos can be compared byte-for-byte.
    pub manifest: Manifest,
    /// Unix seconds.
    pub installed_at: u64,
    /// Manual or dependency.
    pub kind: InstallKind,
}

impl InstallRecord {
    /// The bytes that are signed: canonical JSON (serde_json with sorted map keys —
    /// every map in this struct is a `BTreeMap`/`BTreeSet`, so field order is fixed by
    /// the struct definition and key order by the tree).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("install record is always serializable")
    }

    /// Capabilities the manifest asks for that the user did not accept.
    pub fn declined(&self) -> BTreeSet<Capability> {
        self.manifest
            .capabilities
            .iter()
            .filter(|c| !self.accepted.contains(c))
            .copied()
            .collect()
    }

    /// True if the manifest the module presented at launch is the one that was installed.
    pub fn matches_hello(&self, presented: &Manifest) -> bool {
        &self.manifest == presented
    }
}

/// An install record plus its HMAC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedInstallRecord {
    /// The payload.
    pub record: InstallRecord,
    /// Hex HMAC-SHA256 over [`InstallRecord::canonical_bytes`].
    pub mac: String,
    /// Which key signed it (so rotation can verify old records).
    pub key_id: String,
}

/// Verification failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RightsError {
    /// The MAC does not match; the file was edited or the key is wrong.
    BadSignature,
    /// The hello manifest differs from the installed one.
    ManifestMismatch,
    /// The record asks for something the manifest does not.
    AcceptedNotRequested(Capability),
    /// The key is unusable.
    BadKey,
}

impl fmt::Display for RightsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RightsError::BadSignature => f.write_str("install record signature does not verify"),
            RightsError::ManifestMismatch => {
                f.write_str("module presented a manifest that differs from its install record")
            }
            RightsError::AcceptedNotRequested(c) => {
                write!(
                    f,
                    "install record accepts `{c}` which the manifest never requested"
                )
            }
            RightsError::BadKey => f.write_str("signing key is unusable"),
        }
    }
}
impl std::error::Error for RightsError {}

impl SignedInstallRecord {
    /// Sign a record. `key` is the raw HMAC key from the keychain; it is never logged.
    pub fn sign(record: InstallRecord, key: &[u8], key_id: &str) -> Result<Self, RightsError> {
        for c in &record.accepted {
            if !record.manifest.capabilities.contains(c) {
                return Err(RightsError::AcceptedNotRequested(*c));
            }
        }
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| RightsError::BadKey)?;
        mac.update(&record.canonical_bytes());
        let tag = mac.finalize().into_bytes();
        Ok(SignedInstallRecord {
            record,
            mac: hex(&tag),
            key_id: key_id.to_string(),
        })
    }

    /// Verify and unwrap. Constant-time compare via `hmac::Mac::verify_slice`.
    pub fn verify(&self, key: &[u8]) -> Result<&InstallRecord, RightsError> {
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| RightsError::BadKey)?;
        mac.update(&self.record.canonical_bytes());
        let want = unhex(&self.mac).ok_or(RightsError::BadSignature)?;
        mac.verify_slice(&want)
            .map_err(|_| RightsError::BadSignature)?;
        for c in &self.record.accepted {
            if !self.record.manifest.capabilities.contains(c) {
                return Err(RightsError::AcceptedNotRequested(*c));
            }
        }
        Ok(&self.record)
    }
}

/// An update that would widen the module's capabilities, parked until accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldUpdate {
    /// The module.
    pub module_id: ModuleId,
    /// Installed version.
    pub from: Version,
    /// Available version.
    pub to: Version,
    /// Capabilities the new manifest requests that the old one did not.
    pub added: BTreeSet<Capability>,
    /// Capabilities the new manifest dropped (informational).
    pub removed: BTreeSet<Capability>,
    /// The new manifest, so acceptance can mint the next record without a refetch.
    pub manifest: Manifest,
}

impl HeldUpdate {
    /// Compare an installed record to a candidate manifest. `None` means the update
    /// widens nothing and may be applied silently.
    pub fn check(installed: &InstallRecord, candidate: &Manifest) -> Option<HeldUpdate> {
        let old: BTreeSet<_> = installed.manifest.capabilities.iter().copied().collect();
        let new: BTreeSet<_> = candidate.capabilities.iter().copied().collect();
        let added: BTreeSet<_> = new.difference(&old).copied().collect();
        if added.is_empty() {
            return None;
        }
        Some(HeldUpdate {
            module_id: installed.module_id.clone(),
            from: installed.version.clone(),
            to: candidate.module.version.clone(),
            added,
            removed: old.difference(&new).copied().collect(),
            manifest: candidate.clone(),
        })
    }
}

/// A named bundle of right values a publisher ships (`[[profiles]]`) or a user saves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    /// Display name.
    pub name: String,
    /// One line.
    #[serde(default)]
    pub description: String,
    /// Capability → value.
    #[serde(default)]
    pub values: BTreeMap<Capability, RightValue>,
}

/// The user's per-module right values, layered: profile, then explicit overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModuleRights {
    /// A profile name applied first, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Explicit values that beat the profile.
    #[serde(default)]
    pub overrides: BTreeMap<Capability, RightValue>,
}

impl ModuleRights {
    /// The effective value for one capability given the profiles available.
    pub fn value(&self, cap: Capability, profiles: &[PermissionProfile]) -> RightValue {
        if let Some(v) = self.overrides.get(&cap) {
            return *v;
        }
        if let Some(name) = &self.profile {
            if let Some(p) = profiles.iter().find(|p| &p.name == name) {
                if let Some(v) = p.values.get(&cap) {
                    return *v;
                }
            }
        }
        RightValue::default()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
pub(crate) fn sample_record(manifest: Manifest) -> InstallRecord {
    InstallRecord {
        module_id: manifest.module.id.clone(),
        repo: format!("https://github.com/{}.git", manifest.module.id),
        tag: manifest.tag(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        version: manifest.module.version.clone(),
        artifact_sha256: "ab".repeat(32),
        skills_sha256: String::new(),
        source: manifest.distribution.kind,
        accepted: manifest.capabilities.iter().copied().collect(),
        manifest,
        installed_at: 1_800_000_000,
        kind: InstallKind::Manual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest::parse(crate::manifest::tests::EXAMPLE).unwrap()
    }

    const KEY: &[u8] = b"test-key-not-a-real-secret-0123456789";

    #[test]
    fn sign_then_verify_round_trips_and_survives_serde() {
        let signed = SignedInstallRecord::sign(sample_record(manifest()), KEY, "k1").unwrap();
        assert_eq!(signed.mac.len(), 64);
        let json = serde_json::to_string(&signed).unwrap();
        let back: SignedInstallRecord = serde_json::from_str(&json).unwrap();
        let rec = back.verify(KEY).unwrap();
        assert_eq!(rec.tag, "v1.2.0");
        assert!(rec.matches_hello(&manifest()));
    }

    #[test]
    fn tampering_with_accepted_breaks_the_signature() {
        let mut signed = SignedInstallRecord::sign(sample_record(manifest()), KEY, "k1").unwrap();
        signed.record.accepted.remove(&Capability::FsRead);
        assert_eq!(signed.verify(KEY), Err(RightsError::BadSignature));
        let signed = SignedInstallRecord::sign(sample_record(manifest()), KEY, "k1").unwrap();
        assert_eq!(signed.verify(b"other-key"), Err(RightsError::BadSignature));
        let mut bad = signed.clone();
        bad.mac = "zz".repeat(32);
        assert_eq!(bad.verify(KEY), Err(RightsError::BadSignature));
    }

    #[test]
    fn accepting_an_unrequested_capability_is_refused_at_sign_time() {
        let mut rec = sample_record(manifest());
        rec.accepted.insert(Capability::ProcessSpawn);
        assert_eq!(
            SignedInstallRecord::sign(rec, KEY, "k1").unwrap_err(),
            RightsError::AcceptedNotRequested(Capability::ProcessSpawn)
        );
    }

    #[test]
    fn declined_is_the_complement_of_accepted() {
        let mut rec = sample_record(manifest());
        rec.accepted.remove(&Capability::WorkspaceRead);
        assert_eq!(rec.declined(), BTreeSet::from([Capability::WorkspaceRead]));
    }

    #[test]
    fn hello_with_a_different_manifest_does_not_match() {
        let rec = sample_record(manifest());
        let mut other = manifest();
        other.capabilities.push(Capability::ProcessSpawn);
        assert!(!rec.matches_hello(&other));
    }

    #[test]
    fn updates_that_widen_capabilities_are_held() {
        let rec = sample_record(manifest());
        let mut next = manifest();
        next.module.version = Version::new(1, 3, 0);
        assert!(
            HeldUpdate::check(&rec, &next).is_none(),
            "same caps apply silently"
        );
        next.capabilities.push(Capability::NetFetch);
        next.capabilities
            .retain(|c| *c != Capability::WorkspaceRead);
        let held = HeldUpdate::check(&rec, &next).unwrap();
        assert_eq!(held.added, BTreeSet::from([Capability::NetFetch]));
        assert_eq!(held.removed, BTreeSet::from([Capability::WorkspaceRead]));
        assert_eq!(held.to, Version::new(1, 3, 0));
    }

    #[test]
    fn rights_layer_overrides_over_profile_over_default() {
        let profiles = manifest().profiles;
        let mut r = ModuleRights {
            profile: Some("High security".into()),
            overrides: BTreeMap::new(),
        };
        assert_eq!(r.value(Capability::FsRead, &profiles), RightValue::Always);
        assert_eq!(
            r.value(Capability::UiRail, &profiles),
            RightValue::Ask,
            "not in profile"
        );
        r.overrides.insert(Capability::FsRead, RightValue::Never);
        assert_eq!(r.value(Capability::FsRead, &profiles), RightValue::Never);
        r.profile = Some("missing".into());
        assert_eq!(
            r.value(Capability::WorkspaceRead, &profiles),
            RightValue::Ask
        );
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
    }
}
