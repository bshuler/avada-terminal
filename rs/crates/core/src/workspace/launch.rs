//! Port of `resolveLaunchWorkspace` / `getInitialWorkspace` / `getInitialWindows`
//! from `src/main/workspace.ts` — resolve the workspace to open on launch.
//!
//! Precedence (mirrors the TS): inline `-c` flags win, then an explicit positional
//! `.json`, then the last session (`last-workspace.json`); inline workspaces have
//! their relative cwds resolved against the launch directory. `get_initial_windows`
//! normalises the result through `windows_of` (last-session restore included).

use crate::cli::parse::parse_cli;
use crate::persistence::paths;
use crate::workspace::io;
use crate::workspace::model::{WindowSpec, WorkspaceFile};
use crate::workspace::project::find_project_root;
use avada_module_sdk::contract::WorkspaceInfo;
use std::path::{Path, PathBuf};

/// What to load on launch, parameterized by `argv` + `cwd` (so it also serves the
/// `second-instance` event). Relative cwds resolve against `cwd`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn resolve_launch_workspace(argv: &[String], cwd: &str) -> Option<WorkspaceFile> {
    resolve_launch_workspace_with(argv, cwd, &paths::last_workspace_json())
}

/// Resolve ONLY an explicitly-requested launch workspace from `argv` — an inline `-c …` flag
/// set or a positional `.json` — WITHOUT the last-session fallback. The headless core
/// bootstrap uses this (argv-only); the native GUI uses [`resolve_launch_workspace`] so a
/// plain relaunch restores the last session (#14 — the GUI writes `last-workspace.json`
/// when its final window closes). Relative cwds resolve against `cwd`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn resolve_cli_workspace(argv: &[String], cwd: &str) -> Option<WorkspaceFile> {
    let parsed = parse_cli(argv);
    if let Some(ws) = parsed.workspace {
        return Some(io::resolve_cwds(&ws, cwd));
    }
    if let Some(json_path) = parsed.json_path {
        return io::read_workspace(json_path);
    }
    None
}

/// The marketplace workspace key of the workspace this launch names, if any.
///
/// The app starts installed modules before its first window exists, so it has to know
/// *which* workspace's enable/disable choices apply from `argv` alone. Only an explicit
/// positional file names a workspace: an inline `-c …` launch has no file, and the
/// last-session restore is a snapshot, not a workspace the human enabled a module in.
/// The key is the file stem (`dev.avada` → `dev`), the same identity the persistence
/// lockfile and the workspace library already use; a stem the marketplace would refuse
/// as a key (see [`crate::marketplace::workspace::valid_key`]) cannot have any state
/// stored under it, so it resolves to `None` rather than a name nothing can match.
pub fn launch_workspace_key(argv: &[String]) -> Option<String> {
    let json_path = parse_cli(argv).json_path?;
    let key = crate::persistence::lockfile::workspace_key(Path::new(&json_path));
    crate::marketplace::workspace::valid_key(&key).then_some(key)
}

/// The workspace as modules are told about it for this launch.
///
/// A module learns one workspace: `hello.workspace` on start and the `module.activate`
/// argument after it, and every `host.fs.*` call is scoped to that workspace's `root`.
/// So the root decides what a file browser shows, and "none" shows nothing. It is the
/// project root (a `.avada/` or `.git` marker, see [`find_project_root`]) above the
/// first pane the launch describes, or that pane's directory when nothing above it marks
/// a project; a launch whose panes name no directory uses `cwd`, the directory those
/// panes will start in. `id` is the launch file's marketplace key (see
/// [`launch_workspace_key`]) and `default` when the launch names no file, so a module's
/// per-workspace data lines up with the marketplace's per-workspace choices; `name` is
/// the file's own `name`, then the key, then the root directory's name.
///
/// `file` is the resolved launch workspace (see [`resolve_launch_workspace`]) with its
/// relative cwds already made absolute; passing it in keeps the file from being read a
/// second time and keeps this decision in step with what actually gets seeded.
pub fn module_workspace(argv: &[String], cwd: &str, file: Option<&WorkspaceFile>) -> WorkspaceInfo {
    let key = launch_workspace_key(argv);
    let start = first_pane_cwd(file).unwrap_or_else(|| cwd.to_string());
    let root = find_project_root(&start)
        .map(|r| r.dir)
        .unwrap_or_else(|| PathBuf::from(&start));
    let name = file
        .and_then(|f| f.name.clone())
        .or_else(|| key.clone())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "default".to_string());
    WorkspaceInfo {
        id: key.unwrap_or_else(|| "default".to_string()),
        name,
        root: Some(root.to_string_lossy().into_owned()),
    }
}

/// The directory of the first pane the launch describes, in the order the seed opens
/// them (`windows` → `groups` → `panes`, the [`io::windows_of`] precedence).
fn first_pane_cwd(file: Option<&WorkspaceFile>) -> Option<String> {
    io::windows_of(file)
        .iter()
        .flat_map(|w| w.groups.iter())
        .flat_map(|g| g.panes.iter())
        .find_map(|p| p.cwd.clone())
}

/// The launch resolution with the last-session path injected (for testability). Inline / explicit
/// `.json` (via [`resolve_cli_workspace`]) win; otherwise fall back to the last session.
#[tracing::instrument(level = "debug", skip_all)]
fn resolve_launch_workspace_with(
    argv: &[String],
    cwd: &str,
    last_path: &Path,
) -> Option<WorkspaceFile> {
    if let Some(ws) = resolve_cli_workspace(argv, cwd) {
        return Some(ws);
    }
    if last_path.exists() {
        io::read_workspace(last_path)
    } else {
        None
    }
}

/// What to load on launch from this process's own argv + cwd.
#[tracing::instrument(level = "debug", skip_all)]
pub fn get_initial_workspace() -> Option<WorkspaceFile> {
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    resolve_launch_workspace(&argv, &cwd)
}

/// The window list to open on first launch (last-session restore included).
#[tracing::instrument(level = "debug", ret)]
pub fn get_initial_windows() -> Vec<WindowSpec> {
    io::windows_of(get_initial_workspace().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["/path/to/avada".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hp-launch-{}-{tag}.json", std::process::id()))
    }

    /// A scratch directory that no `.git` or `.avada` sits above, so the walk finds
    /// nothing unless the test plants a marker.
    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-modws-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn file_with_cwd(cwd: &str) -> WorkspaceFile {
        WorkspaceFile {
            name: Some("Dev".to_string()),
            panes: Some(vec![crate::workspace::model::PaneSpec {
                cwd: Some(cwd.to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn module_workspace_of_a_bare_launch_is_the_cwd_itself() {
        let d = scratch_dir("bare");
        let ws = module_workspace(&argv(&[]), d.to_str().unwrap(), None);
        assert_eq!(ws.id, "default");
        assert_eq!(ws.root.as_deref(), Some(d.to_str().unwrap()));
        // No file, no key: the directory's own name is what the module can show.
        assert_eq!(ws.name, d.file_name().unwrap().to_string_lossy());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn module_workspace_root_is_the_project_root_above_the_first_pane() {
        let d = scratch_dir("project");
        // A linked worktree's `.git` is a file; that must count as a marker too.
        std::fs::write(d.join(".git"), b"gitdir: elsewhere\n").unwrap();
        let deep = d.join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();
        let ws = module_workspace(
            &argv(&[]),
            "/nowhere",
            Some(&file_with_cwd(deep.to_str().unwrap())),
        );
        assert_eq!(ws.root.as_deref(), Some(d.to_str().unwrap()));
        assert_eq!(ws.name, "Dev", "the file's own name wins");
        assert_eq!(ws.id, "default", "no launch file, so no marketplace key");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn module_workspace_without_a_marker_is_the_pane_directory() {
        let d = scratch_dir("plain");
        let ws = module_workspace(
            &argv(&[]),
            "/nowhere",
            Some(&file_with_cwd(d.to_str().unwrap())),
        );
        assert_eq!(ws.root.as_deref(), Some(d.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn module_workspace_id_is_the_launch_files_key() {
        let d = scratch_dir("keyed");
        let file = d.join("dev.avada");
        std::fs::write(&file, br#"{"panes":[{"command":"cat"}]}"#).unwrap();
        // The pane names no directory, so the root is the launch cwd, not the file's dir.
        let ws = module_workspace(
            &argv(&[file.to_str().unwrap()]),
            d.to_str().unwrap(),
            Some(&WorkspaceFile {
                panes: Some(vec![Default::default()]),
                ..Default::default()
            }),
        );
        assert_eq!(ws.id, "dev");
        assert_eq!(ws.name, "dev", "no file name, so the key names it");
        assert_eq!(ws.root.as_deref(), Some(d.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn module_workspace_follows_the_seed_precedence() {
        // `windows` beats `panes`, exactly as the launcher seeds them.
        let a = scratch_dir("prec-a");
        let b = scratch_dir("prec-b");
        let file = WorkspaceFile {
            panes: Some(vec![crate::workspace::model::PaneSpec {
                cwd: Some(a.to_str().unwrap().to_string()),
                ..Default::default()
            }]),
            windows: Some(vec![WindowSpec {
                groups: vec![crate::workspace::model::GroupSpec {
                    panes: vec![crate::workspace::model::PaneSpec {
                        cwd: Some(b.to_str().unwrap().to_string()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ws = module_workspace(&argv(&[]), "/nowhere", Some(&file));
        assert_eq!(ws.root.as_deref(), Some(b.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn inline_flags_win_over_everything() {
        // A present last-session file must be ignored when inline `-c` is given.
        let last = temp_file("inline-last");
        std::fs::write(&last, br#"{"panes":[{"command":"old"}]}"#).unwrap();
        let ws = resolve_launch_workspace_with(&argv(&["-c", "npm run dev"]), ".", &last)
            .expect("inline workspace");
        let panes = ws.panes.unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].command.as_deref(), Some("npm run dev"));
        let _ = std::fs::remove_file(&last);
    }

    #[test]
    fn falls_back_to_last_session_when_no_args() {
        let last = temp_file("fallback-last");
        std::fs::write(&last, br#"{"panes":[{"command":"restored","label":"r"}]}"#).unwrap();
        let ws = resolve_launch_workspace_with(&argv(&[]), ".", &last).expect("last session");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("restored"));
        let _ = std::fs::remove_file(&last);
    }

    #[test]
    fn returns_none_with_no_args_and_no_last_session() {
        let last = temp_file("none-last");
        let _ = std::fs::remove_file(&last);
        assert!(resolve_launch_workspace_with(&argv(&[]), ".", &last).is_none());
    }

    #[test]
    fn cli_workspace_takes_inline_but_never_last_session() {
        // `resolve_cli_workspace` is what the GUI uses: inline `-c` resolves…
        let ws =
            resolve_cli_workspace(&argv(&["-c", "npm run dev"]), ".").expect("inline workspace");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("npm run dev"));
        // …but a plain launch yields None even though a last-session file exists on disk (the GUI
        // must stay EmptyTab on a bare launch — that fallback belongs only to resolve_launch_workspace).
        assert!(resolve_cli_workspace(&argv(&[]), ".").is_none());
    }

    #[test]
    fn json_path_is_read_when_present() {
        let json = temp_file("explicit");
        std::fs::write(&json, br#"{"panes":[{"command":"fromfile","label":"f"}]}"#).unwrap();
        let json_str = json.to_string_lossy().into_owned();
        let no_last = temp_file("explicit-nolast");
        let _ = std::fs::remove_file(&no_last);
        let ws = resolve_launch_workspace_with(&argv(&[&json_str]), ".", &no_last)
            .expect("workspace from json path");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("fromfile"));
        let _ = std::fs::remove_file(&json);
    }

    /// The module runtime asks this before any window exists, so it must agree with the
    /// file the window will then load — and refuse names the marketplace refuses.
    #[test]
    fn the_launch_workspace_key_is_the_positional_files_stem() {
        // `parse_cli` only records a positional file that exists, so make them.
        let dir = std::env::temp_dir().join(format!("hp-launch-key-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("some dir")).unwrap();
        let dev = dir.join("some dir").join("dev.json");
        let ops = dir.join("ops.avada");
        let spaced = dir.join("my ws.json");
        for f in [&dev, &ops, &spaced] {
            std::fs::write(f, b"{}").unwrap();
        }
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(
            launch_workspace_key(&argv(&[&s(&dev)])).as_deref(),
            Some("dev")
        );
        assert_eq!(
            launch_workspace_key(&argv(&[&s(&ops)])).as_deref(),
            Some("ops")
        );
        // Inline flags and a bare launch name no workspace file.
        assert_eq!(launch_workspace_key(&argv(&["-c", "npm run dev"])), None);
        assert_eq!(launch_workspace_key(&argv(&[])), None);
        // A stem the marketplace would refuse as a key has no state to read.
        assert_eq!(launch_workspace_key(&argv(&[&s(&spaced)])), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
