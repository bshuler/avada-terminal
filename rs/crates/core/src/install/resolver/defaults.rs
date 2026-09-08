//! Provider defaults: the module the user wants answering a shape when several
//! installed modules provide it.
//!
//! Persisted at `<modules root>/defaults.json`, owner-only, next to `modules.lock`.
//! The rules (docs/modules-fanout-plan.md §2): a hand install makes the module the
//! default for every shape it provides that has no default yet (or that it already
//! holds); dependency installs and upgrades never displace a default; the user can
//! set one explicitly; uninstalling the default clears it.

use crate::install::dirs::InstallPaths;
use crate::install::{io_at, InstallError};
use crate::persistence::paths::write_atomic_private;
use avada_module_sdk::manifest::{Manifest, ModuleId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name under the modules root.
pub const DEFAULTS_FILE: &str = "defaults.json";
/// Schema version written.
pub const DEFAULTS_VERSION: u32 = 1;

/// Shape → the module the user prefers as its provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    /// [`DEFAULTS_VERSION`].
    pub version: u32,
    /// Shape → module.
    #[serde(default)]
    pub shapes: BTreeMap<String, ModuleId>,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            version: DEFAULTS_VERSION,
            shapes: BTreeMap::new(),
        }
    }
}

impl Defaults {
    /// `<root>/defaults.json`.
    pub fn path(paths: &InstallPaths) -> PathBuf {
        paths.root().join(DEFAULTS_FILE)
    }

    /// Read the file, or the empty set when there is none. A file this host cannot
    /// read (newer schema, not JSON) is an error rather than silently empty, so a
    /// later save cannot erase what a newer host wrote.
    pub fn load(paths: &InstallPaths) -> Result<Defaults, InstallError> {
        Self::load_from(&Self::path(paths))
    }

    fn load_from(path: &Path) -> Result<Defaults, InstallError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Defaults::default()),
            Err(e) => return Err(io_at(path, e)),
        };
        let parsed: Defaults = serde_json::from_str(&text).map_err(|e| InstallError::Record {
            path: path.to_path_buf(),
            reason: format!("defaults.json does not parse: {e}"),
        })?;
        if parsed.version > DEFAULTS_VERSION {
            return Err(InstallError::Record {
                path: path.to_path_buf(),
                reason: format!(
                    "defaults.json version {} is newer than this host understands",
                    parsed.version
                ),
            });
        }
        Ok(parsed)
    }

    /// Write atomically, owner-only.
    pub fn save(&self, paths: &InstallPaths) -> Result<(), InstallError> {
        let path = Self::path(paths);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
        }
        let mut text = serde_json::to_string_pretty(self).expect("defaults serialize");
        text.push('\n');
        write_atomic_private(&path, text.as_bytes()).map_err(|e| io_at(&path, e))
    }

    /// The default provider of `shape`, if the user has one.
    pub fn get(&self, shape: &str) -> Option<&ModuleId> {
        self.shapes.get(shape)
    }

    /// Whether `id` is the default for `shape`.
    pub fn is_default(&self, shape: &str, id: &ModuleId) -> bool {
        self.shapes.get(shape) == Some(id)
    }

    /// The user's explicit choice: always wins.
    pub fn set_default(&mut self, shape: &str, id: &ModuleId) {
        self.shapes.insert(shape.to_string(), id.clone());
    }

    /// A hand install of `manifest`'s module: it becomes the default for each shape
    /// it provides that has no default yet, and stays the default where it already
    /// is. Returns the shapes newly defaulted. Dependency installs must not call this.
    pub fn note_manual_install(&mut self, manifest: &Manifest) -> Vec<String> {
        let id = manifest.id();
        let mut newly = Vec::new();
        for p in &manifest.provides {
            if !self.shapes.contains_key(&p.shape) {
                self.shapes.insert(p.shape.clone(), id.clone());
                newly.push(p.shape.clone());
            }
        }
        newly
    }

    /// The module is gone: forget every default naming it. Returns the shapes cleared.
    pub fn clear_module(&mut self, id: &ModuleId) -> Vec<String> {
        let cleared: Vec<String> = self
            .shapes
            .iter()
            .filter(|(_, m)| *m == id)
            .map(|(s, _)| s.clone())
            .collect();
        for s in &cleared {
            self.shapes.remove(s);
        }
        cleared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_module_sdk::manifest::Manifest;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).unwrap()
    }

    fn manifest(id: &str, provides: &[&str]) -> Manifest {
        let mut text = format!(
            "[module]\nid = \"{id}\"\nname = \"x\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
             publisher = \"p\"\ncontract = \"^1\"\n\n[distribution]\nkind = \"source\"\n"
        );
        for shape in provides {
            text.push_str(&format!(
                "\n[[provides]]\nshape = \"{shape}\"\nversion = \"1.0.0\"\n"
            ));
        }
        Manifest::parse(&text).unwrap()
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avada-defaults-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn manual_install_takes_free_shapes_only_and_uninstall_clears() {
        let mut d = Defaults::default();
        let a = manifest("acme/a", &["s.one", "s.two"]);
        let b = manifest("acme/b", &["s.two", "s.three"]);
        assert_eq!(d.note_manual_install(&a), vec!["s.one", "s.two"]);
        // b by hand: s.two already has a default, only s.three is free.
        assert_eq!(d.note_manual_install(&b), vec!["s.three"]);
        assert_eq!(d.get("s.two"), Some(&id("acme/a")));
        // a again (an upgrade by hand): nothing changes.
        assert!(d.note_manual_install(&a).is_empty());
        assert!(d.is_default("s.one", &id("acme/a")));
        // The user chooses b for s.two: explicit wins.
        d.set_default("s.two", &id("acme/b"));
        assert_eq!(d.get("s.two"), Some(&id("acme/b")));
        // Uninstalling b forgets every default naming it.
        let mut cleared = d.clear_module(&id("acme/b"));
        cleared.sort();
        assert_eq!(cleared, vec!["s.three", "s.two"]);
        assert_eq!(d.get("s.two"), None);
        assert_eq!(d.get("s.one"), Some(&id("acme/a")));
        assert!(d.clear_module(&id("acme/none")).is_empty());
    }

    #[test]
    fn persists_owner_only_under_the_modules_root() {
        let root = scratch();
        let paths = InstallPaths::under(&root);
        assert_eq!(Defaults::load(&paths).unwrap(), Defaults::default());
        let mut d = Defaults::default();
        d.set_default("avada.files.tree", &id("acme/avada-files"));
        d.save(&paths).unwrap();
        let path = Defaults::path(&paths);
        assert_eq!(path, root.join("defaults.json"));
        assert_eq!(Defaults::load(&paths).unwrap(), d);
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["shapes"]["avada.files.tree"], "acme/avada-files");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "owner-only");
        }
        // A newer schema is refused, not emptied.
        std::fs::write(&path, r#"{"version": 99, "shapes": {}}"#).unwrap();
        assert!(matches!(
            Defaults::load(&paths),
            Err(InstallError::Record { .. })
        ));
        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(
            Defaults::load(&paths),
            Err(InstallError::Record { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }
}
