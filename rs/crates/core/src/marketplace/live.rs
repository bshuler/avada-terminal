//! The Wave 2 exit proof, as a test rather than a script.
//!
//! Every other marketplace test runs against the fakes in [`super::testing`]: a GitHub
//! on `127.0.0.1`, fixture repositories served over `file://`, a `cargo` that is a
//! shell script. That is the right default — the suite has to be fast, offline and
//! deterministic. But it proves the plumbing, not the claim. The claim Wave 2 exits on
//! is that a real build discovers a real module on real GitHub *by its topic*, clones
//! it, compiles it with a real toolchain, and turns it on and off in a workspace.
//!
//! So this module is `#[ignore]`d. It reaches the network, shells out to `cargo
//! build --release`, and takes minutes. Run it deliberately:
//!
//! ```text
//! cargo test -p avada-core --lib marketplace::live -- --ignored --nocapture
//! ```
//!
//! `bshuler/avada-files` is a private repository, so both halves need credentials, and
//! they come from two different places because GitHub uses two different protocols
//! here. The REST half (search, tags, manifest) needs a bearer token: the test asks
//! `gh auth token` for one and hands it straight to a [`FileTokenStore`], which writes
//! it 0600 under the scratch root. The value is never printed, never passed as an
//! argument and never lands in the repository. The git half (`clone --depth 1`) is
//! plain `git`, so it uses whatever credential helper the machine already has — the
//! same one a user's own `git clone` would.
//!
//! What this does *not* cover is the panel: that the rail entry appears, that a click
//! opens a pane, that revealing a path scrolls to it, and that disabling the module
//! leaves the placeholder behind. Those are UI facts and they are proven against the
//! real component tree in the app crate's `uitest::files` and `uitest::placeholder`.
//! This test proves the half that needs the network, and stops there.

use super::github::GitHubConfig;
use super::job::Phase;
use super::testing::scratch;
use super::token::{FileTokenStore, Token, TokenStore};
use super::{InstallRequest, Marketplace};
use crate::install::InstallPaths;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The module Wave 2 was ordered around: the file explorer, after it left the app.
const MODULE: &str = "bshuler/avada-files";
/// Pinned rather than "newest", so the proof does not change meaning when the module
/// ships 0.2.0.
const TAG: &str = "v0.1.0";
/// The workspace the install enables it in.
const WORKSPACE: &str = "live-proof";
/// A `cargo build --release` of a fresh dependency graph is minutes, not seconds.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The GitHub token `gh` is already signed in with, or `None`.
///
/// Captured in-process: `gh` writes it to a pipe this function owns, and the only
/// place it goes from there is [`FileTokenStore::set`]. It is deliberately not
/// returned as a `String` — [`Token`] redacts its own `Debug` and wipes its bytes on
/// drop, so an accidental `dbg!` or a panic message cannot leak it.
fn gh_token() -> Option<Token> {
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Token::new(trimmed))
    }
}

/// A marketplace rooted in a fresh scratch directory, with the token already stored.
///
/// The root is `<scratch>/modules` and not `<scratch>` itself, because
/// [`state_dir_beside`](super::state_dir_beside) names a *sibling* of the modules
/// root. Rooting at the scratch directory directly would put the cache in
/// `$TMPDIR/marketplace`, shared with every other test on the machine.
fn live_marketplace() -> Option<(Arc<Marketplace>, PathBuf)> {
    let token = gh_token()?;
    let root = scratch("live").join("modules");
    std::fs::create_dir_all(&root).unwrap();
    FileTokenStore::new(&InstallPaths::under(&root).keys_dir())
        .set(&token)
        .unwrap();
    let mp = Marketplace::open_under(&root, GitHubConfig::default()).unwrap();
    Some((Arc::new(mp), root))
}

#[tokio::test]
#[ignore = "reaches github.com and runs a real cargo build; see the module docs"]
async fn a_real_module_is_found_by_topic_built_from_source_and_switched_on_and_off() {
    let Some((mp, root)) = live_marketplace() else {
        panic!("`gh auth token` produced nothing — sign in with `gh auth login` first");
    };

    // ---- discovery. The whole point of the topic: nothing here names the repository
    // to GitHub, the search asks for `topic:avada-module` and the module has to be in
    // the answer on its own merits.
    let found = mp.search("").await.expect("topic search failed");
    assert!(
        found.iter().any(|r| r.full_name == MODULE),
        "{MODULE} is not in the `avada-module` topic search; got {:?}",
        found.iter().map(|r| &r.full_name).collect::<Vec<_>>()
    );

    // ---- the detail view the marketplace UI draws, including the manifest read out
    // of the repository at the newest tag.
    let (owner, repo) = MODULE.split_once('/').unwrap();
    let view = mp.show(owner, repo).await.expect("show failed");
    assert!(
        view.manifest_error.is_none(),
        "the manifest at the newest tag did not parse: {:?}",
        view.manifest_error
    );
    let manifest = view.manifest.expect("no avada.toml at the newest tag");
    assert_eq!(manifest.module.id.as_str(), MODULE);
    assert!(
        view.tags.iter().any(|t| t.name == TAG),
        "{TAG} is not among the tags GitHub lists"
    );
    assert!(
        view.installed.is_empty(),
        "a fresh root should have nothing installed in it"
    );

    // ---- install. This clones the tag, checks the commit, resolves requirements and
    // runs `cargo build --release --locked` on a real toolchain.
    assert!(
        mp.toolchain().guide().is_none(),
        "no usable toolchain: {:?}",
        mp.toolchain().guide()
    );
    let mut req = InstallRequest::new(MODULE);
    req.tag = Some(TAG.to_string());
    req.workspace = Some(WORKSPACE.to_string());
    let started = mp.install(req).expect("install refused");

    let began = Instant::now();
    let mut seen: Vec<Phase> = Vec::new();
    let job = loop {
        let job = mp.job(&started.id).expect("the job vanished mid-install");
        // Printed, not merely asserted on: this test exists to be watched once in a
        // while, and "which phase took the minutes" is the thing a human wants from it.
        if seen.last() != Some(&job.phase) {
            println!("[{:>6.1}s] {:?}", began.elapsed().as_secs_f32(), job.phase);
            seen.push(job.phase);
        }
        if job.phase.is_terminal() {
            break job;
        }
        assert!(
            began.elapsed() < INSTALL_TIMEOUT,
            "install stuck in {:?} after {:?}; last log: {:?}",
            job.phase,
            began.elapsed(),
            job.log_tail.last()
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(
        job.phase,
        Phase::Done,
        "install failed: {}\nlog tail:\n{}",
        job.error.unwrap_or_default(),
        job.log_tail.join("\n")
    );

    // ---- the binary the host will actually run has to exist on disk, signed into the
    // install record. `installed()` re-verifies every record, so a `broken` entry here
    // means the signature or the hash did not match what was written.
    assert!(
        seen.contains(&Phase::Build),
        "the poll never saw a Build phase, so nothing here proves a compile happened: {seen:?}"
    );
    println!("{}", job.log_tail.join("\n"));

    let installed = mp.installed().expect("installed() failed");
    let entry = installed
        .iter()
        .find(|i| i.module.as_deref() == Some(MODULE))
        .expect("the module is not in `installed()` after a Done job");
    assert!(
        entry.broken.is_none(),
        "install record is broken: {:?}",
        entry.broken
    );
    assert!(
        entry.active,
        "the version just installed should be the active one"
    );
    assert_eq!(entry.tag.as_deref(), Some(TAG));

    // ---- enabling and disabling. `install` was asked to enable it in the workspace,
    // so it should already be on; the app reads exactly this map to decide whether the
    // rail entry is drawn or the pane falls back to the placeholder.
    assert_eq!(
        entry.enabled.get(WORKSPACE),
        Some(&true),
        "install was given a workspace and should have enabled it there"
    );
    let off = mp
        .set_enabled(WORKSPACE, MODULE, false)
        .expect("disable failed");
    assert_eq!(off.get(WORKSPACE), Some(&false));
    let on = mp
        .set_enabled(WORKSPACE, MODULE, true)
        .expect("re-enable failed");
    assert_eq!(on.get(WORKSPACE), Some(&true));

    // ---- and it uninstalls cleanly, so a machine that ran this proof is not left
    // carrying a build of it.
    // The artifact the host would actually exec, not just a record claiming one.
    let version = entry.version.clone().expect("no version on the record");
    let id = crate::rights::ModuleId::new(MODULE).unwrap();
    let bin = InstallPaths::under(&root).binary_path(&id, &version.parse().unwrap(), &manifest);
    let meta =
        std::fs::metadata(&bin).unwrap_or_else(|e| panic!("no binary at {}: {e}", bin.display()));
    println!("built {} ({} bytes)", bin.display(), meta.len());
    assert!(
        meta.len() > 100_000,
        "{} is too small to be a real release binary",
        bin.display()
    );

    mp.uninstall(MODULE, &version).expect("uninstall failed");
    assert!(
        mp.installed()
            .unwrap()
            .iter()
            .all(|i| i.module.as_deref() != Some(MODULE)),
        "uninstall left the record behind"
    );
    let _ = std::fs::remove_dir_all(root.parent().unwrap_or(&root));
}
