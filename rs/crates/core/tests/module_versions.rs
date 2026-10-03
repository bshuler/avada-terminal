//! A bundled module whose source changed must change its version.
//!
//! `install::seed` seeds each bundled module once per `id@version`: a machine that already
//! has that version installed adopts it as it is and never looks at the new binary. So a
//! fix to `rs/modules/<name>` that keeps the version ships in every fresh bundle and
//! reaches no existing install — which is how the Tools activate fix sat unused through a
//! rebuild and a relaunch.
//!
//! `rs/modules/versions.lock` records, per module, the version and a hash of the source
//! that version was cut from. This test fails when a module's source no longer matches the
//! hash recorded for its current version. After bumping the version in both `avada.toml`
//! and `Cargo.toml`, record the new pair with
//!
//! ```text
//! AVADA_UPDATE_MODULE_VERSIONS=1 cargo test -p avada-core --test module_versions
//! ```
//!
//! The update refuses to re-hash a version it already has: re-recording the same version
//! with new source is exactly the mistake this exists to catch.
//!
//! What counts as source: every file under the module directory except `target/`,
//! `tests/` and `*.md` — none of which reach the installed module. Line endings are
//! normalised so a Windows checkout hashes the same as a Unix one.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const UPDATE_ENV: &str = "AVADA_UPDATE_MODULE_VERSIONS";

fn modules_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules")
}

fn lock_path() -> PathBuf {
    modules_dir().join("versions.lock")
}

/// The first `key = "value"` line in `text`: the `[module]` / `[package]` table comes
/// first in both manifests, ahead of any contribution that carries its own `id`.
fn first_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.trim_start().strip_prefix('=')?;
        Some(rest.trim().trim_matches('"').to_string())
    })
}

fn source_files(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if path.is_dir() {
            if rel != "target" && rel != "tests" {
                source_files(&path, root, out);
            }
        } else if !rel.ends_with(".md") {
            out.push((rel, path));
        }
    }
}

fn source_hash(dir: &Path) -> String {
    let mut files = Vec::new();
    source_files(dir, dir, &mut files);
    files.sort();
    let mut h = Sha256::new();
    for (rel, path) in files {
        let bytes: Vec<u8> = std::fs::read(&path)
            .unwrap()
            .into_iter()
            .filter(|&b| b != b'\r')
            .collect();
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(&bytes);
        h.update([0]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `id -> (version, hash)` for every module in the tree.
fn current() -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(modules_dir()).unwrap().flatten() {
        let dir = entry.path();
        let Ok(manifest) = std::fs::read_to_string(dir.join("avada.toml")) else {
            continue;
        };
        let id = first_value(&manifest, "id").expect("module id");
        let version = first_value(&manifest, "version").expect("module version");
        let cargo = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert_eq!(
            first_value(&cargo, "version").as_deref(),
            Some(version.as_str()),
            "{id}: avada.toml and Cargo.toml disagree on the version"
        );
        out.insert(id, (version, source_hash(&dir)));
    }
    out
}

fn recorded() -> BTreeMap<String, (String, String)> {
    std::fs::read_to_string(lock_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut f = l.split_whitespace();
            let id = f.next().unwrap().to_string();
            let version = f.next().expect("lock line has a version").to_string();
            let hash = f.next().expect("lock line has a hash").to_string();
            (id, (version, hash))
        })
        .collect()
}

fn write(lock: &BTreeMap<String, (String, String)>) {
    let mut text = String::from(
        "# Bundled module versions and the source hash each was cut from.\n\
         # Checked by rs/crates/core/tests/module_versions.rs; regenerate with\n\
         # AVADA_UPDATE_MODULE_VERSIONS=1 cargo test -p avada-core --test module_versions\n",
    );
    for (id, (version, hash)) in lock {
        text.push_str(&format!("{id} {version} {hash}\n"));
    }
    std::fs::write(lock_path(), text).unwrap();
}

#[test]
fn a_changed_module_has_a_new_version() {
    let now = current();
    let then = recorded();
    let mut changed = Vec::new();
    let mut stale = Vec::new();
    for (id, (version, hash)) in &now {
        match then.get(id) {
            Some((v, h)) if v == version && h != hash => changed.push(format!(
                "  {id} {version}: source changed but the version did not — bump it in \
                 avada.toml and Cargo.toml, or existing installs keep the old build"
            )),
            Some((v, h)) if v == version && h == hash => {}
            _ => stale.push(format!("  {id} {version}: not recorded")),
        }
    }
    let gone = then.keys().any(|id| !now.contains_key(id));

    if std::env::var_os(UPDATE_ENV).is_some() {
        assert!(
            changed.is_empty(),
            "refusing to re-record a version with different source:\n{}",
            changed.join("\n")
        );
        if !stale.is_empty() || gone {
            write(&now);
        }
        return;
    }
    assert!(changed.is_empty(), "\n{}", changed.join("\n"));
    assert!(
        stale.is_empty() && !gone,
        "rs/modules/versions.lock is out of date:\n{}\nrun: {UPDATE_ENV}=1 cargo test -p \
         avada-core --test module_versions",
        stale.join("\n")
    );
}
