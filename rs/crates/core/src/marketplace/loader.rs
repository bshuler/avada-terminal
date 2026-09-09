//! How an install job gets the artifact it is about to record (track G10).
//!
//! There are two ways to end up with a module binary and only one of them is in this
//! repository. The free edition compiles the clone: [`SourceLoader`] runs
//! `cargo build --release --locked` and hands back what it produced. The commercial
//! edition downloads a precompiled, notarized artifact instead — that loader lives in
//! the private `avada-commercial` crate, which depends on this one and installs itself
//! with [`Marketplace::install_loader`](super::Marketplace::install_loader).
//!
//! The seam is here rather than inline in the install job for the same reason the
//! notarization verifier is a trait: the commercial half has to arrive as an
//! *addition*, not as an edit to a file both editions share. What the loader returns is
//! a path to a file; everything after it — the policy verdict, the hash, the install
//! record — is identical for a binary that was compiled here and one that was signed
//! elsewhere. That is the whole point of putting the seam at this line and not later.

use super::fetch::run_streaming;
use crate::install::dirs::binary_name;
use avada_module_sdk::manifest::{DistributionKind, Manifest, ModuleId};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Everything a loader is told about the module it must produce a binary for.
#[derive(Debug)]
pub struct LoadRequest<'a> {
    /// Which module.
    pub id: &'a ModuleId,
    /// Its validated manifest, read from the clone.
    pub manifest: &'a Manifest,
    /// The repository the tag came from.
    pub repo: &'a str,
    /// The tag being installed.
    pub tag: &'a str,
    /// The commit that tag names, already proven against `ls-remote`.
    pub commit: &'a str,
    /// The clone: a source loader builds in it, a prebuilt loader reads its manifest
    /// and downloads beside it.
    pub scratch: &'a Path,
    /// `cargo`, when the toolchain detector found one.
    pub cargo: Option<PathBuf>,
    /// The `PATH` subprocesses should run with (None keeps the process one).
    pub path: Option<OsString>,
}

/// Why a loader could not produce an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The build or the download failed; the message is for the job log.
    Failed(String),
    /// Nothing is wrong with the toolchain — this build is not allowed to do it.
    /// The install job turns this into a refusal rather than a build failure.
    Refused(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Failed(m) | LoadError::Refused(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for LoadError {}

/// Produces the artifact for one kind of distribution.
pub trait Loader: std::fmt::Debug + Send + Sync {
    /// For the job log and the install record.
    fn name(&self) -> &'static str;

    /// Whether this loader handles modules distributed that way.
    fn serves(&self, kind: DistributionKind) -> bool;

    /// Produce the binary. `log` takes subprocess output line by line; `progress` takes
    /// a whole-job percentage, which the install job passes straight through, so a
    /// loader that cannot estimate simply never calls it.
    fn load(
        &self,
        req: &LoadRequest<'_>,
        log: &dyn Fn(&str),
        progress: &dyn Fn(u8),
    ) -> Result<PathBuf, LoadError>;
}

/// Compile the clone with cargo. The free edition's only loader.
#[derive(Debug, Default, Clone, Copy)]
pub struct SourceLoader;

impl Loader for SourceLoader {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn serves(&self, kind: DistributionKind) -> bool {
        kind == DistributionKind::Source
    }

    fn load(
        &self,
        req: &LoadRequest<'_>,
        log: &dyn Fn(&str),
        progress: &dyn Fn(u8),
    ) -> Result<PathBuf, LoadError> {
        let cargo = req.cargo.clone().unwrap_or_else(|| PathBuf::from("cargo"));
        let target_dir = req.scratch.join("target");
        let mut cmd = std::process::Command::new(&cargo);
        cmd.current_dir(req.scratch)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env("CARGO_TERM_COLOR", "never")
            .args(["build", "--release", "--locked"]);
        if let Some(bin) = &req.manifest.distribution.bin {
            cmd.args(["--bin", bin]);
        }
        if let Some(p) = &req.path {
            cmd.env("PATH", p);
        }
        // Progress is counted in crates compiled because cargo will not say how many
        // there are: 50 at the start of the build, one point per crate, capped at 85 so
        // the install phase still has room above it.
        let mut compiled = 0u8;
        run_streaming(cmd, "cargo build", &mut |line| {
            if line.trim_start().starts_with("Compiling") {
                compiled = compiled.saturating_add(1);
                progress(50 + compiled.min(35));
            }
            log(line);
        })
        .map_err(|e| LoadError::Failed(e.to_string()))?;
        let binary = target_dir.join("release").join(format!(
            "{}{}",
            binary_name(req.id, req.manifest),
            std::env::consts::EXE_SUFFIX
        ));
        if !binary.is_file() {
            return Err(LoadError::Failed(format!(
                "cargo finished but produced no {}",
                binary.display()
            )));
        }
        Ok(binary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_loader_serves_source_and_nothing_else() {
        let l = SourceLoader;
        assert!(l.serves(DistributionKind::Source));
        assert!(!l.serves(DistributionKind::Binary));
        assert_eq!(l.name(), "cargo");
    }

    #[test]
    fn a_load_error_reads_as_its_message_whichever_kind_it_is() {
        assert_eq!(LoadError::Failed("boom".into()).to_string(), "boom");
        assert_eq!(LoadError::Refused("nope".into()).to_string(), "nope");
    }
}
