//! The marketplace: find modules on GitHub, build them from source, install and enable
//! them — the free-tier pipeline behind the `marketplace.*` control routes.
//!
//! [`Marketplace`] is the facade the routes call. Around it:
//!
//! - [`github`]: discovery (search by the `avada-module` topic, tags, manifests) behind
//!   a trait, with an on-disk [`cache`] and optional device-flow sign-in whose token
//!   lives in a [`token::TokenStore`].
//! - [`toolchain`]: `rustup`/`cargo`/`git` detection with a per-OS install guide.
//! - [`fetch`]: `git ls-remote` + shallow `git clone` as subprocesses, manifest reading
//!   and the free-build gate (source only, never commercial).
//! - [`job`]: the background install job the routes poll.
//! - [`workspace`]: per-workspace enable/disable state.
//!
//! Install state ends in the existing [`InstallStore`]: one directory per id and
//! version, a signed `InstallRecord`, the lockfile pinning tag + commit + SHA-256. This
//! module never spawns a module process; the host (track H4) consumes the state.

pub mod cache;
pub mod fetch;
pub mod github;
pub mod job;
pub mod token;
pub mod toolchain;
pub mod workspace;

#[cfg(test)]
pub(crate) mod testing;

use crate::install::dirs::binary_name;
use crate::install::{
    hash_file, FileKeyStore, InstallError, InstallPaths, InstallStore, KeyStore, RecordStatus,
};
use crate::persistence::lockfile::LockfileIoError;
use avada_module_sdk::caps::Capability;
use avada_module_sdk::manifest::{Manifest, ModuleId, Requirement};
use avada_module_sdk::rights::{InstallKind, InstallRecord};
use cache::Cache;
use fetch::{check_free_build, newest_tag, read_manifest, run_streaming, Git};
use github::{DevicePoll, GitHubApi, GitHubConfig, GitHubError, HttpGitHub, RepoSummary, TagInfo};
use job::{Job, JobBook, Phase};
use semver::Version;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use token::{FileTokenStore, Token, TokenStore};
use toolchain::Toolchain;
use workspace::{valid_key, WorkspaceStates};

/// How deep the inline dependency install follows `requires` (the real resolver is G6).
const DEPENDENCY_DEPTH: usize = 4;
/// The marketplace's own state directory, a sibling of the modules root
/// (`<data_dir>/marketplace`). It is not under the modules root because the install
/// store lists every directory there as a module.
pub const STATE_DIR: &str = "marketplace";
/// Subdirectory of the state dir for clones in progress.
pub const SCRATCH_DIR: &str = "scratch";
/// Subdirectory of the state dir for the GitHub answer cache.
pub const CACHE_DIR: &str = "cache";

/// `<modules root>/../marketplace`: where the marketplace keeps cache, scratch and
/// per-workspace state for the store rooted at `modules_root`.
pub fn state_dir_beside(modules_root: &Path) -> PathBuf {
    modules_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| modules_root.to_path_buf())
        .join(STATE_DIR)
}

/// Why a marketplace call failed. Every variant maps to an HTTP status.
#[derive(Debug)]
pub enum MarketplaceError {
    /// The module id is not `owner/repo`.
    BadModuleId(String),
    /// The workspace key is unusable.
    BadWorkspace(String),
    /// The request is understood and refused (binary/commercial module, tag/commit
    /// mismatch, no provider for a requirement).
    Refused(String),
    /// The module (at that version) is not installed.
    NotInstalled(String),
    /// No such job.
    NoSuchJob(String),
    /// No such sign-in.
    NoSuchSignIn(String),
    /// Sign-in cannot start (no client id).
    SignInUnavailable(String),
    /// The build toolchain is missing; carries the install guide.
    Toolchain(String),
    /// GitHub did not answer usefully.
    GitHub(GitHubError),
    /// The install store failed.
    Install(InstallError),
    /// A workspace state file failed.
    Io(String),
    /// git failed.
    Git(String),
    /// cargo failed.
    Build(String),
}

impl MarketplaceError {
    /// The HTTP status a route answers with.
    pub fn http_status(&self) -> u16 {
        match self {
            MarketplaceError::BadModuleId(_) | MarketplaceError::BadWorkspace(_) => 400,
            MarketplaceError::Refused(_) => 409,
            MarketplaceError::NotInstalled(_)
            | MarketplaceError::NoSuchJob(_)
            | MarketplaceError::NoSuchSignIn(_) => 404,
            MarketplaceError::SignInUnavailable(_) => 503,
            MarketplaceError::Toolchain(_) => 412,
            MarketplaceError::GitHub(_) => 502,
            MarketplaceError::Install(_)
            | MarketplaceError::Io(_)
            | MarketplaceError::Git(_)
            | MarketplaceError::Build(_) => 500,
        }
    }
}

impl fmt::Display for MarketplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarketplaceError::BadModuleId(m) => write!(f, "`{m}` is not an owner/repo module id"),
            MarketplaceError::BadWorkspace(w) => write!(f, "`{w}` is not a usable workspace key"),
            MarketplaceError::Refused(r) => write!(f, "refused: {r}"),
            MarketplaceError::NotInstalled(m) => write!(f, "{m} is not installed"),
            MarketplaceError::NoSuchJob(j) => write!(f, "no job `{j}`"),
            MarketplaceError::NoSuchSignIn(s) => write!(f, "no sign-in `{s}`"),
            MarketplaceError::SignInUnavailable(e) => write!(f, "sign-in unavailable: {e}"),
            MarketplaceError::Toolchain(g) => write!(f, "{g}"),
            MarketplaceError::GitHub(e) => write!(f, "{e}"),
            MarketplaceError::Install(e) => write!(f, "install: {e}"),
            MarketplaceError::Io(e) => write!(f, "{e}"),
            MarketplaceError::Git(e) => write!(f, "{e}"),
            MarketplaceError::Build(e) => write!(f, "build: {e}"),
        }
    }
}
impl std::error::Error for MarketplaceError {}

impl From<GitHubError> for MarketplaceError {
    fn from(e: GitHubError) -> Self {
        MarketplaceError::GitHub(e)
    }
}
impl From<InstallError> for MarketplaceError {
    fn from(e: InstallError) -> Self {
        MarketplaceError::Install(e)
    }
}
impl From<LockfileIoError> for MarketplaceError {
    fn from(e: LockfileIoError) -> Self {
        MarketplaceError::Io(e.to_string())
    }
}
impl From<token::TokenError> for MarketplaceError {
    fn from(e: token::TokenError) -> Self {
        MarketplaceError::Io(e.to_string())
    }
}

/// Knobs the host and the tests set.
#[derive(Debug, Clone)]
pub struct MarketplaceOptions {
    /// Where `owner/repo.git` clones from; `https://github.com` in production, a
    /// `file://` root in tests.
    pub git_base: String,
    /// `PATH` for tool detection and subprocesses; `None` inherits the process one.
    pub path: Option<OsString>,
}

impl Default for MarketplaceOptions {
    fn default() -> Self {
        MarketplaceOptions {
            git_base: "https://github.com".into(),
            path: None,
        }
    }
}

/// What a caller asks to install.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    /// `owner/repo`.
    pub module: String,
    /// The tag; `None` picks the newest `vX.Y.Z` tag.
    pub tag: Option<String>,
    /// Capabilities granted; `None` grants the manifest's request minus escape hatches.
    pub accepted: Option<BTreeSet<Capability>>,
    /// Enable in this workspace once installed.
    pub workspace: Option<String>,
    /// The commit the tag must name (the UI passes what `show` displayed); `None`
    /// trusts `ls-remote` and still checks the clone against it.
    pub expected_commit: Option<String>,
    /// Manual or dependency.
    pub kind: InstallKind,
}

impl InstallRequest {
    /// A manual install of `module`'s newest tag with default rights.
    pub fn new(module: &str) -> Self {
        InstallRequest {
            module: module.to_string(),
            tag: None,
            accepted: None,
            workspace: None,
            expected_commit: None,
            kind: InstallKind::Manual,
        }
    }
}

/// What `show` answers.
#[derive(Debug, Clone, Serialize)]
pub struct ModuleView {
    /// `owner/repo`.
    pub module: String,
    /// The repository, when GitHub knows it.
    pub repo: Option<RepoSummary>,
    /// Its manifest at the newest tag (or default branch when untagged).
    pub manifest: Option<Manifest>,
    /// Why the manifest could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_error: Option<String>,
    /// Tags GitHub lists.
    pub tags: Vec<TagInfo>,
    /// The newest installable tag.
    pub newest_tag: Option<String>,
    /// Versions installed on this machine.
    pub installed: Vec<String>,
    /// The active version.
    pub active: Option<String>,
    /// Workspace key → enabled there.
    pub enabled: BTreeMap<String, bool>,
}

/// One installed version as `installed` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledView {
    /// `owner/repo` (None when the directory name is unreadable).
    pub module: Option<String>,
    /// The version (None when unreadable).
    pub version: Option<String>,
    /// Whether the lockfile pins this version.
    pub active: bool,
    /// Manual or dependency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<InstallKind>,
    /// The tag installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The commit installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The binary's SHA-256.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Capabilities granted.
    pub accepted: Vec<Capability>,
    /// Workspace key → enabled there.
    pub enabled: BTreeMap<String, bool>,
    /// Why the record cannot be trusted, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

/// Where a sign-in is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignInStatus {
    /// Waiting for the user to type the code.
    Pending,
    /// The token is stored.
    Done,
    /// The code expired.
    Expired,
    /// The user refused.
    Denied,
}

/// What the sign-in routes answer. Never carries the device code or the token.
#[derive(Debug, Clone, Serialize)]
pub struct SignInView {
    /// Sign-in id to poll.
    pub id: String,
    /// The code the user types.
    pub user_code: String,
    /// Where they type it.
    pub verification_uri: String,
    /// Unix seconds when the code expires.
    pub expires_at: u64,
    /// Minimum seconds between polls.
    pub interval: u64,
    /// Where it is.
    pub status: SignInStatus,
}

struct SignIn {
    device_code: Token,
    view: SignInView,
}

/// The marketplace facade.
pub struct Marketplace {
    store: InstallStore,
    github: Arc<dyn GitHubApi>,
    tokens: Arc<dyn TokenStore>,
    options: MarketplaceOptions,
    jobs: JobBook,
    signins: Mutex<BTreeMap<String, SignIn>>,
    workspaces: WorkspaceStates,
    state_dir: PathBuf,
}

impl Marketplace {
    /// A marketplace over `store`, asking `github` and keeping the token in `tokens`.
    pub fn new(
        store: InstallStore,
        github: Arc<dyn GitHubApi>,
        tokens: Arc<dyn TokenStore>,
        options: MarketplaceOptions,
    ) -> Self {
        let state_dir = state_dir_beside(store.paths().root());
        let workspaces = WorkspaceStates::under(&state_dir);
        Marketplace {
            store,
            github,
            tokens,
            options,
            jobs: JobBook::new(),
            signins: Mutex::new(BTreeMap::new()),
            workspaces,
            state_dir,
        }
    }

    /// The production wiring over the modules root `modules_root`: file key store, file
    /// token store (both in the store's key directory), the real GitHub client caching
    /// under `<modules_root>/../marketplace/cache`.
    pub fn open_under(modules_root: &Path, github: GitHubConfig) -> Result<Self, InstallError> {
        let paths = InstallPaths::under(modules_root);
        let keys: Arc<dyn KeyStore> = Arc::new(FileKeyStore::new(paths.keys_dir()));
        let tokens: Arc<dyn TokenStore> = Arc::new(FileTokenStore::new(&paths.keys_dir()));
        let store = InstallStore::open(paths, keys)?;
        let cache = Cache::new(state_dir_beside(modules_root).join(CACHE_DIR));
        let api: Arc<dyn GitHubApi> = Arc::new(HttpGitHub::new(github, cache));
        Ok(Marketplace::new(
            store,
            api,
            tokens,
            MarketplaceOptions::default(),
        ))
    }

    /// The production wiring under the host's modules root.
    pub fn host() -> Result<Self, InstallError> {
        let root = InstallPaths::host().root().to_path_buf();
        Self::open_under(&root, GitHubConfig::default())
    }

    /// The install store.
    pub fn store(&self) -> &InstallStore {
        &self.store
    }

    /// The options in force.
    pub fn options(&self) -> &MarketplaceOptions {
        &self.options
    }

    /// Where the GitHub answer cache lives under this root.
    pub fn cache_dir(&self) -> PathBuf {
        self.state_dir.join(CACHE_DIR)
    }

    /// `<modules root>/../marketplace`: cache, scratch and workspace state.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn auth(&self) -> Result<Option<Token>, MarketplaceError> {
        Ok(self.tokens.get()?)
    }

    fn path(&self) -> Option<&OsStr> {
        self.options.path.as_deref()
    }

    fn parse_id(module: &str) -> Result<ModuleId, MarketplaceError> {
        ModuleId::new(module).map_err(|_| MarketplaceError::BadModuleId(module.to_string()))
    }

    fn check_workspace(key: &str) -> Result<(), MarketplaceError> {
        if valid_key(key) {
            Ok(())
        } else {
            Err(MarketplaceError::BadWorkspace(key.to_string()))
        }
    }

    fn repo_url(&self, id: &ModuleId) -> String {
        format!(
            "{}/{}.git",
            self.options.git_base.trim_end_matches('/'),
            id.as_str()
        )
    }

    // ---- discovery

    /// Repositories tagged `avada-module` matching `query`.
    pub async fn search(&self, query: &str) -> Result<Vec<RepoSummary>, MarketplaceError> {
        let auth = self.auth()?;
        Ok(self.github.search(query, auth.as_ref()).await?)
    }

    /// Everything the UI shows for one module.
    pub async fn show(&self, owner: &str, repo: &str) -> Result<ModuleView, MarketplaceError> {
        let module = format!("{owner}/{repo}");
        let id = Self::parse_id(&module)?;
        let auth = self.auth()?;
        let summary = self.github.repo(owner, repo, auth.as_ref()).await?;
        let tags = self.github.tags(owner, repo, auth.as_ref()).await?;
        let newest = newest_tag(tags.iter().map(|t| t.name.as_str())).map(|(t, _)| t);
        let (manifest, manifest_error) = if summary.is_some() {
            let reference = newest.clone().unwrap_or_else(|| "HEAD".to_string());
            match self
                .github
                .file(owner, repo, fetch::MANIFEST_FILE, &reference, auth.as_ref())
                .await?
            {
                None => (
                    None,
                    Some(format!("no {} at {reference}", fetch::MANIFEST_FILE)),
                ),
                Some(text) => match Manifest::parse(&text).and_then(|m| m.validate().map(|()| m)) {
                    Ok(m) => (Some(m), None),
                    Err(e) => (None, Some(e.to_string())),
                },
            }
        } else {
            (None, None)
        };
        let installed = self
            .store
            .installed_versions(&id)?
            .iter()
            .map(|v| v.to_string())
            .collect();
        let active = self.store.record(&id)?.map(|i| i.version.to_string());
        Ok(ModuleView {
            module,
            repo: summary,
            manifest,
            manifest_error,
            tags,
            newest_tag: newest,
            installed,
            active,
            enabled: self.workspaces.enabled_in(&id),
        })
    }

    // ---- install pipeline

    /// The toolchain as seen on the configured `PATH`.
    pub fn toolchain(&self) -> Toolchain {
        Toolchain::detect_in(self.path())
    }

    /// Start an install job. Returns the job in [`Phase::Fetch`]; poll [`Self::job`].
    pub fn install(self: &Arc<Self>, req: InstallRequest) -> Result<Job, MarketplaceError> {
        Self::parse_id(&req.module)?;
        if let Some(ws) = &req.workspace {
            Self::check_workspace(ws)?;
        }
        let toolchain = self.toolchain();
        if let Some(guide) = toolchain.guide() {
            return Err(MarketplaceError::Toolchain(guide));
        }
        let job = self.jobs.start(&req.module, req.tag.as_deref(), req.kind);
        let me = Arc::clone(self);
        let job_id = job.id.clone();
        std::thread::Builder::new()
            .name(format!("marketplace-install-{}", &job_id[..8]))
            .spawn(move || {
                match me.run_install(&job_id, &req, &toolchain, 0) {
                    Ok(version) => me.jobs.done(&job_id, &version.to_string()),
                    Err(e) => {
                        tracing::warn!(job = %job_id, module = %req.module, error = %e, "marketplace install failed");
                        me.jobs.fail(&job_id, &e.to_string());
                    }
                }
            })
            .map_err(|e| MarketplaceError::Io(format!("could not start install thread: {e}")))?;
        Ok(job)
    }

    /// The whole pipeline for one module, on the calling thread. Dependencies recurse
    /// with the same job (their log lines interleave, prefixed by module).
    fn run_install(
        &self,
        job_id: &str,
        req: &InstallRequest,
        toolchain: &Toolchain,
        depth: usize,
    ) -> Result<Version, MarketplaceError> {
        let id = Self::parse_id(&req.module)?;
        let jobs = &self.jobs;
        let log = |line: &str| jobs.log(job_id, &format!("[{}] {line}", id.as_str()));
        let git = Git::new(
            toolchain
                .git
                .clone()
                .unwrap_or_else(|| PathBuf::from("git")),
            self.path(),
        );
        let url = self.repo_url(&id);

        // Fetch: which commit does the tag name?
        jobs.phase(job_id, Phase::Fetch, Some(5));
        log(&format!("resolving tags of {url}"));
        let tags = git
            .ls_remote_tags(&url)
            .map_err(|e| MarketplaceError::Git(e.to_string()))?;
        let tag = match &req.tag {
            Some(t) => t.clone(),
            None => newest_tag(tags.keys().map(String::as_str))
                .map(|(t, _)| t)
                .ok_or_else(|| {
                    MarketplaceError::Refused(format!(
                        "{} has no vX.Y.Z tag to install",
                        id.as_str()
                    ))
                })?,
        };
        if depth == 0 {
            jobs.tag(job_id, &tag);
        }
        let remote_commit = tags.get(&tag).cloned().ok_or_else(|| {
            MarketplaceError::Refused(format!("{} has no tag `{tag}`", id.as_str()))
        })?;
        let expected = match &req.expected_commit {
            Some(want) if !commit_matches(want, &remote_commit) => {
                return Err(MarketplaceError::Refused(format!(
                    "tag {tag} of {} names commit {remote_commit}, not the expected {want}",
                    id.as_str()
                )));
            }
            Some(want) => want.clone(),
            None => remote_commit.clone(),
        };
        log(&format!("tag {tag} is commit {remote_commit}"));

        // Fetch: shallow clone of exactly that tag.
        let scratch = self
            .state_dir
            .join(SCRATCH_DIR)
            .join(job_id)
            .join(id.dir_name());
        let _cleanup = Cleanup(scratch.clone());
        if let Some(parent) = scratch.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MarketplaceError::Io(format!("{}: {e}", parent.display())))?;
        }
        jobs.phase(job_id, Phase::Fetch, Some(10));
        git.clone_tag(&url, &tag, &scratch, &mut |line| log(line))
            .map_err(|e| MarketplaceError::Git(e.to_string()))?;
        jobs.phase(job_id, Phase::Fetch, Some(20));

        // Verify: the clone is the commit ls-remote named, and the manifest agrees.
        jobs.phase(job_id, Phase::Verify, Some(30));
        let head = git
            .head_commit(&scratch)
            .map_err(|e| MarketplaceError::Git(e.to_string()))?;
        if !commit_matches(&expected, &head) {
            return Err(MarketplaceError::Refused(format!(
                "clone of {tag} is at {head} but the tag was announced as {expected}"
            )));
        }
        let manifest =
            read_manifest(&scratch).map_err(|e| MarketplaceError::Refused(e.to_string()))?;
        if manifest.id() != &id {
            return Err(MarketplaceError::Refused(format!(
                "{url} says it is {}, not {}",
                manifest.id().as_str(),
                id.as_str()
            )));
        }
        if manifest.tag() != tag {
            return Err(MarketplaceError::Refused(format!(
                "tag {tag} does not match the manifest version {}",
                manifest.module.version
            )));
        }
        check_free_build(&manifest).map_err(MarketplaceError::Refused)?;
        log(&format!(
            "manifest ok: {} {} ({} capabilities)",
            manifest.module.name,
            manifest.module.version,
            manifest.capabilities.len()
        ));

        // Verify: what it requires must be installed.
        for requirement in &manifest.requires {
            self.ensure_requirement(job_id, requirement, toolchain, depth, &log)?;
        }

        // Build.
        jobs.phase(job_id, Phase::Build, Some(50));
        let cargo = toolchain
            .cargo
            .clone()
            .unwrap_or_else(|| PathBuf::from("cargo"));
        let target_dir = scratch.join("target");
        let mut cmd = std::process::Command::new(&cargo);
        cmd.current_dir(&scratch)
            .env("CARGO_TARGET_DIR", &target_dir)
            .env("CARGO_TERM_COLOR", "never")
            .args(["build", "--release", "--locked"]);
        if let Some(bin) = &manifest.distribution.bin {
            cmd.args(["--bin", bin]);
        }
        if let Some(p) = self.path() {
            cmd.env("PATH", p);
        }
        let mut compiled = 0u8;
        run_streaming(cmd, "cargo build", &mut |line| {
            if line.trim_start().starts_with("Compiling") {
                compiled = compiled.saturating_add(1);
                jobs.progress(job_id, 50 + compiled.min(35));
            }
            log(line);
        })
        .map_err(|e| MarketplaceError::Build(e.to_string()))?;
        let binary = target_dir.join("release").join(format!(
            "{}{}",
            binary_name(&id, &manifest),
            std::env::consts::EXE_SUFFIX
        ));
        if !binary.is_file() {
            return Err(MarketplaceError::Build(format!(
                "cargo finished but produced no {}",
                binary.display()
            )));
        }

        // Install: hash, record, store, activate, enable.
        jobs.phase(job_id, Phase::Install, Some(90));
        let sha256 = hash_file(&binary)?;
        log(&format!("binary sha256 {sha256}"));
        let accepted = req.accepted.clone().unwrap_or_else(|| {
            manifest
                .capabilities
                .iter()
                .copied()
                .filter(|c| !c.is_escape_hatch())
                .collect()
        });
        let version = manifest.module.version.clone();
        let record = InstallRecord {
            module_id: id.clone(),
            repo: url.clone(),
            tag: tag.clone(),
            commit: head.clone(),
            version: version.clone(),
            artifact_sha256: sha256,
            source: manifest.distribution.kind,
            accepted,
            manifest,
            installed_at: cache::now_secs(),
            kind: req.kind,
        };
        self.store.install(record, &binary)?;
        self.store.activate(&id, &version)?;
        jobs.phase(job_id, Phase::Install, Some(95));
        if let Some(ws) = &req.workspace {
            self.workspaces.set_enabled(ws, &id, true)?;
            log(&format!("enabled in workspace {ws}"));
        }
        log(&format!("installed {} {version}", id.as_str()));
        Ok(version)
    }

    /// Satisfy one `requires` entry: already provided by an installed module, or
    /// install the declared provider's newest tag as a dependency.
    fn ensure_requirement(
        &self,
        job_id: &str,
        requirement: &Requirement,
        toolchain: &Toolchain,
        depth: usize,
        log: &dyn Fn(&str),
    ) -> Result<(), MarketplaceError> {
        if self.provided(requirement)? {
            log(&format!(
                "requires {} {}: provided",
                requirement.shape, requirement.version
            ));
            return Ok(());
        }
        let Some(provider) = &requirement.provider else {
            return Err(MarketplaceError::Refused(format!(
                "requires {} {} but nothing installed provides it and no provider is named \
                 (install a provider first)",
                requirement.shape, requirement.version
            )));
        };
        if depth >= DEPENDENCY_DEPTH {
            return Err(MarketplaceError::Refused(format!(
                "dependency chain deeper than {DEPENDENCY_DEPTH} at {}",
                provider.as_str()
            )));
        }
        log(&format!(
            "requires {} {}: installing {} as a dependency",
            requirement.shape,
            requirement.version,
            provider.as_str()
        ));
        let dep = InstallRequest {
            module: provider.as_str().to_string(),
            tag: None,
            accepted: None,
            workspace: None,
            expected_commit: None,
            kind: InstallKind::Dependency,
        };
        self.run_install(job_id, &dep, toolchain, depth + 1)?;
        if !self.provided(requirement)? {
            return Err(MarketplaceError::Refused(format!(
                "{} was installed but does not provide {} {}",
                provider.as_str(),
                requirement.shape,
                requirement.version
            )));
        }
        Ok(())
    }

    /// Whether an installed module provides the shape at an acceptable version.
    fn provided(&self, requirement: &Requirement) -> Result<bool, MarketplaceError> {
        for status in self.store.records()? {
            if let RecordStatus::Ok(installed) = status {
                if let Some(provider) = &requirement.provider {
                    if &installed.id != provider {
                        continue;
                    }
                }
                let satisfied = installed.rights().manifest.provides.iter().any(|p| {
                    p.shape == requirement.shape && requirement.version.matches(&p.version)
                });
                if satisfied {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Every job, oldest first.
    pub fn jobs(&self) -> Vec<Job> {
        self.jobs.list()
    }

    /// One job.
    pub fn job(&self, id: &str) -> Result<Job, MarketplaceError> {
        self.jobs
            .get(id)
            .ok_or_else(|| MarketplaceError::NoSuchJob(id.to_string()))
    }

    // ---- state

    /// Enable or disable an installed module in a workspace.
    pub fn set_enabled(
        &self,
        workspace: &str,
        module: &str,
        enabled: bool,
    ) -> Result<BTreeMap<String, bool>, MarketplaceError> {
        let id = Self::parse_id(module)?;
        Self::check_workspace(workspace)?;
        if self.store.installed_versions(&id)?.is_empty() {
            return Err(MarketplaceError::NotInstalled(module.to_string()));
        }
        self.workspaces.set_enabled(workspace, &id, enabled)?;
        Ok(self.workspaces.enabled_in(&id))
    }

    /// Remove one installed version; the last one removed forgets workspace state too.
    pub fn uninstall(&self, module: &str, version: &str) -> Result<(), MarketplaceError> {
        let id = Self::parse_id(module)?;
        let version = Version::parse(version)
            .map_err(|_| MarketplaceError::NotInstalled(format!("{module}@{version}")))?;
        match self.store.uninstall(&id, &version) {
            Ok(()) => {}
            Err(InstallError::NotInstalled { .. }) => {
                return Err(MarketplaceError::NotInstalled(format!(
                    "{module}@{version}"
                )))
            }
            Err(e) => return Err(e.into()),
        }
        if self.store.installed_versions(&id)?.is_empty() {
            self.workspaces.remove_module(&id)?;
        }
        Ok(())
    }

    /// Every installed version, verified or reported broken.
    pub fn installed(&self) -> Result<Vec<InstalledView>, MarketplaceError> {
        let mut out = Vec::new();
        for status in self.store.records()? {
            out.push(match status {
                RecordStatus::Ok(i) => {
                    let r = i.rights();
                    InstalledView {
                        module: Some(i.id.as_str().to_string()),
                        version: Some(i.version.to_string()),
                        active: i.active,
                        kind: Some(r.kind),
                        tag: Some(r.tag.clone()),
                        commit: Some(r.commit.clone()),
                        sha256: Some(r.artifact_sha256.clone()),
                        accepted: r.accepted.iter().copied().collect(),
                        enabled: self.workspaces.enabled_in(&i.id),
                        broken: None,
                    }
                }
                RecordStatus::Broken {
                    id,
                    version,
                    reason,
                    ..
                } => InstalledView {
                    module: id.as_ref().map(|i| i.as_str().to_string()),
                    version: version.map(|v| v.to_string()),
                    active: false,
                    kind: None,
                    tag: None,
                    commit: None,
                    sha256: None,
                    accepted: Vec::new(),
                    enabled: id
                        .map(|i| self.workspaces.enabled_in(&i))
                        .unwrap_or_default(),
                    broken: Some(reason),
                },
            });
        }
        Ok(out)
    }

    // ---- sign-in

    /// Whether a GitHub token is stored.
    pub fn signed_in(&self) -> bool {
        matches!(self.tokens.get(), Ok(Some(_)))
    }

    /// Forget the stored token.
    pub fn sign_out(&self) -> Result<(), MarketplaceError> {
        Ok(self.tokens.clear()?)
    }

    /// Start the device flow. The answer is what to show the user.
    pub async fn signin_start(&self) -> Result<SignInView, MarketplaceError> {
        let start = match self.github.device_start().await {
            Ok(s) => s,
            Err(GitHubError::NoClientId) => {
                return Err(MarketplaceError::SignInUnavailable(
                    GitHubError::NoClientId.to_string(),
                ))
            }
            Err(e) => return Err(e.into()),
        };
        let view = SignInView {
            id: uuid::Uuid::new_v4().to_string(),
            user_code: start.user_code,
            verification_uri: start.verification_uri,
            expires_at: cache::now_secs() + start.expires_in,
            interval: start.interval,
            status: SignInStatus::Pending,
        };
        self.signins.lock().unwrap().insert(
            view.id.clone(),
            SignIn {
                device_code: start.device_code,
                view: view.clone(),
            },
        );
        Ok(view)
    }

    /// Poll a sign-in; on success the token is stored and never returned.
    pub async fn signin_poll(&self, id: &str) -> Result<SignInView, MarketplaceError> {
        let (device_code, view) = {
            let signins = self.signins.lock().unwrap();
            let s = signins
                .get(id)
                .ok_or_else(|| MarketplaceError::NoSuchSignIn(id.to_string()))?;
            (s.device_code.clone(), s.view.clone())
        };
        if view.status != SignInStatus::Pending {
            return Ok(view);
        }
        let status = match self.github.device_poll(&device_code).await? {
            DevicePoll::Pending | DevicePoll::SlowDown => SignInStatus::Pending,
            DevicePoll::Expired => SignInStatus::Expired,
            DevicePoll::Denied => SignInStatus::Denied,
            DevicePoll::Token(token) => {
                self.tokens.set(&token)?;
                SignInStatus::Done
            }
        };
        let mut signins = self.signins.lock().unwrap();
        let Some(s) = signins.get_mut(id) else {
            return Err(MarketplaceError::NoSuchSignIn(id.to_string()));
        };
        s.view.status = status;
        if status != SignInStatus::Pending {
            // The device code has served; wipe it now rather than at process exit.
            s.device_code = Token::new("");
        }
        Ok(s.view.clone())
    }
}

/// `want` may be a prefix (the UI shows short hashes); compare case-insensitively.
fn commit_matches(want: &str, actual: &str) -> bool {
    let want = want.trim().to_ascii_lowercase();
    want.len() >= 7 && actual.to_ascii_lowercase().starts_with(&want)
}

/// Removes the scratch checkout when the pipeline leaves, however it leaves.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
