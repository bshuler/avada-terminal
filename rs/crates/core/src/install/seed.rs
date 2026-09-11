//! First-run seeding of precompiled first-party modules (docs/modules-fanout-plan.md,
//! requirement #2: "first-party modules installed by default").
//!
//! The app ships its first-party modules as compiled artifacts inside the bundle — a
//! binary and its `avada.toml` (plus any `[skills]` tree) per module, laid out as
//!
//! ```text
//! <seed_dir>/<owner>__<repo>/
//!   avada.toml            the manifest, exactly as the repo's
//!   bin/<name>            the compiled artifact (name per binary_name())
//!   <skills paths...>     directories named by the manifest's [skills] paths
//! ```
//!
//! On launch [`seed_bundled`] walks that directory and installs each module the machine
//! has not been offered before, straight through [`InstallStore::install`] — the same
//! seam the marketplace uses, so a seeded install is byte-for-byte a marketplace install
//! minus the clone and build. It is **offline** (no toolchain, no network) and
//! **idempotent**: a per-`id@version` ledger beside the store records what has been
//! seeded, so
//!
//! * first run seeds everything the bundle carries,
//! * a module the user later uninstalls stays gone (its `id@version` is in the ledger),
//! * an app update that bumps a module's version seeds the new version (a new key).
//!
//! Nothing here is fatal: a missing seed directory, an unreadable manifest or a failed
//! install is logged and counted, never propagated. A machine that cannot be seeded
//! behaves exactly like one with no modules installed, which is a far better failure than
//! taking the GUI down over a directory the packager got wrong.

use super::store::InstallStore;
use super::{dirs::binary_name, InstallError};
use crate::marketplace::cache::now_secs;
use avada_module_sdk::manifest::ManifestError;
use avada_module_sdk::rights::{InstallKind, InstallRecord};
use avada_module_sdk::{Manifest, ModuleId};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The manifest file inside each seed subdirectory (matches the repo's own name).
const MANIFEST_FILE: &str = "avada.toml";
/// The artifact subdirectory inside each seed subdirectory (mirrors the install layout).
const BIN_DIR: &str = "bin";
/// The seed ledger, a sibling of the modules root (never inside it: `records()` would
/// report a stray file there as a broken install on every scan).
const LEDGER_FILE: &str = "seed-ledger.json";

/// What one seeding pass did. Purely informational — the caller logs it; the GUI does not
/// depend on any of it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeedOutcome {
    /// Modules installed by this pass.
    pub seeded: Vec<ModuleId>,
    /// Subdirectories skipped because their `id@version` was already in the ledger (or
    /// already installed).
    pub skipped: usize,
    /// `(subdirectory, reason)` for every module that could not be seeded.
    pub failures: Vec<(String, String)>,
}

/// The ledger path for a store rooted at `modules_root`: a sibling of the root, like the
/// marketplace state directory.
pub fn ledger_path(modules_root: &Path) -> PathBuf {
    modules_root
        .parent()
        .unwrap_or(modules_root)
        .join(LEDGER_FILE)
}

/// The bundled seed-modules directory, resolved relative to the running executable, or
/// `None` when the build carries none. Mirrors [`crate::shell_integration::shell_integration_dir`]:
/// the packager copies the compiled first-party modules next to the binary
/// (`extraResources`-style `resources/seed-modules`, a flat `seed-modules`, macOS
/// `../Resources/seed-modules`, or an FHS `<prefix>/{share,lib}/avada/resources/seed-modules`),
/// and this returns the first that exists. Returning `None` rather than a default keeps
/// seeding additive — a build with no seed directory simply seeds nothing.
#[tracing::instrument(level = "debug", ret)]
pub fn seed_modules_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    const LEAF: &str = "seed-modules";
    let candidates = [
        exe_dir.join("resources").join(LEAF),
        exe_dir.join(LEAF),
        // macOS .app: binary in Contents/MacOS, resources in Contents/Resources.
        exe_dir
            .parent()
            .map(|p| p.join("Resources").join(LEAF))
            .unwrap_or_else(|| exe_dir.join(LEAF)),
        // FHS install (deb/rpm): <prefix>/bin/avada with resources under
        // <prefix>/{share,lib}/avada/resources — exe_dir.parent() is the prefix.
        exe_dir
            .parent()
            .map(|p| p.join("share").join("avada").join("resources").join(LEAF))
            .unwrap_or_else(|| exe_dir.join(LEAF)),
        exe_dir
            .parent()
            .map(|p| p.join("lib").join("avada").join("resources").join(LEAF))
            .unwrap_or_else(|| exe_dir.join(LEAF)),
    ];
    candidates.into_iter().find(|c| c.is_dir())
}

/// Seed every module under `seed_dir` that this machine has not been offered before.
///
/// `ledger` is the ledger path (see [`ledger_path`]). A missing `seed_dir` yields an empty
/// outcome — the bundle simply carries no seed modules. Errors on individual modules are
/// captured in [`SeedOutcome::failures`], not returned.
pub fn seed_bundled(store: &InstallStore, seed_dir: &Path, ledger: &Path) -> SeedOutcome {
    let mut outcome = SeedOutcome::default();
    let mut ledger_set = Ledger::load(ledger);

    let entries = match std::fs::read_dir(seed_dir) {
        Ok(e) => e,
        Err(e) => {
            // Not an error: a build with no seed modules has no seed directory.
            tracing::debug!(dir = %seed_dir.display(), error = %e, "no seed directory; nothing to seed");
            return outcome;
        }
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        match seed_one(store, &dir, &mut ledger_set) {
            Ok(Some(id)) => outcome.seeded.push(id),
            Ok(None) => outcome.skipped += 1,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not seed a bundled module");
                outcome.failures.push((dir.display().to_string(), e.to_string()));
            }
        }
    }

    // Persist whenever the ledger changed — a fresh seed OR an adoption of an already
    // installed version — and never fail over it: a lost ledger costs a redundant
    // (idempotent) re-seed next launch, not a broken install.
    if ledger_set.dirty {
        if let Err(e) = ledger_set.save(ledger) {
            tracing::warn!(error = %e, "could not persist the seed ledger; modules may be re-seeded next launch");
        }
    }

    if !outcome.seeded.is_empty() || !outcome.failures.is_empty() {
        tracing::info!(
            seeded = outcome.seeded.len(),
            skipped = outcome.skipped,
            failed = outcome.failures.len(),
            "seeded bundled modules"
        );
    }
    outcome
}

/// Seed one subdirectory. `Ok(Some(id))` installed it, `Ok(None)` skipped it (already
/// offered), `Err` failed.
fn seed_one(
    store: &InstallStore,
    dir: &Path,
    ledger: &mut Ledger,
) -> Result<Option<ModuleId>, SeedError> {
    let manifest_text = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|e| SeedError::Manifest(format!("{MANIFEST_FILE}: {e}")))?;
    let manifest = Manifest::parse(&manifest_text)?;
    let id = manifest.id().clone();
    let version = manifest.module.version.clone();
    let key = ledger_key(&id, &manifest);

    // Offered before — even if the user has since uninstalled it. This is what makes an
    // uninstall stick across launches.
    if ledger.contains(&key) {
        return Ok(None);
    }
    // Defensive against a lost ledger: if this exact version is already installed, adopt
    // it into the ledger rather than reinstalling over a live module.
    if store.installed_versions(&id)?.contains(&version) {
        ledger.insert(key);
        return Ok(None);
    }

    let binary = dir.join(BIN_DIR).join(binary_name(&id, &manifest));
    if !binary.is_file() {
        return Err(SeedError::MissingBinary(binary));
    }

    // First-party modules are trusted by the edition that ships them, so we accept every
    // capability the manifest requests except the escape hatches — mirroring the
    // marketplace's default when the caller named no narrower set.
    let accepted = manifest
        .capabilities
        .iter()
        .copied()
        .filter(|c| !c.is_escape_hatch())
        .collect();

    let record = InstallRecord {
        module_id: id.clone(),
        // Cosmetic for a bundled artifact — the canonical GitHub URL the module would be
        // cloned from if it were installed the long way.
        repo: format!("https://github.com/{}/{}", id.owner(), id.repo()),
        tag: manifest.tag(),
        // No git provenance: the artifact came from the bundle, not a clone.
        commit: String::new(),
        version: version.clone(),
        // Left empty for `install` to fill from the bytes it actually stages.
        artifact_sha256: String::new(),
        skills_sha256: String::new(),
        source: manifest.distribution.kind,
        accepted,
        manifest,
        installed_at: now_secs(),
        kind: InstallKind::Manual,
    };

    // `Some(dir)`: a module that ships skills needs its `[skills] paths` staged from the
    // seed subdirectory. `install` no-ops the source when there are no skills, and its
    // lockfile logic already refuses to demote a newer active version, so there is no
    // explicit `activate` here.
    store.install(record, &binary, Some(dir))?;
    ledger.insert(key);
    Ok(Some(id))
}

/// The ledger key for a module version: `owner/repo@version`.
fn ledger_key(id: &ModuleId, manifest: &Manifest) -> String {
    format!("{}@{}", id.as_str(), manifest.module.version)
}

/// The set of `id@version` strings already offered on this machine, persisted as JSON.
#[derive(Debug, Default)]
struct Ledger {
    seen: BTreeSet<String>,
    /// Whether anything was inserted since load — the signal to persist.
    dirty: bool,
}

impl Ledger {
    /// Load the ledger, treating any read or parse failure as an empty ledger: a
    /// corrupt ledger must not wedge seeding, only cost an idempotent re-seed.
    fn load(path: &Path) -> Ledger {
        let seen = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<BTreeSet<String>>(&t).ok())
            .unwrap_or_default();
        Ledger { seen, dirty: false }
    }

    fn contains(&self, key: &str) -> bool {
        self.seen.contains(key)
    }

    fn insert(&mut self, key: String) {
        self.dirty |= self.seen.insert(key);
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text =
            serde_json::to_string_pretty(&self.seen).unwrap_or_else(|_| String::from("[]"));
        std::fs::write(path, text)
    }
}

/// Why seeding one module failed.
#[derive(Debug)]
enum SeedError {
    /// The manifest could not be read or parsed.
    Manifest(String),
    /// The compiled artifact was not where the layout expects it.
    MissingBinary(PathBuf),
    /// The install store refused the record.
    Install(InstallError),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::Manifest(m) => write!(f, "manifest: {m}"),
            SeedError::MissingBinary(p) => write!(f, "no artifact at {}", p.display()),
            SeedError::Install(e) => write!(f, "install: {e}"),
        }
    }
}

impl std::error::Error for SeedError {}

impl From<ManifestError> for SeedError {
    fn from(e: ManifestError) -> Self {
        SeedError::Manifest(e.to_string())
    }
}

impl From<InstallError> for SeedError {
    fn from(e: InstallError) -> Self {
        SeedError::Install(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::keyring::{KeyStore, MemoryKeyStore};
    use crate::install::record::fixtures::{manifest, manifest_at};
    use crate::install::InstallPaths;
    use avada_module_sdk::Manifest;
    use semver::Version;
    use std::sync::Arc;

    /// A scratch store plus a bundle-style seed directory beside it, all under one temp
    /// base that is asserted to contain every path the store writes.
    struct Bench {
        base: PathBuf,
        seed: PathBuf,
        store: InstallStore,
    }

    impl Bench {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "avada-seed-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            let root = base.join("modules");
            let seed = base.join("seed-modules");
            std::fs::create_dir_all(&seed).unwrap();
            let keys = Arc::new(MemoryKeyStore::new());
            let store =
                InstallStore::open(InstallPaths::under(&root), keys as Arc<dyn KeyStore>).unwrap();
            Bench { base, seed, store }
        }

        fn ledger(&self) -> PathBuf {
            ledger_path(&self.base.join("modules"))
        }

        /// Stage one module in the seed directory: its manifest, a fake binary at the
        /// layout's `bin/<name>`, and — because the reference manifest declares
        /// `[skills] paths = ["skills"]` — a `skills/greet/SKILL.md` so staging has
        /// something to copy.
        fn stage(&self, manifest: &Manifest, body: &[u8]) -> PathBuf {
            let dir = self.seed.join(manifest.id().dir_name());
            std::fs::create_dir_all(dir.join(BIN_DIR)).unwrap();
            std::fs::write(dir.join(MANIFEST_FILE), manifest.to_toml()).unwrap();
            let bin = dir.join(BIN_DIR).join(binary_name(manifest.id(), manifest));
            std::fs::write(&bin, body).unwrap();
            for p in &manifest.skills.paths {
                let unit = dir.join(p).join("greet");
                std::fs::create_dir_all(&unit).unwrap();
                std::fs::write(unit.join("SKILL.md"), "# greet\n").unwrap();
            }
            dir
        }

        fn seed(&self) -> SeedOutcome {
            seed_bundled(&self.store, &self.seed, &self.ledger())
        }

        fn installed(&self, m: &Manifest) -> Vec<Version> {
            self.store.installed_versions(m.id()).unwrap()
        }
    }

    #[test]
    fn seeds_a_bundled_module_then_skips_it_next_launch() {
        let b = Bench::new("once");
        let m = manifest();
        b.stage(&m, b"binary-bytes");

        let first = b.seed();
        assert_eq!(first.seeded, vec![m.id().clone()]);
        assert_eq!(first.skipped, 0);
        assert!(first.failures.is_empty(), "{:?}", first.failures);
        assert_eq!(b.installed(&m), vec![m.module.version.clone()]);

        // Second launch: the ledger already carries id@version, so nothing is reinstalled.
        let second = b.seed();
        assert!(second.seeded.is_empty());
        assert_eq!(second.skipped, 1);
    }

    #[test]
    fn the_staged_skills_and_the_binary_both_land() {
        let b = Bench::new("skills");
        let m = manifest();
        b.stage(&m, b"binary-bytes");
        b.seed();

        let installed = b.store.record(m.id()).unwrap().expect("installed");
        assert_eq!(std::fs::read(&installed.binary).unwrap(), b"binary-bytes");
        let unit = installed.version_dir.join("skills").join("greet");
        assert_eq!(
            std::fs::read_to_string(unit.join("SKILL.md")).unwrap(),
            "# greet\n"
        );
    }

    #[test]
    fn an_uninstalled_module_is_not_resurrected() {
        let b = Bench::new("uninstall");
        let m = manifest();
        b.stage(&m, b"x");
        b.seed();

        // The user removes it through the store; the ledger still remembers it.
        b.store.uninstall(m.id(), &m.module.version).unwrap();
        assert!(b.installed(&m).is_empty());

        let again = b.seed();
        assert!(again.seeded.is_empty(), "an uninstall must stick");
        assert_eq!(again.skipped, 1);
        assert!(b.installed(&m).is_empty());
    }

    #[test]
    fn an_app_update_seeds_the_bumped_version() {
        let b = Bench::new("bump");
        let v1 = manifest_at(Version::new(1, 2, 0));
        b.stage(&v1, b"v1");
        b.seed();

        // The next app release ships v1.3.0 in place of v1.2.0.
        std::fs::remove_dir_all(b.seed.join(v1.id().dir_name())).unwrap();
        let v2 = manifest_at(Version::new(1, 3, 0));
        b.stage(&v2, b"v2");

        let out = b.seed();
        assert_eq!(out.seeded, vec![v2.id().clone()]);
        let mut versions = b.installed(&v2);
        versions.sort();
        assert_eq!(versions, vec![Version::new(1, 2, 0), Version::new(1, 3, 0)]);
    }

    #[test]
    fn a_missing_seed_directory_is_a_no_op() {
        let b = Bench::new("empty");
        std::fs::remove_dir_all(&b.seed).unwrap();
        let out = b.seed();
        assert_eq!(out, SeedOutcome::default());
    }

    #[test]
    fn a_module_already_installed_without_a_ledger_is_adopted_not_reinstalled() {
        let b = Bench::new("adopt");
        let m = manifest();
        b.stage(&m, b"x");
        b.seed();

        // Simulate a lost ledger while the store keeps the install.
        std::fs::remove_file(b.ledger()).unwrap();
        let out = b.seed();
        assert!(out.seeded.is_empty(), "must not reinstall a live module");
        assert_eq!(out.skipped, 1);
        // And the ledger is rebuilt, so the next launch is a clean skip too.
        assert!(b.ledger().exists());
    }

    #[test]
    fn a_broken_module_is_reported_and_the_rest_still_seed() {
        let b = Bench::new("mixed");
        let good = manifest();
        b.stage(&good, b"x");

        // A subdirectory whose manifest will not parse.
        let bad = b.seed.join("acme__broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(MANIFEST_FILE), "this is not toml = = =").unwrap();

        // A subdirectory whose manifest is fine but whose artifact is missing.
        let noboin = manifest_at(Version::new(2, 0, 0));
        let dir = b.seed.join("acme__noboin");
        // A distinct id so it does not collide with `good`.
        let noboin = rename(noboin, "acme/avada-noboin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), noboin.to_toml()).unwrap();

        let out = b.seed();
        assert_eq!(out.seeded, vec![good.id().clone()]);
        assert_eq!(out.failures.len(), 2, "{:?}", out.failures);
    }

    /// The reference manifest under a different id — the seed dir is keyed by id, and a
    /// test that needs two modules needs two ids.
    fn rename(mut m: Manifest, id: &str) -> Manifest {
        m.module.id = avada_module_sdk::ModuleId::new(id).unwrap();
        // Dependencies name the old id's sibling; drop them so the manifest still parses
        // as a standalone module.
        m.dependencies.clear();
        m.requires.clear();
        m
    }
}
