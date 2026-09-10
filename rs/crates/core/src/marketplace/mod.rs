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
pub mod loader;
// ---- track G6 resolver
pub mod resolve;
// ---- end track G6 resolver
pub mod token;
pub mod toolchain;
pub mod workspace;

#[cfg(test)]
pub(crate) mod testing;

// The live end-to-end proof. Ignored by default: it reaches github.com and builds.
#[cfg(test)]
mod live;

// The G6/G7/G10 wiring proofs, which are about this module rather than part of it.
// Unix-only because they drive the pipeline through `testing::fake_tools`, a shell script.
#[cfg(all(test, unix))]
mod wiring_tests;

use crate::install::{
    hash_file, FileKeyStore, InstallError, InstallPaths, InstallStore, KeyStore, RecordStatus,
};
use crate::persistence::lockfile::LockfileIoError;
// ---- track G6 resolver
use crate::install::resolver::{self, Defaults, InstalledVersion};
use resolve::MarketplaceSource;
use semver::VersionReq;
// ---- end track G6 resolver
// ---- track G7 policy
use crate::policy::{self, Decision, Policy, Verifier};
// ---- end track G7 policy
use crate::edition::Edition;
use avada_module_sdk::caps::Capability;
use avada_module_sdk::manifest::{DistributionKind, Manifest, ModuleId};
use avada_module_sdk::rights::{InstallKind, InstallRecord};
use cache::Cache;
use fetch::{check_edition, newest_tag, read_manifest, Git};
use github::{DevicePoll, GitHubApi, GitHubConfig, GitHubError, HttpGitHub, RepoSummary, TagInfo};
use job::{Job, JobBook, Phase};
use loader::{LoadError, LoadRequest, Loader, SourceLoader};
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

/// Environment variable that points installs at a different git root, for offline and
/// sandbox runs: `file:///some/dir` makes `owner/repo` clone from
/// `/some/dir/owner/repo.git`. Empty or unset means GitHub.
pub const GIT_BASE_ENV: &str = "AVADA_MARKETPLACE_GIT_BASE";

impl MarketplaceOptions {
    /// The defaults, with [`GIT_BASE_ENV`] honoured. `Default` stays pure so tests that
    /// build options by hand are unaffected by the caller's environment; only the
    /// production constructors ([`Marketplace::open_under`]) go through here.
    pub fn from_env() -> Self {
        let raw = std::env::var_os(GIT_BASE_ENV).map(|v| v.to_string_lossy().into_owned());
        Self::with_git_base_override(raw.as_deref())
    }

    /// `from_env` with the variable's value passed in, so it can be tested without the
    /// process-wide `set_var`. Blank means "no override"; a trailing slash is dropped
    /// because the clone URL appends `/owner/repo.git`.
    pub fn with_git_base_override(raw: Option<&str>) -> Self {
        let mut o = MarketplaceOptions::default();
        if let Some(base) = raw.map(str::trim).filter(|b| !b.is_empty()) {
            o.git_base = base.trim_end_matches('/').to_string();
        }
        o
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
    // ---- track G7 policy
    /// The notarization verifier applied to every artifact before it is recorded.
    /// A field rather than a [`MarketplaceOptions`] entry because that struct is
    /// literal-constructed in tests; this way the swap seam (a stricter commercial
    /// verifier) and the test hook are the same one line.
    verifier: Mutex<Arc<dyn Verifier>>,
    // ---- end track G7 policy
    // ---- track G10 commercial
    /// How an artifact is obtained, one loader per distribution kind. The free build
    /// ships exactly one; the commercial crate adds the precompiled one at startup.
    loaders: Mutex<Vec<Arc<dyn Loader>>>,
    /// Which edition's rules this marketplace applies. A field rather than a `cfg!`
    /// read at the point of use so that a free build can drive the commercial path in
    /// a test — the only way the commercial path is tested before the private crate
    /// exists. Defaults to whatever this binary was compiled as.
    edition: Mutex<Edition>,
    // ---- end track G10 commercial
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
            // ---- track G7 policy
            verifier: Mutex::new(policy::platform_verifier()),
            // ---- end track G7 policy
            // ---- track G10 commercial
            loaders: Mutex::new(vec![Arc::new(SourceLoader)]),
            edition: Mutex::new(Edition::CURRENT),
            // ---- end track G10 commercial
        }
    }

    // ---- track G7 policy

    /// Replace the notarization verifier. The commercial build swaps in a stricter one
    /// (Developer ID team pinning, EV certificate pinning); tests swap in a fake.
    pub fn set_verifier(&self, verifier: Arc<dyn Verifier>) {
        *self.verifier.lock().expect("verifier lock") = verifier;
    }

    /// The verifier in force.
    pub fn verifier(&self) -> Arc<dyn Verifier> {
        Arc::clone(&self.verifier.lock().expect("verifier lock"))
    }

    /// Assess `binary` and either let the install proceed or fail the job.
    ///
    /// Called once per artifact, after it exists and before the install record that
    /// blesses it is written — the last moment at which refusing costs nothing but a
    /// scratch directory. Returns the assessment to record beside the installed
    /// artifact.
    fn check_notarization(
        &self,
        id: &ModuleId,
        manifest: &Manifest,
        binary: &Path,
        commit: &str,
        url: &str,
        log: &dyn Fn(&str),
    ) -> Result<policy::RecordedVerdict, MarketplaceError> {
        let (policy, complaint) = Policy::load_or_default(self.store.paths().root());
        if let Some(complaint) = complaint {
            // Never silent: a policy file nobody can parse is not a policy of "allow".
            log(&format!("{complaint}; falling back to the default policy"));
        }
        // The free pipeline only ever compiles source, so this is `Built` today. The
        // mapping is written out anyway so the commercial binary path arrives with its
        // half of the policy already wired.
        let source = match manifest.distribution.kind {
            DistributionKind::Source => policy::Source::Built {
                commit: commit.to_string(),
            },
            DistributionKind::Binary => policy::Source::Prebuilt {
                url: url.to_string(),
            },
        };
        let verifier = self.verifier();
        let ctx = policy.context(id, source.clone());
        let verdict = verifier.verify(binary, &ctx);
        let decision = policy::decide(&policy, &verdict, &source);
        log(&format!("notarization: {verdict} -> {decision}"));
        if let Decision::Refuse { reason } = &decision {
            return Err(MarketplaceError::Refused(reason.clone()));
        }
        Ok(policy::RecordedVerdict::new(
            &verdict,
            &decision,
            &source,
            verifier.name(),
        ))
    }

    // ---- end track G7 policy

    // ---- track G10 commercial

    /// Add a loader, ahead of the ones already installed.
    ///
    /// The commercial crate calls this at startup with its precompiled loader; tests
    /// call it with a fake. Installing a second loader for a kind that already has one
    /// replaces it in effect, because the search takes the first match — which is how
    /// the commercial build overrides the source path if it ever needs to.
    pub fn install_loader(&self, loader: Arc<dyn Loader>) {
        self.loaders.lock().expect("loader lock").insert(0, loader);
    }

    /// The edition whose rules this marketplace applies.
    pub fn edition(&self) -> Edition {
        *self.edition.lock().expect("edition lock")
    }

    /// Change the edition in force. The commercial crate calls this at startup beside
    /// [`Marketplace::install_loader`]; tests call it to exercise the other branch.
    pub fn set_edition(&self, edition: Edition) {
        *self.edition.lock().expect("edition lock") = edition;
    }

    /// The loader that serves `kind`, or a refusal naming the edition.
    fn loader_for(&self, kind: DistributionKind) -> Result<Arc<dyn Loader>, MarketplaceError> {
        let loaders = self.loaders.lock().expect("loader lock");
        loaders
            .iter()
            .find(|l| l.serves(kind))
            .map(Arc::clone)
            .ok_or_else(|| {
                // In a free build `check_edition` has already refused a prebuilt module
                // by the time anything asks for a loader, so reaching this is either a
                // commercial build that forgot to install one or a kind nobody serves.
                MarketplaceError::Refused(format!(
                    "no loader in the {} edition can produce a {} module",
                    self.edition(),
                    match kind {
                        DistributionKind::Source => "source",
                        DistributionKind::Binary => "prebuilt",
                    }
                ))
            })
    }

    // ---- end track G10 commercial

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
            MarketplaceOptions::from_env(),
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
    ///
    /// `options.path` of `None` means "inherit the process `PATH`", which is what the
    /// subprocess spawner below does with it. [`Toolchain::detect_in`] reads `None` the
    /// other way round — as an empty path — so forwarding it straight through would
    /// report a missing toolchain on every machine that had not injected one, and the
    /// production wiring (`MarketplaceOptions::default()`) is exactly that case.
    /// Detection and execution have to agree about which `PATH` they mean.
    pub fn toolchain(&self) -> Toolchain {
        match self.path() {
            Some(p) => Toolchain::detect_in(Some(p)),
            None => Toolchain::detect(),
        }
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
                match me.run_install(&job_id, &req, &toolchain, true) {
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

    /// The whole pipeline for one module, on the calling thread.
    ///
    /// `root` marks the module the user asked for. Its manifest is resolved against
    /// the whole dependency graph (see [`Self::install_dependencies`]) and every
    /// module the plan needs is installed first, on this same job, so their log lines
    /// interleave prefixed by module.
    fn run_install(
        &self,
        job_id: &str,
        req: &InstallRequest,
        toolchain: &Toolchain,
        root: bool,
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
        if root {
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
        check_edition(self.edition(), &manifest).map_err(MarketplaceError::Refused)?;
        log(&format!(
            "manifest ok: {} {} ({} capabilities)",
            manifest.module.name,
            manifest.module.version,
            manifest.capabilities.len()
        ));

        // ---- track G6 resolver
        // Verify: resolve the whole graph and install what it needs, first.
        if root {
            self.install_dependencies(job_id, req, &manifest, &git, toolchain, &log)?;
        }
        // ---- end track G6 resolver

        // ---- track G10 commercial
        // Build, or download: which one is the loader's business, and past this line
        // the two are the same artifact.
        jobs.phase(job_id, Phase::Build, Some(50));
        let loader = self.loader_for(manifest.distribution.kind)?;
        let request = LoadRequest {
            id: &id,
            manifest: &manifest,
            repo: &url,
            tag: &tag,
            commit: &head,
            scratch: &scratch,
            cargo: toolchain.cargo.clone(),
            path: self.path().map(OsStr::to_os_string),
        };
        let binary = loader
            .load(&request, &log, &|pct| jobs.progress(job_id, pct))
            .map_err(|e| match e {
                LoadError::Refused(m) => MarketplaceError::Refused(m),
                LoadError::Failed(m) => MarketplaceError::Build(m),
            })?;
        log(&format!("{} produced {}", loader.name(), binary.display()));
        // ---- end track G10 commercial

        // ---- track G7 policy
        // Between "the artifact exists" and "the record blesses it": assess it, and
        // fail the job if the policy refuses.
        let assessment =
            self.check_notarization(&id, &manifest, &binary, &head, &url, &log as &dyn Fn(&str))?;
        // ---- end track G7 policy

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
        // ---- track G6 resolver
        let claims_defaults = (req.kind == InstallKind::Manual && !manifest.provides.is_empty())
            .then(|| manifest.clone());
        // ---- end track G6 resolver
        let record = InstallRecord {
            module_id: id.clone(),
            repo: url.clone(),
            tag: tag.clone(),
            commit: head.clone(),
            version: version.clone(),
            artifact_sha256: sha256,
            // Left for `install` to fill: the pin has to describe the tree the host
            // actually staged, and only `install` has seen that tree.
            skills_sha256: String::new(),
            source: manifest.distribution.kind,
            accepted,
            manifest,
            installed_at: cache::now_secs(),
            kind: req.kind,
        };
        // The checkout, not just the artifact: a module that ships skills needs its
        // `[skills] paths` staged into the version directory before the tree is swept.
        let installed = self.store.install(record, &binary, Some(&scratch))?;
        // ---- track G7 policy
        // Remembered next to the installed artifact so the spawn path can refuse a
        // binary installed under a refusal. Not fatal if it cannot be written: the
        // sidecar can only ever add a refusal, never remove one.
        if let Err(e) = policy::write_verdict(&installed.binary, &assessment) {
            log(&format!("could not record the notarization verdict: {e}"));
        }
        // ---- end track G7 policy
        self.store.activate(&id, &version)?;
        // ---- track G6 resolver
        if let Some(m) = &claims_defaults {
            self.note_defaults(m, req.kind, &log);
        }
        // ---- end track G6 resolver
        jobs.phase(job_id, Phase::Install, Some(95));
        if let Some(ws) = &req.workspace {
            self.workspaces.set_enabled(ws, &id, true)?;
            log(&format!("enabled in workspace {ws}"));
        }
        log(&format!("installed {} {version}", id.as_str()));
        Ok(version)
    }

    // ---- track G6 resolver

    /// Resolve the whole dependency graph of one install request and install
    /// everything it needs, dependencies first.
    ///
    /// The root's manifest is already in hand — this job cloned and verified it — so
    /// it is handed to the resolver directly and never fetched again. Everything else
    /// the plan reaches comes from the install store (already installed) or from a
    /// throwaway shallow clone of one tag (see [`resolve::MarketplaceSource`]).
    ///
    /// One plan covers the whole job: `resolve` either returns an order that satisfies
    /// every version requirement, `requires` shape and workspace pin at once, or a
    /// [`resolver::Conflict`] explaining which of them cannot hold together. There is
    /// no depth limit any more; a cycle is a conflict, not a runaway recursion.
    fn install_dependencies(
        &self,
        job_id: &str,
        req: &InstallRequest,
        root: &Manifest,
        git: &Git,
        toolchain: &Toolchain,
        log: &dyn Fn(&str),
    ) -> Result<(), MarketplaceError> {
        let id = root.id().clone();
        let version = root.module.version.clone();

        // What is on this machine already: candidate versions the resolver may keep,
        // and their manifests, without a single fetch.
        let mut known: BTreeMap<(ModuleId, Version), Manifest> = BTreeMap::new();
        let mut installed: Vec<InstalledVersion> = Vec::new();
        for status in self.store.records()? {
            if let RecordStatus::Ok(i) = status {
                let rights = i.rights();
                installed.push(InstalledVersion {
                    id: i.id.clone(),
                    version: i.version.clone(),
                    active: i.active,
                    installed_at: rights.installed_at,
                });
                known.insert((i.id.clone(), i.version.clone()), rights.manifest.clone());
            }
        }
        known.insert((id.clone(), version.clone()), root.clone());

        let pins = match &req.workspace {
            Some(ws) => self.workspaces.pins(ws)?,
            None => BTreeMap::new(),
        };
        let defaults = Defaults::load(self.store.paths())?;

        // Manifest clones live here and are removed as soon as they have been read;
        // this guard drops before the root's own scratch clone, which then empties
        // the job directory.
        let src = self.state_dir.join(SCRATCH_DIR).join(job_id).join("src");
        let _src_cleanup = Cleanup(src.clone());
        let source = MarketplaceSource::new(git, &self.options.git_base, &src, known, &log);

        let roots = [resolver::Root {
            id: id.clone(),
            version: VersionReq::parse(&format!("={version}")).map_err(|e| {
                MarketplaceError::Refused(format!("{version} is not a usable version: {e}"))
            })?,
        }];
        let plan = resolver::resolve(&roots, &source, &installed, &defaults, &pins)
            .map_err(|c| MarketplaceError::Refused(c.to_string()))?;

        // The free build refuses a commercial or prebuilt module anywhere in the plan,
        // not only at the root, and says which one.
        for step in &plan.steps {
            if step.id == id {
                continue; // already checked, with the plain message
            }
            check_edition(self.edition(), &step.manifest)
                .map_err(|e| MarketplaceError::Refused(format!("{}: {e}", step.id.as_str())))?;
        }

        for choice in &plan.providers {
            let ready = choice
                .providers
                .iter()
                .all(|(p, v)| plan.step(p, v).is_none_or(|s| s.installed));
            let who = if choice.requirer == id {
                String::new()
            } else {
                format!("{} ", choice.requirer.as_str())
            };
            if ready {
                log(&format!(
                    "{who}requires {} {}: provided",
                    choice.shape, choice.version
                ));
            } else {
                let names: Vec<&str> = choice.providers.iter().map(|(p, _)| p.as_str()).collect();
                log(&format!(
                    "{who}requires {} {}: installing {} as a dependency",
                    choice.shape,
                    choice.version,
                    names.join(", ")
                ));
            }
        }

        // Post-order: every step's own dependencies come before it, and the root last.
        for step in plan.to_install() {
            if step.root {
                continue; // the caller is installing it, from the clone it already has
            }
            let dep = InstallRequest {
                module: step.id.as_str().to_string(),
                tag: Some(format!("v{}", step.version)),
                accepted: None,
                workspace: None,
                expected_commit: None,
                kind: InstallKind::Dependency,
            };
            self.run_install(job_id, &dep, toolchain, false)?;
        }
        Ok(())
    }

    /// A hand install claims every shape it provides that has no default provider yet
    /// (`docs/modules-fanout-plan.md` §2). A dependency install never does: the user
    /// did not choose it, so it must not silently become the answer to a shape.
    fn note_defaults(&self, manifest: &Manifest, kind: InstallKind, log: &dyn Fn(&str)) {
        if kind != InstallKind::Manual || manifest.provides.is_empty() {
            return;
        }
        let paths = self.store.paths();
        let mut defaults = match Defaults::load(paths) {
            Ok(d) => d,
            Err(e) => {
                log(&format!("could not read the provider defaults: {e}"));
                return;
            }
        };
        let newly = defaults.note_manual_install(manifest);
        if newly.is_empty() {
            return;
        }
        match defaults.save(paths) {
            Ok(()) => log(&format!("default provider for {}", newly.join(", "))),
            Err(e) => log(&format!("could not record the provider defaults: {e}")),
        }
    }

    // ---- end track G6 resolver

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

    // ---- track G6 resolver

    /// Pin `module` to `version` in `workspace`, refusing a pin that would break
    /// something already enabled there.
    ///
    /// The check is [`resolver::check_pin`]: every enabled module's dependency and
    /// named-requirement demands on `module` are gathered, and if the pinned version
    /// fails any of them the refusal names each broken demand and offers the nearest
    /// installed version that keeps them all working, up or down.
    pub fn pin(
        &self,
        workspace: &str,
        module: &str,
        version: &str,
    ) -> Result<BTreeMap<ModuleId, Version>, MarketplaceError> {
        let id = Self::parse_id(module)?;
        Self::check_workspace(workspace)?;
        let version = Version::parse(version)
            .map_err(|_| MarketplaceError::NotInstalled(format!("{module}@{version}")))?;
        let mut candidates: Vec<(Version, Manifest)> = Vec::new();
        let mut demands: Vec<resolver::Demand> = Vec::new();
        let enabled = self.workspaces.get(workspace)?.enabled;
        for status in self.store.records()? {
            let RecordStatus::Ok(i) = status else {
                continue;
            };
            let manifest = i.rights().manifest.clone();
            if i.id == id {
                candidates.push((i.version.clone(), manifest));
                continue;
            }
            if enabled.get(&i.id) != Some(&true) {
                continue; // only what this workspace actually runs constrains the pin
            }
            for (dep, req) in &manifest.dependencies {
                if dep == &id {
                    demands.push(resolver::Demand {
                        requirer: i.id.clone(),
                        requirer_version: i.version.clone(),
                        kind: resolver::DemandKind::Identity(req.clone()),
                    });
                }
            }
            for r in &manifest.requires {
                if r.provider.as_ref() == Some(&id) {
                    demands.push(resolver::Demand {
                        requirer: i.id.clone(),
                        requirer_version: i.version.clone(),
                        kind: resolver::DemandKind::Shape {
                            shape: r.shape.clone(),
                            version: r.version.clone(),
                        },
                    });
                }
            }
        }
        resolver::check_pin(&id, &version, &candidates, &demands)
            .map_err(|c| MarketplaceError::Refused(c.message))?;
        Ok(self.workspaces.set_pin(workspace, &id, &version)?.pins)
    }

    /// Forget the pin on `module` in `workspace`.
    pub fn unpin(
        &self,
        workspace: &str,
        module: &str,
    ) -> Result<BTreeMap<ModuleId, Version>, MarketplaceError> {
        let id = Self::parse_id(module)?;
        Self::check_workspace(workspace)?;
        Ok(self.workspaces.clear_pin(workspace, &id)?.pins)
    }

    /// The versions pinned in `workspace`.
    pub fn pins(&self, workspace: &str) -> Result<BTreeMap<ModuleId, Version>, MarketplaceError> {
        Self::check_workspace(workspace)?;
        Ok(self.workspaces.pins(workspace)?)
    }

    /// The user's provider defaults: shape → the module they want answering it.
    pub fn defaults(&self) -> Result<BTreeMap<String, ModuleId>, MarketplaceError> {
        Ok(Defaults::load(self.store.paths())?.shapes)
    }

    /// Make `module` the default provider of `shape`.
    ///
    /// A default is a promise the resolver will keep, so it is refused unless it can be:
    /// the module must be installed, and some installed version of it must actually
    /// `provide` the shape. Without those checks `defaults.json` could carry a
    /// preference that silently never applies, and the resolver has no way to complain
    /// about a shape nobody offers.
    pub fn set_default(
        &self,
        shape: &str,
        module: &str,
    ) -> Result<BTreeMap<String, ModuleId>, MarketplaceError> {
        let id = Self::parse_id(module)?;
        if shape.trim().is_empty() {
            return Err(MarketplaceError::Refused(
                "a shape name cannot be empty".into(),
            ));
        }
        let mut installed = false;
        let mut provides = false;
        for status in self.store.records()? {
            let RecordStatus::Ok(i) = status else {
                continue;
            };
            if i.id != id {
                continue;
            }
            installed = true;
            if i.rights()
                .manifest
                .provides
                .iter()
                .any(|p| p.shape == shape)
            {
                provides = true;
                break;
            }
        }
        if !installed {
            return Err(MarketplaceError::NotInstalled(module.to_string()));
        }
        if !provides {
            return Err(MarketplaceError::Refused(format!(
                "{module} does not provide `{shape}`"
            )));
        }
        let mut defaults = Defaults::load(self.store.paths())?;
        defaults.set_default(shape, &id);
        defaults.save(self.store.paths())?;
        Ok(defaults.shapes)
    }

    /// Forget the default for `shape`. A shape that had none is not an error — the
    /// caller asked for a state and that state now holds.
    pub fn clear_default(
        &self,
        shape: &str,
    ) -> Result<BTreeMap<String, ModuleId>, MarketplaceError> {
        let mut defaults = Defaults::load(self.store.paths())?;
        defaults.shapes.remove(shape);
        defaults.save(self.store.paths())?;
        Ok(defaults.shapes)
    }

    // ---- end track G6 resolver

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
            // ---- track G6 resolver
            // The module is gone: it can no longer be the default provider of anything.
            let paths = self.store.paths();
            let mut defaults = Defaults::load(paths)?;
            if !defaults.clear_module(&id).is_empty() {
                defaults.save(paths)?;
            }
            // ---- end track G6 resolver
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

#[cfg(test)]
mod options_tests {
    use super::MarketplaceOptions;

    #[test]
    fn git_base_override_is_trimmed_and_blank_means_github() {
        assert_eq!(
            MarketplaceOptions::with_git_base_override(None).git_base,
            "https://github.com"
        );
        assert_eq!(
            MarketplaceOptions::with_git_base_override(Some("  ")).git_base,
            "https://github.com"
        );
        // A trailing slash would double up against the `/owner/repo.git` the clone appends.
        assert_eq!(
            MarketplaceOptions::with_git_base_override(Some(" file:///srv/mirror/ ")).git_base,
            "file:///srv/mirror"
        );
        // The override never touches the local path override.
        assert_eq!(
            MarketplaceOptions::with_git_base_override(Some("file:///x")).path,
            None
        );
    }
}
