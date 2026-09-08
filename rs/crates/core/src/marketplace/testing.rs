//! Marketplace tests and the fixtures the route tests reuse: a fake GitHub on
//! 127.0.0.1, fixture git repositories served over `file://`, and fake `cargo`/`rustup`
//! first on an injected `PATH`. No real network, no real cargo, no real `HOME`.

use super::github::{GitHubConfig, HttpGitHub};
use super::job::Phase;
use super::token::{MemoryTokenStore, Token, TokenStore};
use super::*;
use crate::install::{InstallPaths, InstallStore, MemoryKeyStore};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- fixtures

/// A scratch directory unique to this test.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "avada-mp-{name}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// How the fake device flow answers polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DeviceOutcome {
    #[default]
    Pending,
    Approved,
    Denied,
    Expired,
}

/// What the fake GitHub knows and what it saw.
#[derive(Default)]
pub(crate) struct FakeState {
    /// Search items (GitHub's shape).
    pub repos: Vec<Value>,
    /// `owner/repo` → `[(tag, sha)]`.
    pub tags: BTreeMap<String, Vec<(String, String)>>,
    /// `owner/repo/path` → file body.
    pub files: BTreeMap<String, String>,
    /// Every GET seen: (path and query, had a bearer token, `If-None-Match` value).
    pub requests: Vec<(String, bool, Option<String>)>,
    /// The ETag every 200 carries.
    pub etag: String,
    /// Answer every GET with 403 (rate limit).
    pub rate_limited: bool,
    /// The device flow's answer.
    pub outcome: DeviceOutcome,
    /// The device code the fake issued.
    pub issued_device_code: Option<String>,
    /// Device codes the fake was polled with.
    pub polled_with: Vec<String>,
}

/// The token the fake device flow issues on approval.
pub(crate) const FAKE_ACCESS_TOKEN: &str = "gho_fake_access_token_for_tests";
/// A token tests hand to the store directly.
pub(crate) const FAKE_STORED_TOKEN: &str = "ghp_fake_stored_token_for_tests";

/// The fake GitHub server.
pub(crate) struct FakeGitHub {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    pub state: Arc<Mutex<FakeState>>,
}

pub(crate) fn repo_item(full_name: &str, description: &str, stars: u64) -> Value {
    json!({
        "full_name": full_name,
        "description": description,
        "html_url": format!("https://github.com/{full_name}"),
        "stargazers_count": stars,
        "pushed_at": "2026-09-01T00:00:00Z",
    })
}

impl FakeGitHub {
    /// Start on a free port, serving `state`.
    pub(crate) async fn start(state: FakeState) -> Self {
        let state = Arc::new(Mutex::new(state));
        let app = Router::new()
            .route("/search/repositories", get(search))
            .route("/repos/{owner}/{repo}", get(repo))
            .route("/repos/{owner}/{repo}/tags", get(tags))
            .route("/repos/{owner}/{repo}/contents/{*path}", get(contents))
            .route("/login/device/code", post(device_code))
            .route("/login/oauth/access_token", post(access_token))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeGitHub {
            base: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    /// A config pointing the real client at this fake.
    pub(crate) fn config(&self, ttl: Duration, client_id: &str) -> GitHubConfig {
        GitHubConfig {
            api_base: self.base.clone(),
            web_base: self.base.clone(),
            client_id: client_id.to_string(),
            ttl,
            user_agent: "avada-tests".into(),
        }
    }

    pub(crate) fn requests(&self) -> Vec<(String, bool, Option<String>)> {
        self.state.lock().unwrap().requests.clone()
    }
}

type St = State<Arc<Mutex<FakeState>>>;

fn seen(state: &Mutex<FakeState>, path: String, headers: &HeaderMap) -> Option<Response> {
    let mut st = state.lock().unwrap();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("Bearer "))
        .unwrap_or(false);
    let inm = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    st.requests.push((path, auth, inm.clone()));
    if st.rate_limited {
        return Some((StatusCode::FORBIDDEN, "rate limited").into_response());
    }
    if inm.as_deref() == Some(st.etag.as_str()) && !st.etag.is_empty() {
        return Some(StatusCode::NOT_MODIFIED.into_response());
    }
    None
}

fn tagged(state: &Mutex<FakeState>, body: Response) -> Response {
    let etag = state.lock().unwrap().etag.clone();
    let mut r = body;
    if !etag.is_empty() {
        r.headers_mut().insert("etag", etag.parse().unwrap());
    }
    r
}

async fn search(
    State(state): St,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = q.get("q").cloned().unwrap_or_default();
    if let Some(r) = seen(&state, format!("/search/repositories?q={query}"), &headers) {
        return r;
    }
    let needle = query.replace("topic:avada-module", "").trim().to_string();
    let items: Vec<Value> = state
        .lock()
        .unwrap()
        .repos
        .iter()
        .filter(|r| {
            needle.is_empty()
                || r["full_name"].as_str().unwrap_or("").contains(&needle)
                || r["description"].as_str().unwrap_or("").contains(&needle)
        })
        .cloned()
        .collect();
    tagged(
        &state,
        axum::Json(json!({"total_count": items.len(), "items": items})).into_response(),
    )
}

async fn repo(
    State(state): St,
    AxPath((owner, repo)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = seen(&state, format!("/repos/{owner}/{repo}"), &headers) {
        return r;
    }
    let full = format!("{owner}/{repo}");
    let found = state
        .lock()
        .unwrap()
        .repos
        .iter()
        .find(|r| r["full_name"] == full)
        .cloned();
    match found {
        Some(v) => tagged(&state, axum::Json(v).into_response()),
        None => (StatusCode::NOT_FOUND, "{\"message\":\"Not Found\"}").into_response(),
    }
}

async fn tags(
    State(state): St,
    AxPath((owner, repo)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = seen(&state, format!("/repos/{owner}/{repo}/tags"), &headers) {
        return r;
    }
    let list = state
        .lock()
        .unwrap()
        .tags
        .get(&format!("{owner}/{repo}"))
        .cloned();
    match list {
        Some(list) => {
            let v: Vec<Value> = list
                .iter()
                .map(|(name, sha)| json!({"name": name, "commit": {"sha": sha}}))
                .collect();
            tagged(&state, axum::Json(v).into_response())
        }
        None => (StatusCode::NOT_FOUND, "{\"message\":\"Not Found\"}").into_response(),
    }
}

async fn contents(
    State(state): St,
    AxPath((owner, repo, path)): AxPath<(String, String, String)>,
    Query(q): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let reference = q.get("ref").cloned().unwrap_or_default();
    if let Some(r) = seen(
        &state,
        format!("/repos/{owner}/{repo}/contents/{path}?ref={reference}"),
        &headers,
    ) {
        return r;
    }
    let body = state
        .lock()
        .unwrap()
        .files
        .get(&format!("{owner}/{repo}/{path}"))
        .cloned();
    match body {
        Some(b) => tagged(&state, b.into_response()),
        None => (StatusCode::NOT_FOUND, "{\"message\":\"Not Found\"}").into_response(),
    }
}

fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.replace('+', " "))
    })
}

async fn device_code(State(state): St, body: String) -> Response {
    if form_field(&body, "client_id").as_deref() != Some("test-client") {
        return (StatusCode::NOT_FOUND, "bad client").into_response();
    }
    let code = format!("dc-{}", uuid::Uuid::new_v4());
    let uri = "https://github.com/login/device".to_string();
    state.lock().unwrap().issued_device_code = Some(code.clone());
    axum::Json(json!({
        "device_code": code,
        "user_code": "ABCD-1234",
        "verification_uri": uri,
        "expires_in": 900,
        "interval": 1,
    }))
    .into_response()
}

async fn access_token(State(state): St, body: String) -> Response {
    let code = form_field(&body, "device_code").unwrap_or_default();
    let mut st = state.lock().unwrap();
    st.polled_with.push(code.clone());
    if st.issued_device_code.as_deref() != Some(code.as_str()) {
        return axum::Json(json!({"error": "incorrect_device_code"})).into_response();
    }
    let v = match st.outcome {
        DeviceOutcome::Pending => json!({"error": "authorization_pending"}),
        DeviceOutcome::Approved => {
            json!({"access_token": FAKE_ACCESS_TOKEN, "token_type": "bearer", "scope": "read:user"})
        }
        DeviceOutcome::Denied => json!({"error": "access_denied"}),
        DeviceOutcome::Expired => json!({"error": "expired_token"}),
    };
    axum::Json(v).into_response()
}

// ---- git fixtures

/// The real `git` on this machine's `PATH`.
pub(crate) fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH for the marketplace tests")
}

fn git_in(dir: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new(real_git())
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Fixture repositories served as `file://<root>/<owner>/<repo>.git`.
pub(crate) struct Fixtures {
    pub root: PathBuf,
}

impl Fixtures {
    pub(crate) fn new(name: &str) -> Self {
        Fixtures {
            root: scratch(&format!("fx-{name}")),
        }
    }

    /// The `git_base` a marketplace clones from.
    pub(crate) fn git_base(&self) -> String {
        format!("file://{}", self.root.display())
    }

    /// A bare repository for `id` whose tag `tag` holds `manifest` as `avada.toml`.
    /// Returns the tagged commit.
    pub(crate) fn repo(&self, id: &str, tag: &str, manifest: &str) -> String {
        let work = self.root.join("work").join(id);
        let bare = self.root.join(format!("{id}.git"));
        let home = self.root.join("home");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        git_in(&work, &home, &["init", "-q"]);
        std::fs::write(work.join(fetch::MANIFEST_FILE), manifest).unwrap();
        std::fs::write(
            work.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        git_in(&work, &home, &["add", "."]);
        git_in(&work, &home, &["commit", "-q", "-m", "fixture"]);
        git_in(&work, &home, &["tag", tag]);
        let sha = git_in(&work, &home, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git_in(
            &self.root,
            &home,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        sha
    }
}

/// A manifest for `id` (`kind` is the `[distribution]` body; `extra` goes at the end).
pub(crate) fn manifest_for(id: &str, version: &str, distribution: &str, extra: &str) -> String {
    let name = id.split('/').nth(1).unwrap_or(id);
    format!(
        r#"capabilities = ["fs.read", "ui.rail", "process.spawn"]

[module]
id = "{id}"
name = "{name}"
version = "{version}"
description = "fixture module {id}"
publisher = "fixture"
contract = "^1"

[distribution]
{distribution}
{extra}
"#
    )
}

// ---- fake tools

/// What the fake cargo does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FakeCargo {
    /// Prints two `Compiling` lines and writes a stub binary where the pipeline looks.
    Builds,
    /// Prints an error and exits 101.
    Fails,
}

fn write_exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A directory with fake `rustup`, fake `cargo` and a wrapper around the real `git`.
/// `git_head_lies` makes `git rev-parse HEAD` answer forty zeros — the clone disagreeing
/// with `ls-remote`.
pub(crate) fn fake_tools(name: &str, cargo: FakeCargo, git_head_lies: bool) -> PathBuf {
    let dir = scratch(&format!("tools-{name}"));
    write_exec(&dir.join("rustup"), "#!/bin/sh\necho 'rustup 1.0 (fake)'\n");
    let cargo_text = match cargo {
        FakeCargo::Builds => {
            r#"#!/bin/sh
# fake cargo: pretend to build, drop a stub binary where the pipeline looks.
PATH=/usr/bin:/bin:$PATH
bin=""
while [ $# -gt 0 ]; do
  case "$1" in --bin) shift; bin="$1";; esac
  shift
done
if [ -z "$bin" ]; then bin=$(basename "$PWD" | sed 's/^.*__//'); fi
echo "   Compiling fixture-dep v0.1.0" >&2
echo "   Compiling $bin v1.0.0" >&2
mkdir -p "$CARGO_TARGET_DIR/release"
printf 'stub binary for %s\n' "$bin" > "$CARGO_TARGET_DIR/release/$bin"
chmod 755 "$CARGO_TARGET_DIR/release/$bin"
echo "    Finished release [optimized] target(s)" >&2
"#
        }
        FakeCargo::Fails => {
            "#!/bin/sh\necho 'error[E0425]: cannot find value `nope` in this scope' >&2\n\
             echo 'error: could not compile `fixture`' >&2\nexit 101\n"
        }
    };
    write_exec(&dir.join("cargo"), cargo_text);
    let real = real_git();
    let git_text = if git_head_lies {
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"rev-parse\" ]; then echo \
             0000000000000000000000000000000000000000; exit 0; fi\nexec {} \"$@\"\n",
            real.display()
        )
    } else {
        format!("#!/bin/sh\nexec {} \"$@\"\n", real.display())
    };
    write_exec(&dir.join("git"), &git_text);
    dir
}

// ---- the marketplace under test

/// Everything one test needs.
pub(crate) struct Rig {
    pub root: PathBuf,
    pub fixtures: Fixtures,
    pub github: FakeGitHub,
    pub tokens: Arc<MemoryTokenStore>,
    pub mp: Arc<Marketplace>,
}

/// Build a marketplace on a fresh root: fake GitHub with `state`, fixture repos, fake
/// tools on `PATH`. `ttl` is the GitHub cache TTL.
pub(crate) async fn rig(name: &str, state: FakeState, cargo: FakeCargo, ttl: Duration) -> Rig {
    rig_with(name, state, cargo, ttl, false).await
}

pub(crate) async fn rig_with(
    name: &str,
    state: FakeState,
    cargo: FakeCargo,
    ttl: Duration,
    git_head_lies: bool,
) -> Rig {
    let root = scratch(&format!("root-{name}"));
    let fixtures = Fixtures::new(name);
    let github = FakeGitHub::start(state).await;
    let tokens = Arc::new(MemoryTokenStore::new());
    let tools = fake_tools(name, cargo, git_head_lies);
    let mp = reopen(
        &root,
        &fixtures,
        &github,
        Arc::clone(&tokens),
        Some(tools),
        ttl,
    );
    Rig {
        root,
        fixtures,
        github,
        tokens,
        mp,
    }
}

/// A marketplace over an existing root (the "restart" in persistence tests).
pub(crate) fn reopen(
    root: &Path,
    fixtures: &Fixtures,
    github: &FakeGitHub,
    tokens: Arc<MemoryTokenStore>,
    tools: Option<PathBuf>,
    ttl: Duration,
) -> Arc<Marketplace> {
    let paths = InstallPaths::under(root);
    let store = InstallStore::open(paths, Arc::new(MemoryKeyStore::new())).unwrap();
    let api = HttpGitHub::new(
        github.config(ttl, "test-client"),
        Cache::new(state_dir_beside(root).join(CACHE_DIR)),
    );
    let tokens: Arc<dyn TokenStore> = tokens;
    Arc::new(Marketplace::new(
        store,
        Arc::new(api),
        tokens,
        MarketplaceOptions {
            git_base: fixtures.git_base(),
            path: tools.map(|t| t.into_os_string()),
        },
    ))
}

/// Poll until the job stops moving.
pub(crate) async fn wait(mp: &Marketplace, id: &str) -> Job {
    for _ in 0..1500 {
        let job = mp.job(id).unwrap();
        if job.phase.is_terminal() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("job {id} never finished: {:?}", mp.job(id));
}

pub(crate) const FILES: &str = "acme/avada-files";
pub(crate) const GIT: &str = "acme/avada-git";

pub(crate) fn files_state() -> FakeState {
    FakeState {
        repos: vec![
            repo_item(FILES, "Files module", 42),
            repo_item(GIT, "Git module", 7),
            repo_item("other/avada-junk", "junk", 0),
        ],
        etag: "\"etag-1\"".into(),
        ..Default::default()
    }
}

// ---- tests

#[tokio::test]
async fn install_from_source_records_pins_and_enables() {
    let r = rig(
        "install",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    let sha = r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let mut req = InstallRequest::new(FILES);
    req.workspace = Some("ws1".into());
    let job = r.mp.install(req).unwrap();
    assert_eq!(job.phase, Phase::Fetch);
    assert_eq!(job.module, FILES);
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Done, "{done:?}");
    assert_eq!(done.progress, Some(100));
    assert_eq!(done.tag.as_deref(), Some("v1.0.0"));
    assert_eq!(done.version.as_deref(), Some("1.0.0"));
    assert!(done.finished_at.is_some());
    assert!(
        done.log_tail.iter().any(|l| l.contains("Compiling")),
        "{:?}",
        done.log_tail
    );
    assert!(
        done.log_tail.iter().any(|l| l.contains(&sha)),
        "log names the commit"
    );

    // The store has the record, the lockfile pins tag + commit + sha.
    let id = ModuleId::new(FILES).unwrap();
    let installed = r.mp.store().record(&id).unwrap().expect("active record");
    let rights = installed.rights();
    assert_eq!(rights.tag, "v1.0.0");
    assert_eq!(rights.commit, sha);
    assert_eq!(rights.kind, InstallKind::Manual);
    assert_eq!(
        rights.repo,
        format!("{}/{FILES}.git", r.fixtures.git_base())
    );
    assert_eq!(
        rights.artifact_sha256,
        hash_file(&installed.binary).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&installed.binary).unwrap(),
        "stub binary for avada-files\n"
    );
    assert!(
        rights.accepted.contains(&Capability::FsRead)
            && rights.accepted.contains(&Capability::UiRail)
            && !rights.accepted.contains(&Capability::ProcessSpawn),
        "escape hatches are not granted by default: {:?}",
        rights.accepted
    );
    let lock = r.mp.store().lockfile().unwrap();
    let locked = lock.get(&id).expect("locked");
    assert_eq!(locked.tag, "v1.0.0");
    assert_eq!(locked.commit, sha);
    assert_eq!(locked.sha256, rights.artifact_sha256);
    assert_eq!(locked.kind, InstallKind::Manual);

    // installed() and show() agree, and the workspace was enabled.
    let list = r.mp.installed().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].module.as_deref(), Some(FILES));
    assert!(list[0].active);
    assert_eq!(list[0].enabled.get("ws1"), Some(&true));
    assert!(list[0].broken.is_none());
    assert!(
        !r.mp.state_dir().join(SCRATCH_DIR).join(&job.id).exists(),
        "scratch removed"
    );
    assert_eq!(r.mp.jobs().len(), 1);
}

#[tokio::test]
async fn a_failing_build_fails_the_job_with_the_tail() {
    let r = rig(
        "buildfail",
        files_state(),
        FakeCargo::Fails,
        Duration::from_secs(300),
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let job = r.mp.install(InstallRequest::new(FILES)).unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Failed);
    let err = done.error.unwrap();
    assert!(err.contains("cargo build") && err.contains("101"), "{err}");
    assert!(
        done.log_tail.iter().any(|l| l.contains("E0425")),
        "{:?}",
        done.log_tail
    );
    assert!(r.mp.installed().unwrap().is_empty());
    assert!(
        !r.mp.state_dir().join(SCRATCH_DIR).join(&job.id).exists(),
        "scratch removed on failure"
    );
}

#[tokio::test]
async fn accepted_set_and_explicit_tag_are_honoured() {
    let r = rig(
        "accepted",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let mut req = InstallRequest::new(FILES);
    req.tag = Some("v1.0.0".into());
    req.accepted = Some([Capability::FsRead].into_iter().collect());
    let job = r.mp.install(req).unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Done, "{done:?}");
    let id = ModuleId::new(FILES).unwrap();
    let rec = r.mp.store().record(&id).unwrap().unwrap();
    assert_eq!(
        rec.rights().accepted.iter().copied().collect::<Vec<_>>(),
        vec![Capability::FsRead]
    );
}

#[tokio::test]
async fn refusals_before_the_build() {
    let r = rig(
        "refuse",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    let sha = r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    r.fixtures.repo(
        "acme/avada-binary",
        "v1.0.0",
        &manifest_for("acme/avada-binary", "1.0.0", "kind = \"binary\"", ""),
    );
    r.fixtures.repo(
        "acme/avada-paid",
        "v1.0.0",
        &manifest_for(
            "acme/avada-paid",
            "1.0.0",
            "kind = \"source\"\ncommercial = true\nissuer = \"https://license.example\"",
            "",
        ),
    );
    r.fixtures.repo(
        "acme/avada-liar",
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    r.fixtures.repo(
        "acme/avada-offbyone",
        "v1.0.0",
        &manifest_for("acme/avada-offbyone", "1.0.1", "kind = \"source\"", ""),
    );

    let run = |req: InstallRequest| {
        let mp = Arc::clone(&r.mp);
        async move {
            let job = mp.install(req).unwrap();
            wait(&mp, &job.id).await
        }
    };

    // Unknown tag.
    let mut req = InstallRequest::new(FILES);
    req.tag = Some("v9.9.9".into());
    let job = run(req).await;
    assert_eq!(job.phase, Phase::Failed);
    assert!(
        job.error.as_deref().unwrap().contains("no tag `v9.9.9`"),
        "{job:?}"
    );

    // Tag names a different commit than the caller expected.
    let mut req = InstallRequest::new(FILES);
    req.expected_commit = Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into());
    let job = run(req).await;
    assert_eq!(job.phase, Phase::Failed);
    let e = job.error.unwrap();
    assert!(e.contains("names commit") && e.contains("deadbeef"), "{e}");

    // A correct short prefix passes the same check (and the whole install).
    let mut req = InstallRequest::new(FILES);
    req.expected_commit = Some(sha[..12].to_uppercase());
    let job = run(req).await;
    assert_eq!(job.phase, Phase::Done, "{job:?}");

    // Binary distribution.
    let job = run(InstallRequest::new("acme/avada-binary")).await;
    assert_eq!(job.phase, Phase::Failed);
    assert!(
        job.error.as_deref().unwrap().contains("prebuilt binary"),
        "{job:?}"
    );

    // Commercial.
    let job = run(InstallRequest::new("acme/avada-paid")).await;
    assert_eq!(job.phase, Phase::Failed);
    assert!(
        job.error.as_deref().unwrap().contains("commercial"),
        "{job:?}"
    );

    // Manifest claims another id.
    let job = run(InstallRequest::new("acme/avada-liar")).await;
    assert_eq!(job.phase, Phase::Failed);
    assert!(
        job.error
            .as_deref()
            .unwrap()
            .contains("says it is acme/avada-files"),
        "{job:?}"
    );

    // Tag and manifest version disagree.
    let job = run(InstallRequest::new("acme/avada-offbyone")).await;
    assert_eq!(job.phase, Phase::Failed);
    assert!(
        job.error
            .as_deref()
            .unwrap()
            .contains("does not match the manifest version"),
        "{job:?}"
    );

    // No tags at all.
    let job = run(InstallRequest::new("acme/avada-missing")).await;
    assert_eq!(job.phase, Phase::Failed);

    // Only the one successful install exists.
    let list = r.mp.installed().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].module.as_deref(), Some(FILES));
}

#[tokio::test]
async fn a_clone_that_disagrees_with_ls_remote_is_refused() {
    let r = rig_with(
        "headlies",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
        true,
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let job = r.mp.install(InstallRequest::new(FILES)).unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Failed);
    let e = done.error.unwrap();
    assert!(e.contains("clone of v1.0.0 is at 0000000"), "{e}");
    assert!(r.mp.installed().unwrap().is_empty());
}

#[tokio::test]
async fn bad_ids_workspaces_and_a_missing_toolchain_are_refused_up_front() {
    let r = rig(
        "upfront",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    let e = r.mp.install(InstallRequest::new("not-an-id")).unwrap_err();
    assert!(matches!(e, MarketplaceError::BadModuleId(_)), "{e}");
    assert_eq!(e.http_status(), 400);
    let mut req = InstallRequest::new(FILES);
    req.workspace = Some("../etc".into());
    let e = r.mp.install(req).unwrap_err();
    assert!(matches!(e, MarketplaceError::BadWorkspace(_)), "{e}");
    assert_eq!(e.http_status(), 400);
    assert!(r.mp.jobs().is_empty(), "nothing was started");

    // No tools on PATH: the guide comes back instead of a job.
    let bare = reopen(
        &r.root,
        &r.fixtures,
        &r.github,
        Arc::clone(&r.tokens),
        Some(scratch("empty-path")),
        Duration::from_secs(300),
    );
    assert!(!bare.toolchain().ready());
    let e = bare.install(InstallRequest::new(FILES)).unwrap_err();
    assert_eq!(e.http_status(), 412);
    match e {
        MarketplaceError::Toolchain(guide) => assert!(guide.contains("rustup"), "{guide}"),
        other => panic!("{other}"),
    }
    assert!(bare.jobs().is_empty());
    assert!(r.mp.toolchain().ready());
}

#[tokio::test]
async fn dependencies_are_installed_first_as_dependencies() {
    let r = rig(
        "deps",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.2.0",
        &manifest_for(
            FILES,
            "1.2.0",
            "kind = \"source\"",
            "[[provides]]\nshape = \"avada.files.tree\"\nversion = \"1.2.0\"\n",
        ),
    );
    r.fixtures.repo(
        GIT,
        "v1.0.0",
        &manifest_for(
            GIT,
            "1.0.0",
            "kind = \"source\"",
            "[[requires]]\nshape = \"avada.files.tree\"\nversion = \"^1\"\nprovider = \"acme/avada-files\"\n",
        ),
    );
    r.fixtures.repo(
        "acme/avada-orphan",
        "v1.0.0",
        &manifest_for(
            "acme/avada-orphan",
            "1.0.0",
            "kind = \"source\"",
            "[[requires]]\nshape = \"avada.nothing\"\nversion = \"^1\"\n",
        ),
    );
    let job = r.mp.install(InstallRequest::new(GIT)).unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Done, "{done:?}");
    assert_eq!(
        done.tag.as_deref(),
        Some("v1.0.0"),
        "the job's tag is the requested module's"
    );
    assert!(
        done.log_tail
            .iter()
            .any(|l| l.contains("installing acme/avada-files as a dependency")),
        "{:?}",
        done.log_tail
    );
    let list = r.mp.installed().unwrap();
    assert_eq!(list.len(), 2);
    let files = list
        .iter()
        .find(|i| i.module.as_deref() == Some(FILES))
        .unwrap();
    assert_eq!(files.kind, Some(InstallKind::Dependency));
    assert_eq!(files.tag.as_deref(), Some("v1.2.0"));
    let git = list
        .iter()
        .find(|i| i.module.as_deref() == Some(GIT))
        .unwrap();
    assert_eq!(git.kind, Some(InstallKind::Manual));

    // Second install of a dependent: the provider is already there, nothing reinstalls.
    let before = r.mp.jobs().len();
    let job = r.mp.install(InstallRequest::new(GIT)).unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Done, "{done:?}");
    assert!(done
        .log_tail
        .iter()
        .any(|l| l.contains("avada.files.tree ^1: provided")));
    assert_eq!(r.mp.jobs().len(), before + 1);

    // A requirement nobody provides and no provider is named: refused.
    let job =
        r.mp.install(InstallRequest::new("acme/avada-orphan"))
            .unwrap();
    let done = wait(&r.mp, &job.id).await;
    assert_eq!(done.phase, Phase::Failed);
    assert!(
        done.error
            .as_deref()
            .unwrap()
            .contains("no provider is named"),
        "{done:?}"
    );
}

#[tokio::test]
async fn enable_disable_uninstall_persist_across_reopen() {
    let r = rig(
        "state",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    r.fixtures.repo(
        FILES,
        "v1.0.0",
        &manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );
    let e = r.mp.set_enabled("ws1", FILES, true).unwrap_err();
    assert!(matches!(e, MarketplaceError::NotInstalled(_)), "{e}");
    assert_eq!(e.http_status(), 404);
    let job = r.mp.install(InstallRequest::new(FILES)).unwrap();
    assert_eq!(wait(&r.mp, &job.id).await.phase, Phase::Done);
    let m = r.mp.set_enabled("ws1", FILES, true).unwrap();
    assert_eq!(m.get("ws1"), Some(&true));
    r.mp.set_enabled("ws2", FILES, false).unwrap();
    assert!(matches!(
        r.mp.set_enabled("bad key", FILES, true).unwrap_err(),
        MarketplaceError::BadWorkspace(_)
    ));

    let again = reopen(
        &r.root,
        &r.fixtures,
        &r.github,
        Arc::clone(&r.tokens),
        None,
        Duration::from_secs(300),
    );
    let list = again.installed().unwrap();
    assert_eq!(list[0].enabled.get("ws1"), Some(&true));
    assert_eq!(list[0].enabled.get("ws2"), Some(&false));
    let file =
        r.mp.state_dir()
            .join(workspace::WORKSPACES_DIR)
            .join("ws1")
            .join(workspace::STATE_FILE);
    assert!(file.is_file(), "{}", file.display());

    let e = again.uninstall(FILES, "9.9.9").unwrap_err();
    assert_eq!(e.http_status(), 404);
    let e = again.uninstall(FILES, "not-a-version").unwrap_err();
    assert_eq!(e.http_status(), 404);
    again.uninstall(FILES, "1.0.0").unwrap();
    assert!(again.installed().unwrap().is_empty());
    assert!(again
        .store()
        .lockfile()
        .unwrap()
        .get(&ModuleId::new(FILES).unwrap())
        .is_none());
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        !text.contains(FILES),
        "workspace state forgets an uninstalled module: {text}"
    );
}

#[tokio::test]
async fn search_and_show_use_the_cache_and_the_token() {
    let r = rig(
        "cache",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    r.github.state.lock().unwrap().tags.insert(
        FILES.into(),
        vec![
            ("v1.0.0".into(), "a".repeat(40)),
            ("v0.9.0".into(), "b".repeat(40)),
        ],
    );
    r.github.state.lock().unwrap().files.insert(
        format!("{FILES}/avada.toml"),
        manifest_for(FILES, "1.0.0", "kind = \"source\"", ""),
    );

    let hits = r.mp.search("files").await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].full_name, FILES);
    assert_eq!(hits[0].stars, 42);
    let all = r.mp.search("").await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(r.github.requests().len(), 2);
    assert!(!r.github.requests()[0].1, "anonymous by default");

    // Fresh: served from disk, no request.
    let again = r.mp.search("files").await.unwrap();
    assert_eq!(again, hits);
    assert_eq!(r.github.requests().len(), 2);
    assert!(r.mp.cache_dir().is_dir());

    // Stale (TTL 0): revalidated with If-None-Match, answered 304, same body.
    let stale = reopen(
        &r.root,
        &r.fixtures,
        &r.github,
        Arc::clone(&r.tokens),
        None,
        Duration::ZERO,
    );
    let again = stale.search("files").await.unwrap();
    assert_eq!(again, hits);
    let reqs = r.github.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[2].2.as_deref(), Some("\"etag-1\""));

    // Content changed upstream: new ETag, full answer.
    {
        let mut st = r.github.state.lock().unwrap();
        st.etag = "\"etag-2\"".into();
        st.repos[0]["stargazers_count"] = json!(43);
    }
    let fresh = stale.search("files").await.unwrap();
    assert_eq!(fresh[0].stars, 43);
    assert_eq!(r.github.requests().len(), 4);

    // Rate-limited with a stale copy: the copy is served; without one: an error.
    r.github.state.lock().unwrap().rate_limited = true;
    assert_eq!(stale.search("files").await.unwrap()[0].stars, 43);
    let e = stale.search("nothing-cached").await.unwrap_err();
    assert_eq!(e.http_status(), 502);
    assert!(e.to_string().contains("rate"), "{e}");
    r.github.state.lock().unwrap().rate_limited = false;

    // show() assembles repo + tags + manifest + local state.
    let view = r.mp.show("acme", "avada-files").await.unwrap();
    assert_eq!(view.repo.as_ref().unwrap().full_name, FILES);
    assert_eq!(view.tags.len(), 2);
    assert_eq!(view.newest_tag.as_deref(), Some("v1.0.0"));
    let m = view.manifest.expect("manifest");
    assert_eq!(m.module.version.to_string(), "1.0.0");
    assert!(view.manifest_error.is_none());
    assert!(view.installed.is_empty());
    assert!(view.enabled.is_empty());
    let reqs = r.github.requests();
    assert!(
        reqs.iter()
            .any(|q| q.0.contains("/contents/avada.toml?ref=v1.0.0")),
        "{reqs:?}"
    );
    let unknown = r.mp.show("acme", "avada-nope").await.unwrap();
    assert!(unknown.repo.is_none() && unknown.manifest.is_none() && unknown.tags.is_empty());
    let e = r.mp.show("acme", "bad id").await.unwrap_err();
    assert_eq!(e.http_status(), 400);

    // With a stored token, requests carry it — split cache key, so a new request.
    r.tokens.set(&Token::new(FAKE_STORED_TOKEN)).unwrap();
    assert!(r.mp.signed_in());
    let before = r.github.requests().len();
    r.mp.search("files").await.unwrap();
    let reqs = r.github.requests();
    assert_eq!(reqs.len(), before + 1);
    assert!(reqs.last().unwrap().1, "the token was sent");
    // And the cache on disk never holds the token's value.
    for entry in std::fs::read_dir(r.mp.cache_dir()).unwrap() {
        let text = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&text).contains(FAKE_STORED_TOKEN));
    }
    r.mp.sign_out().unwrap();
    assert!(!r.mp.signed_in());
}

#[tokio::test]
async fn device_flow_sign_in_stores_the_token_and_never_shows_it() {
    let r = rig(
        "signin",
        files_state(),
        FakeCargo::Builds,
        Duration::from_secs(300),
    )
    .await;
    let view = r.mp.signin_start().await.unwrap();
    assert_eq!(view.user_code, "ABCD-1234");
    assert_eq!(view.status, SignInStatus::Pending);
    assert_eq!(view.interval, 1);
    let issued = r
        .github
        .state
        .lock()
        .unwrap()
        .issued_device_code
        .clone()
        .unwrap();
    let json = serde_json::to_string(&view).unwrap();
    assert!(
        !json.contains(&issued),
        "the device code never leaves the process"
    );

    let p = r.mp.signin_poll(&view.id).await.unwrap();
    assert_eq!(p.status, SignInStatus::Pending);
    assert!(!r.mp.signed_in());
    assert_eq!(
        r.github.state.lock().unwrap().polled_with,
        vec![issued.clone()]
    );

    r.github.state.lock().unwrap().outcome = DeviceOutcome::Approved;
    let p = r.mp.signin_poll(&view.id).await.unwrap();
    assert_eq!(p.status, SignInStatus::Done);
    assert!(r.mp.signed_in());
    let stored = r.tokens.get().unwrap().unwrap();
    assert_eq!(stored.expose_secret(), FAKE_ACCESS_TOKEN);
    let json = serde_json::to_string(&p).unwrap();
    assert!(!json.contains(FAKE_ACCESS_TOKEN) && !json.contains(&issued));

    // Done stays done without another round trip.
    let polls = r.github.state.lock().unwrap().polled_with.len();
    assert_eq!(
        r.mp.signin_poll(&view.id).await.unwrap().status,
        SignInStatus::Done
    );
    assert_eq!(r.github.state.lock().unwrap().polled_with.len(), polls);

    // Denied and expired end the sign-in too.
    for (outcome, status) in [
        (DeviceOutcome::Denied, SignInStatus::Denied),
        (DeviceOutcome::Expired, SignInStatus::Expired),
    ] {
        let v = r.mp.signin_start().await.unwrap();
        r.github.state.lock().unwrap().outcome = outcome;
        assert_eq!(r.mp.signin_poll(&v.id).await.unwrap().status, status);
    }
    let e = r.mp.signin_poll("nope").await.unwrap_err();
    assert_eq!(e.http_status(), 404);

    // No client id: sign-in is unavailable, not broken.
    let paths = InstallPaths::under(&r.root);
    let store = InstallStore::open(paths, Arc::new(MemoryKeyStore::new())).unwrap();
    let api = HttpGitHub::new(
        r.github.config(Duration::ZERO, ""),
        Cache::new(state_dir_beside(&r.root).join(CACHE_DIR)),
    );
    let tokens: Arc<dyn TokenStore> = Arc::new(MemoryTokenStore::new());
    let no_client = Marketplace::new(store, Arc::new(api), tokens, MarketplaceOptions::default());
    let e = no_client.signin_start().await.unwrap_err();
    assert_eq!(e.http_status(), 503);
    assert!(matches!(e, MarketplaceError::SignInUnavailable(_)), "{e}");
}

#[test]
fn errors_map_to_statuses_and_read_well() {
    use MarketplaceError as E;
    let cases: Vec<(E, u16, &str)> = vec![
        (E::BadModuleId("x".into()), 400, "owner/repo"),
        (E::BadWorkspace("x".into()), 400, "workspace"),
        (E::Refused("r".into()), 409, "refused: r"),
        (E::NotInstalled("m".into()), 404, "not installed"),
        (E::NoSuchJob("j".into()), 404, "no job"),
        (E::NoSuchSignIn("s".into()), 404, "no sign-in"),
        (E::SignInUnavailable("s".into()), 503, "unavailable"),
        (E::Toolchain("guide".into()), 412, "guide"),
        (E::GitHub(GitHubError::RateLimited), 502, "rate"),
        (E::Io("io".into()), 500, "io"),
        (E::Git("g".into()), 500, "g"),
        (E::Build("b".into()), 500, "build: b"),
    ];
    for (e, status, text) in cases {
        assert_eq!(e.http_status(), status, "{e}");
        assert!(e.to_string().contains(text), "{e}");
    }
    assert!(commit_matches("ABCDEF1", "abcdef1234567890"));
    assert!(
        !commit_matches("abcdef", "abcdef1234567890"),
        "too short to trust"
    );
    assert!(!commit_matches("0000000", "abcdef1234567890"));
}

/// Real GitHub, real network: `cargo test --offline -p avada-core -- --ignored
/// real_github`. Read-only and unauthenticated; it only proves the wire format still
/// matches the fake.
#[tokio::test]
#[ignore = "hits api.github.com"]
async fn real_github_search_and_tags() {
    let root = scratch("real");
    let api = HttpGitHub::new(
        GitHubConfig::default(),
        Cache::new(state_dir_beside(&root).join(CACHE_DIR)),
    );
    let hits = api.search("", None).await.unwrap();
    assert!(hits.iter().all(|h| h.full_name.contains('/')));
    let tags = api.tags("rust-lang", "cargo", None).await.unwrap();
    assert!(!tags.is_empty());
    assert!(tags.iter().all(|t| t.commit.len() == 40));
}
