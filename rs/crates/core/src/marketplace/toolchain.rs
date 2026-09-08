//! What a free-tier install needs on this machine: `rustup`, `cargo` and `git`.
//!
//! The free build compiles modules from source, so the Rust toolchain is the user's
//! problem to install — the marketplace only detects it and, when it is missing, writes
//! the instructions for this OS. Detection looks at `PATH` (injectable so tests run
//! against a scratch directory), never at what a shell profile would add.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// What was found.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct Toolchain {
    /// `rustup` on `PATH`.
    pub rustup: Option<PathBuf>,
    /// `cargo` on `PATH`.
    pub cargo: Option<PathBuf>,
    /// `git` on `PATH`.
    pub git: Option<PathBuf>,
    /// Windows only: the linker comes from Visual Studio Build Tools, which `rustup`
    /// cannot install. Present when this build is for Windows.
    pub build_tools_hint: Option<String>,
}

/// The Windows linker note. Compile-checked on every platform so the text cannot rot,
/// attached to the detection only on Windows.
pub const BUILD_TOOLS_HINT: &str = "Windows builds also need the MSVC linker: install \
    \"Visual Studio Build Tools\" with the \"Desktop development with C++\" workload \
    (https://visualstudio.microsoft.com/visual-cpp-build-tools/), then restart Avada.";

impl Toolchain {
    /// Detect using the process `PATH`.
    pub fn detect() -> Self {
        Self::detect_in(std::env::var_os("PATH").as_deref())
    }

    /// Detect using the given `PATH` value (`None` means an empty path).
    pub fn detect_in(path: Option<&OsStr>) -> Self {
        let dirs: Vec<PathBuf> = path
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        Toolchain {
            rustup: find_in(&dirs, "rustup"),
            cargo: find_in(&dirs, "cargo"),
            git: find_in(&dirs, "git"),
            build_tools_hint: if cfg!(windows) {
                Some(BUILD_TOOLS_HINT.to_string())
            } else {
                None
            },
        }
    }

    /// Whether a source build can be attempted.
    pub fn ready(&self) -> bool {
        self.missing().is_empty()
    }

    /// The names of the tools that are missing.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.rustup.is_none() {
            out.push("rustup");
        }
        if self.cargo.is_none() {
            out.push("cargo");
        }
        if self.git.is_none() {
            out.push("git");
        }
        out
    }

    /// Install instructions for this OS when something is missing; `None` when ready.
    pub fn guide(&self) -> Option<String> {
        let missing = self.missing();
        if missing.is_empty() {
            return None;
        }
        Some(guide_for(
            std::env::consts::OS,
            &missing,
            self.build_tools_hint.as_deref(),
        ))
    }
}

fn find_in(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    dirs.iter()
        .map(|d| d.join(&file))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// The guide text for `os` (`std::env::consts::OS` values) given what is missing.
pub fn guide_for(os: &str, missing: &[&str], build_tools_hint: Option<&str>) -> String {
    let needs_rust = missing.iter().any(|m| *m == "rustup" || *m == "cargo");
    let needs_git = missing.contains(&"git");
    let mut lines = vec![format!(
        "Installing modules on the free tier compiles them from source, which needs: {}.",
        missing.join(", ")
    )];
    if needs_rust {
        lines.push(match os {
            "windows" => "Install Rust: download and run rustup-init.exe from \
                          https://rustup.rs, then restart Avada so the new PATH is seen."
                .to_string(),
            "macos" => "Install Rust: run  curl --proto '=https' --tlsv1.2 -sSf \
                        https://sh.rustup.rs | sh  in a terminal, then restart Avada."
                .to_string(),
            _ => "Install Rust: run  curl --proto '=https' --tlsv1.2 -sSf \
                  https://sh.rustup.rs | sh  (or your distribution's rustup package), \
                  then restart Avada."
                .to_string(),
        });
    }
    if needs_git {
        lines.push(match os {
            "windows" => "Install Git from https://git-scm.com/download/win.".to_string(),
            "macos" => {
                "Install Git: run  xcode-select --install  (or brew install git).".to_string()
            }
            _ => "Install Git with your package manager (apt install git, dnf install git, \
                  pacman -S git)."
                .to_string(),
        });
    }
    if let Some(hint) = build_tools_hint {
        lines.push(hint.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avada-mp-tc-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn fake_tool(dir: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn empty_path_finds_nothing_and_guides() {
        let tc = Toolchain::detect_in(None);
        assert!(!tc.ready());
        assert_eq!(tc.missing(), vec!["rustup", "cargo", "git"]);
        let guide = tc.guide().unwrap();
        assert!(guide.contains("rustup.rs"), "{guide}");
        assert!(guide.contains("Git"), "{guide}");
        assert_eq!(tc.build_tools_hint.is_some(), cfg!(windows));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_with_all_three_is_ready_and_has_no_guide() {
        let dir = scratch("ready");
        for t in ["rustup", "cargo", "git"] {
            fake_tool(&dir, t);
        }
        let tc = Toolchain::detect_in(Some(dir.as_os_str()));
        assert!(tc.ready(), "{tc:?}");
        assert_eq!(tc.cargo.as_deref(), Some(dir.join("cargo").as_path()));
        assert!(tc.guide().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_does_not_count() {
        let dir = scratch("noexec");
        std::fs::write(dir.join("cargo"), "not a program").unwrap();
        fake_tool(&dir, "git");
        let tc = Toolchain::detect_in(Some(dir.as_os_str()));
        assert!(tc.cargo.is_none());
        assert!(tc.git.is_some());
        assert_eq!(tc.missing(), vec!["rustup", "cargo"]);
        let guide = tc.guide().unwrap();
        assert!(!guide.contains("Install Git"), "{guide}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guide_text_is_per_os() {
        let win = guide_for("windows", &["rustup", "git"], Some(BUILD_TOOLS_HINT));
        assert!(win.contains("rustup-init.exe"));
        assert!(win.contains("git-scm.com"));
        assert!(win.contains("Build Tools"));
        let mac = guide_for("macos", &["cargo", "git"], None);
        assert!(mac.contains("sh.rustup.rs"));
        assert!(mac.contains("xcode-select"));
        assert!(!mac.contains("Build Tools"));
        let linux = guide_for("linux", &["git"], None);
        assert!(!linux.contains("rustup.rs"));
        assert!(linux.contains("apt install git"));
    }
}
