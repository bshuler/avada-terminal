//! Command dispatch — Wave-2 **Seam #2**.
//!
//! Every user action — a top-bar click, a key shortcut, and (in Wave 2) a command
//! palette entry or a keybinding — is expressed as a [`Command`] and run through
//! [`dispatch`]. `dispatch` mutates the central [`State`] and returns an
//! [`Effect`] for the thin set of concerns that live outside the state (quitting,
//! OS fullscreen). Wave-2 features add variants here and emit them; they never
//! reach into the UI or the window glue themselves.

use avada_core::layout::navigate::Direction;
use avada_core::layout::presets::{DividerKind, Layout};
use avada_core::session_manager::SessionManager;

use crate::state::{DetachedPane, DetachedTab, NewPaneOpts, ReminderOffset, Setting, State};
use crate::theme;

/// The `--model` ids for the goals-system model pickers, indexed by the New-goal dialog's model
/// options. ORDER MUST MATCH [`GOAL_MODEL_LABELS`]. Defaults per tier: orchestrator/spec =
/// index 0 (opus), implementation = 1 (sonnet).
pub const GOAL_MODELS: [&str; 4] = [
    "claude-opus-5[1m]",
    "claude-sonnet-5[1m]",
    "claude-fable-5[1m]",
    "claude-haiku-4-5",
];

/// Display labels for [`GOAL_MODELS`] (the New-goal dialog's chips + option rows).
pub const GOAL_MODEL_LABELS: [&str; 4] = ["opus[1m]", "sonnet[1m]", "fable[1m]", "haiku"];

/// An action against the workspace. Construct these from any input source.
#[derive(Debug, Clone)]
pub enum Command {
    // panes
    /// Immediately spawn a default pane (the plain ＋ click / palette "New pane").
    NewPane,
    /// Open the "New pane" options dialog (Shift+＋ / the menus' "New pane…").
    OpenNewPane,
    /// Submit the New Pane dialog: spawn a pane from the configured options + close the dialog.
    /// Boxed: `NewPaneOpts` is ~288 bytes and would otherwise size the whole enum.
    SubmitNewPane(Box<NewPaneOpts>),
    /// Open the "New goal" box (command palette → "New goal…").
    OpenNewGoal,
    /// New-goal box: set the goal text — the mirror of the box's TextInput (pushed on `edited`).
    GoalQuery(String),
    /// New-goal box: Tab / Shift+Tab — reveal the option chips, then cycle focus among them.
    GoalNav(i32),
    /// New-goal box: reveal / hide the option chips (Ctrl+O).
    GoalToggleOptions,
    /// New-goal box: hide the option chips + any open list, back to the text field (Esc).
    GoalCollapse,
    /// New-goal box: open (`true`, ↓) / close (`false`) the focused field's option list
    /// (goal field = history; project field = projects; model fields = tiers).
    GoalMenu(bool),
    /// New-goal box: move the open option list's selection by ±1 (chip fields apply live).
    GoalMenuNav(i32),
    /// New-goal box: apply the option list's selected row to the focused field.
    GoalMenuPick,
    /// New-goal box (mouse): focus field `i` and open its option list.
    GoalFieldClick(usize),
    /// New-goal box (mouse): apply option row `i`.
    GoalMenuClick(usize),
    /// Submit the New-goal box (Enter / the submit icon). Routes to [`State::goal_submit`].
    GoalSubmit,
    /// New-goal box: paste the clipboard into the goal — an image becomes an attachment,
    /// text is appended to the goal text (Ctrl+V).
    GoalPasteClipboard,
    /// New-goal box: attach image(s) via the OS file picker.
    GoalAttachImage,
    /// New-goal box: capture the clipboard image (if any) as an attachment.
    GoalPasteImage,
    /// New-goal box: remove attached image `i`.
    GoalRemoveImage(usize),
    CloseFocused,
    ClosePane(usize),
    /// A directory row of a Family B file-browser pane was activated: point pane
    /// `usize` at the new directory. The pane keeps its kind and its uid — a browser
    /// navigating is not a new pane — so nothing about the session model moves.
    ViewNavigate(usize, String),
    /// Open a Family B file-browser pane rooted at pane `usize`'s live cwd. The in-app
    /// twin of [`Command::RevealPaneCwd`]: same starting directory, but the listing lands
    /// in a pane instead of the OS file explorer. This is the only way a human can reach
    /// a non-PTY view pane, so it is deliberately next to "Open Folder" in the pane menu.
    OpenFileBrowser(usize),

    // ---- files: revealing a path, and the host menu a file row gets ----
    //
    // The tree itself is no longer the app's: it is the `files` rail entry a module
    // registers (`bshuler/avada-files` by default). What stays here is what only the HOST
    // can do — deciding which entry a reveal goes to, and building the "Open in…" menu out
    // of the tool registry, which a module has no way to see.
    /// Show `path` in whichever module owns the `files` rail entry: open the panel on that
    /// entry, re-anchor the window's project, and emit `files.reveal`. `line`/`col` come
    /// from a `file:line:col` hit in a pane's output and ride along in the payload.
    ///
    /// This is where a plain click on a filename in a pane lands. It deliberately opens
    /// nothing: the human picks the tool from the row menu, which is the whole reason a
    /// clicked path goes to the panel instead of straight to the OS handler.
    RevealInFiles {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Open `0` in a read-only view pane — the Markdown renderer for `.md`, the plain file
    /// viewer otherwise. Also where a git row's "open" lands.
    FilesOpen(String),
    /// Make `0` the window's project root and reveal it (the row menu's "Open as Root" and
    /// "Browse Containing Folder").
    FilesSetRoot(String),
    // ---- left panel: the module rail (H4) ----
    /// A module entry on the mode strip was clicked. The payload is the entry KEY
    /// (`<owner/repo>#<entry-id>`), which is what the strip has and what routes the
    /// activation back to one module without a second lookup.
    RailActivate(String),
    /// Leave the active module entry and go back to the built-in sections.
    RailBack,
    /// A row under the active module entry was clicked. `2` is the row id and `3` the
    /// gesture (`open` / `toggle` / `context`); an unknown gesture is dropped rather than
    /// guessed, since the module acts on it.
    RailRow(String, String, String),
    /// A row inside a module's own pane was clicked. The pane twin of
    /// [`Command::RailRow`] — same key shape (`<owner/repo>#<surface>`), same gestures —
    /// and a separate variant because the two surfaces have separate row stores and a
    /// click on one must never resolve against the other's rows.
    ModulePaneRow(String, String, String),
    /// A module row was right-clicked, at window-logical `(x, y)`. Not folded into
    /// [`Command::RailRow`] with a `context` gesture because it goes two ways at once: the
    /// module is told, and the host opens its own file menu over the row's path.
    RailContext(String, String, f32, f32),
    /// The filter box under the active tier-1 module entry was edited. Carries the whole
    /// query rather than a keystroke, because it is a `rail.query` event to the module and
    /// the module re-sends the list it wants shown; the host never filters anything itself.
    RailQuery(String),
    // ---- module capability rights (H2) ----
    /// A click on the Preferences rights page or on the app-wide ask toast. The payload is
    /// already parsed (`prefs::rights::wire` drops an unreadable module id or capability
    /// name rather than sending a command that cannot be acted on).
    Rights(crate::prefs::rights::RightsCommand),
    /// Show one commit in the panel — sent by clicking a hash in a pane's output. `cwd` says
    /// which repository (a hash alone does not), and the panel re-roots there so the commit
    /// and the working tree behind it can never be from two different projects.
    ShowCommit {
        cwd: String,
        hash: String,
    },
    /// Open a pane showing a diff. `rev` picks which one: `Some` is `git show` for that
    /// commit, `None` is the working tree against HEAD. `path` is `None` for everything and
    /// `Some(rel)` for one repo-relative file.
    ///
    /// One command for both, where there used to be two, because the panel that knew which
    /// of the two it was showing has left for a module. What arrives now is a row plus the
    /// revision its own module said it came from ([`crate::state::GitOrigin`]), and that is
    /// a single question with a nullable answer, not two commands.
    ///
    /// The working tree is always compared against HEAD rather than switching between
    /// `git diff` and `git diff --cached` by section: a partially-staged file is in both
    /// lists at once, and the human clicking either row wants the whole change.
    ///
    /// A pane running git's own pager gives the real, coloured diff; teaching the file
    /// viewer to render one would only give a worse copy.
    GitDiff {
        root: String,
        rev: Option<String>,
        path: Option<String>,
    },
    /// Open `path` in a terminal pane running tool `tool` — "open in a terminal with vi".
    /// The pane is a `Tool` pane, so it gets the tool's brand and icon like any other.
    OpenPathWith {
        path: String,
        tool: String,
    },
    /// Hand `path` to one specific application the OS says can open its kind — the
    /// "Open With" flyout. `app` is a [`avada_core::open::HandlerApp`]'s launcher,
    /// which is opaque and per-OS, so it is passed straight back through the same seam
    /// that produced it.
    OpenPathInApp {
        path: String,
        app: String,
    },
    /// Open a shell pane in `path`'s directory with the command that runs it **typed but
    /// not submitted**. The interpreter is a guess — a shebang when the file has one, an
    /// extension when it doesn't — and a guess the human can see and correct before
    /// pressing Enter is worth more than one that runs immediately and wrongly.
    RunPath(String),
    /// Open a plain shell pane rooted at `path`, or at its parent when it names a file.
    TerminalAt(String),
    /// Copy an arbitrary path to the clipboard (the row menu). Goes through the focused
    /// pane's clipboard so it raises the same "Copied …" toast as a Ctrl+click does.
    CopyPathText(String),
    /// Show `path` in the OS file explorer (the row menu's "Reveal in Finder").
    RevealPath(String),
    FocusPane(usize),
    FocusDir(Direction),
    // layout
    SetLayout(Layout),
    /// Advance to the next layout preset. `dispatch` already services it; no input source
    /// constructs it yet (no key binding, no menu row), so it is dead until one does.
    #[allow(dead_code)]
    CycleLayout,
    ToggleZoom,
    ToggleFullscreen,
    // font zoom (Ctrl+= / Ctrl+- / Ctrl+0)
    /// Nudge the global terminal font size by `0` px (clamped), re-gridding every pane.
    FontZoom(i32),
    /// Reset the global terminal font size to its default.
    FontReset,
    ResizeDivider {
        kind: DividerKind,
        index: i32,
        delta: f64,
    },
    // tabs
    NewTab,
    CloseTab(usize),
    SwitchTab(usize),
    /// Switch to the next tab, wrapping around (Ctrl+Tab).
    NextTab,
    /// Switch to the previous tab, wrapping around (Ctrl+Shift+Tab).
    PrevTab,
    BeginRename(i32),
    RenameTab(i32, String),
    /// Begin editing pane `0`'s label inline (double-click on its header).
    BeginRenamePane(i32),
    /// Commit pane `0`'s label to `1` (blank keeps the prior label).
    RenamePane(i32, String),
    // ---- pane context-menu actions (target a specific pane by active-tab index) ----
    /// Recolor pane `0` to swatch `1` of the active frame palette (pins it + frame/dot on).
    RecolorPane(usize, usize),
    /// Set pane `0`'s per-pane frame override to `1`.
    SetPaneFrame(usize, bool),
    /// Set pane `0`'s per-pane dot override to `1`.
    SetPaneDot(usize, bool),
    /// Toggle whether pane `0`'s ambient-AI summary line is muted.
    ToggleMuteAi(usize),
    /// Toggle whether pane `0`'s "talk" (speak new assistant replies aloud) is on.
    ToggleTalk(usize),
    /// Toggle pane `0`'s microphone: start recording dictation, or stop it and type the
    /// transcript into that pane (the header mic button / the pane menu's "Dictate").
    ToggleDictation(usize),
    /// Move view pane `0`'s row selection to row `1`; `2` extends from the existing anchor
    /// (shift-click) instead of replacing the selection.
    ViewSelect(usize, usize, bool),
    // ---- speech (global; routed to the ControlHost's SpeechService) ----
    /// Kill any in-flight/queued speech immediately (command palette "Speech: Stop Now").
    SpeechStopNow,
    /// Toggle the global speech mute flag.
    SpeechToggleMuted,
    /// Toggle "only speak the focused pane" (background talkers stay silent while unfocused).
    SpeechToggleFocusedOnly,
    /// Maximize/restore (zoom-in-tab) pane `0`.
    ZoomPane(usize),
    /// Fullscreen/exit-fullscreen pane `0`.
    FullscreenPane(usize),
    /// Restart pane `0`'s shell (kills + respawns its session in place).
    RestartPane(usize),
    /// Re-resolve a FRESH (registry-backed) environment and restart pane `0`'s shell in
    /// place, keeping its live cwd + env overrides (#28; the pane menu's "Refresh Env").
    RefreshEnvPane(usize),
    /// Open pane `0`'s current working directory in the OS file explorer (#23).
    RevealPaneCwd(usize),
    /// Route a URL through Preferences → Browser: the OS default handler, one chosen
    /// browser, or the [`crate::state::Overlay::AskBrowser`] chooser. The single entry
    /// point for "something in a pane wants a link opened", so the setting can never be
    /// bypassed by a caller that opens a URL directly.
    OpenLink(String),
    /// Answer the browser chooser: open its held URL in browser row `0`, then close it.
    PickBrowser(usize),
    /// Open the in-pane search box on pane `0`.
    SearchPane(usize),
    /// Open the in-pane search box on the focused pane (the Ctrl+F keybinding).
    SearchFocused,
    /// Copy pane `0`'s current selection to the clipboard.
    CopyPane(usize),
    /// Copy the focused pane's selection (the Ctrl+Shift+C keybinding) — the explicit copy
    /// gesture now that copy-on-select defaults off. No-op without a selection.
    CopyFocused,
    /// Paste the clipboard into pane `0`'s session.
    PastePane(usize),
    /// Paste the clipboard into the focused pane's session (the Ctrl+V keybinding). Reads
    /// the OS clipboard fresh app-side (arboard, with open retries) instead of forwarding a
    /// raw 0x16 for the shell to resolve — PSReadLine's own clipboard read has no retry and
    /// can come up empty/stale right after an external copy (#9). Unbinding `pane.paste`
    /// in Preferences restores the literal-0x16 passthrough for shells that want it.
    PasteFocused,
    /// Forward a literal Ctrl+V (0x16) to the focused pane (the Alt+V keybinding) so an in-pane
    /// TUI that reads the OS clipboard itself — e.g. Claude Code's image paste — can pull a
    /// clipboard IMAGE. avada' text paste can't carry image bytes through the pty; this
    /// hands the clipboard read to the focused program. Matches the shortcut Claude Code
    /// documents for terminals that intercept Ctrl+V.
    PasteImageFocused,
    /// Select all of pane `0`'s viewport.
    SelectAllPane(usize),
    /// Clear pane `0`'s screen + scrollback.
    ClearPane(usize),
    // ---- reminder panes (Track F) ----
    /// Park pane `0` until quick-offset `1` from now: it leaves the layout but its session
    /// stays alive; it lives in the sidebar bell list until restored.
    RemindPane(usize, ReminderOffset),
    /// Toggle the sidebar bell's reminder-list panel.
    ToggleReminders,
    /// Re-dock the parked pane with session uid `0` into the active tab + clear its reminder.
    RestoreReminder(String),
    /// Hide the fired-reminder alert toast for session uid `0` (the reminder + bell badge stay).
    DismissReminderToast(String),
    /// Move pane `0` into a brand-new tab (disabled when its tab has <2 panes).
    MovePaneToNewTab(usize),
    /// Move pane `0` into existing tab `1`.
    MovePaneToTab(usize, usize),
    // ---- tab context-menu actions (target a specific tab by index) ----
    /// Duplicate tab `0` (a fresh tab with the same number of panes + its layout).
    DuplicateTab(usize),
    /// Close every tab except tab `0`.
    CloseOtherTabs(usize),
    /// Close every tab to the right of tab `0`.
    CloseTabsToRight(usize),
    /// Reopen the most-recently closed pane or tab (replay-primed; no-op when none).
    ReopenClosedTab,
    /// Toggle the rail's RECENTLY CLOSED history section.
    ToggleClosed,
    /// Reopen the history entry with id `0` (a row click — id, not index, so it can't race).
    RestoreClosed(i32),
    /// Drop history entry `0` for good, killing the sessions it held open.
    DiscardClosed(i32),
    /// The close-confirmation card's "Close" — carry out the close it was holding.
    ConfirmCloseGo,
    /// The card's "ask before closing" checkbox.
    SetConfirmClose(bool),
    /// Set tab `0`'s layout to `1`.
    SetTabLayout(usize, Layout),
    /// Move the whole of tab `0` to a new OS window.
    MoveTabToNewWindow(usize),
    // ---- context-menu lifecycle ----
    /// Open the pane context menu for pane `0` at window-logical `(1, 2)`.
    OpenPaneContext(usize, f32, f32),
    /// Open the single-layout taskbar's pane menu for pane `0` at `(1, 2)` (the `inTaskbar`
    /// variant: a leading Show row, no Maximize).
    OpenTaskbarContext(usize, f32, f32),
    /// Open the tab context menu for tab `0` at window-logical `(1, 2)`.
    OpenTabContext(usize, f32, f32),
    /// Open the application (hamburger) menu at window-logical `(0, 1)`.
    OpenAppContext(f32, f32),
    /// Dismiss the open context menu.
    CloseContext,
    // ---- workspace file (application menu) ----
    /// Pick a `workspace.json` and load it (the application menu's "Open workspace…").
    OpenWorkspace,
    /// Serialize the active tab and save it to a chosen file (the menu's "Save workspace…").
    /// Writes back silently to the remembered path once the workspace has one.
    SaveWorkspace,
    // ---- workspace library + sets (M6) ----
    /// Always prompt for a destination, save the active tab there, and remember it.
    SaveWorkspaceAs,
    /// Write the active tab into the checkout it is working in, as
    /// `.avada/project.json` — the layout travels with the repo, not the laptop.
    SaveProject,
    /// Save every non-empty tab as a member workspace and index them in a `sets/*.json`.
    SaveSet,
    /// Pick a saved set and load every member workspace (reattach-or-spawn per pane).
    OpenSet,
    // ---- multi-window (Phase 4) ----
    /// Open a fresh OS window with an empty tab.
    NewWindow,
    /// Re-host the focused pane in a new OS window (replay-primed, no PTY restart).
    MovePaneToNewWindow,
    // ---- Wave-2 overlays (Seam #3) ----
    /// Dismiss whatever overlay panel is open.
    CloseOverlay,
    // command palette
    PaletteOpen,
    PaletteQuery(String),
    /// Move the highlighted palette row by ±1.
    PaletteNav(i32),
    /// Highlight a specific visible palette row (a mouse hover/click).
    PaletteSelect(usize),
    /// Run the highlighted palette row's command (then close the palette).
    PaletteActivate,
    // preferences
    PrefsOpen,
    ApplySetting(Setting),
    /// Edit the appearance draft (previews only; commits on Done).
    DraftSetting(Setting),
    /// Commit the appearance draft and close (the Done button / Save).
    PrefsDone,
    /// Resolve the save/discard prompt: 0 keep · 1 discard · 2 save.
    PrefsConfirm(i32),
    /// Font picker: select option `i` (== FONT_OPTIONS.len() → Custom… mode).
    FontSelect(usize),
    /// Font picker: set the custom font path typed in the Custom… field.
    FontCustomValue(String),
    // sidebar / projects
    /// Show/hide the whole right-edge rail.
    ToggleSidebar,
    /// Expand/collapse the projects flyout behind the 📁 icon.
    ToggleProjects,
    OpenProject(usize),
    /// Recolor flyout row `0` to palette swatch `1`.
    SetProjectColor(usize, usize),
    /// Rename flyout row `0` to `1`.
    RenameProject(usize, String),
    /// Forget flyout row `0`.
    RemoveProject(usize),
    /// Open the "Add project" dialog (the ＋ on the sidebar's PROJECTS header).
    OpenAddProject,
    /// Submit the Add-Project dialog with the typed directory path (validated in state;
    /// a bad path keeps the dialog open with an inline error).
    SubmitAddProject(String),
    // ---- the left slide-out panel (mux plan M5) ----
    /// Show/hide the left panel (workspace tree · library · detached sessions).
    ToggleLeftPanel,
    /// Workspace tree: focus pane `1` of tab `0` (switching to that tab first).
    LeftFocusPane(usize, usize),
    /// Workspace tree: drag pane `1` of tab `0` onto tab `2`, landing at insertion index `3`
    /// among that tab's panes (re-host, no PTY restart).
    LeftMovePane(usize, usize, usize, usize),
    /// Workspace tree: drag pane `1` of tab `0` to insertion index `2` within its OWN tab —
    /// the same gesture as a cross-group drop, resolved as a reorder because it never left.
    LeftReorderPane(usize, usize, usize),
    /// Save the active tab into the workspace library (no file dialog).
    ///
    /// The LIBRARY and SETS drawers are gone from the panel — they are the `avada-workspace`
    /// module's surface now — but this one stayed: it is on the command palette, and a
    /// palette entry is not a panel view. Opening still has a home too, as
    /// [`crate::state::State::open_workspace_path`], which is the shape the module contract
    /// hands out; the row-index forms went with the drawers that produced the indices.
    LeftSaveWorkspace,
    /// Detached: adopt live session uid `0` into the active tab (re-attach + replay).
    LeftAdoptSession(String),
    /// Relaunch the GUI from the installed bundle, leaving the session daemon (and every
    /// pane) alone. How a freshly installed build goes live without touching the panes.
    RestartApp,
    /// A container row of a data pane was clicked: fold or unfold node `.1` (a path such
    /// as `$.deps.serde`) of the active tab's pane `.0`. View state, not a file edit, and
    /// not persisted across restarts.
    ViewToggleNode(usize, String),
}

/// A side effect the controller must apply outside the state (UI/window layer). The
/// multi-window layer ([`crate::app`]) applies these against the owning window + the
/// app-level window registry.
#[derive(Debug)]
pub enum Effect {
    None,
    /// The workspace is empty — close this window (and quit when it was the last).
    Quit,
    /// Apply OS fullscreen (true) or restore (false) to this window.
    SetFullscreen(bool),
    /// Open a fresh empty OS window.
    NewWindow,
    /// Re-host `det` in a new OS window; `source_alive` is `false` when detaching it
    /// emptied this window (so the controller closes it).
    MoveToNewWindow {
        /// Boxed: a `DetachedPane` is ~313 bytes and would otherwise size the whole enum.
        det: Box<DetachedPane>,
        source_alive: bool,
    },
    /// Re-host a whole tab (its panes, title + layout) in a new OS window. `source_alive`
    /// is `false` when moving it emptied this window.
    MoveTabToNewWindow {
        tab: DetachedTab,
        source_alive: bool,
    },
    /// Speech commands route through the `ControlHost`'s `SpeechService` (owned above `State`),
    /// so `dispatch` bubbles them up as effects rather than mutating state directly.
    SpeechStopNow,
    SpeechToggleMuted,
    SpeechToggleFocusedOnly,
    /// Dictation lives on the same `ControlHost` (its `DictationService`), and the pane index
    /// is meaningless up there — so `dispatch` resolves it to a session uid and bubbles that.
    ToggleDictation(String),
    /// Relaunch the GUI (scope 1 — the daemon and its panes are left running). Bubbled
    /// rather than done here: the restart is serviced by the app tick, above `State`.
    RestartApp,
    /// A pane's session was swapped for a fresh one by an in-place restart (`RestartPane` /
    /// `RefreshEnvPane`): `(old_uid, new_uid)`. Bubbled so the app can re-point the
    /// control-plane pane-id alias, exactly as the exit-fallback path does — without it a
    /// menu restart silently gave the pane a new id mid-conversation.
    Rebound(String, String),
}

/// The keyboard layout-cycle order (skips `single`, which the menu still offers).
const LAYOUT_CYCLE: [Layout; 5] = [
    Layout::Auto,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

/// Run `cmd` against `state`. Returns any [`Effect`] the caller must apply.
#[tracing::instrument(level = "debug", skip_all)]
pub fn dispatch(state: &mut State, cmd: Command, mgr: &SessionManager) -> Effect {
    // Any action other than renaming itself cancels an in-progress tab rename,
    // so the inline edit box never lingers when you interact elsewhere.
    //
    // `CloseContext` is exempt: picking "Rename…" out of a context menu dispatches the
    // picked command and then closes the menu, in that order and in the same click. If
    // dismissing the menu counted as "interacting elsewhere" it would cancel the very
    // edit the pick just started, and the inline box would never appear.
    if state.editing_tab != -1
        && !matches!(
            cmd,
            Command::BeginRename(_) | Command::RenameTab(..) | Command::CloseContext
        )
    {
        state.editing_tab = -1;
        state.dirty = true;
    }
    // Likewise, any action other than a pane rename cancels an in-progress pane-label edit
    // (so the inline box never lingers when you interact elsewhere).
    if state.editing_pane != -1
        && !matches!(
            cmd,
            Command::BeginRenamePane(_) | Command::RenamePane(..) | Command::CloseContext
        )
    {
        state.editing_pane = -1;
        state.dirty = true;
    }
    match cmd {
        Command::NewPane => state.add_pane(mgr),
        Command::OpenNewPane => state.open_new_pane(),
        Command::SubmitNewPane(opts) => {
            state.add_pane_opts(mgr, *opts);
            state.close_overlay();
        }
        Command::OpenNewGoal => state.open_new_goal(),
        Command::GoalQuery(q) => state.goal_set_text(q),
        Command::GoalNav(d) => state.goal_nav(d),
        Command::GoalToggleOptions => state.goal_toggle_options(),
        Command::GoalCollapse => state.goal_collapse(),
        Command::GoalMenu(open) => state.goal_menu_toggle(open),
        Command::GoalMenuNav(d) => state.goal_menu_nav(d),
        Command::GoalMenuPick => state.goal_menu_pick(),
        Command::GoalFieldClick(i) => state.goal_field_click(i),
        Command::GoalMenuClick(i) => state.goal_menu_click(i),
        Command::GoalSubmit => state.goal_submit(mgr),
        Command::GoalPasteClipboard => state.goal_paste_clipboard(),
        Command::GoalAttachImage => state.goal_attach_images(),
        Command::GoalPasteImage => {
            state.goal_paste_image();
        }
        Command::GoalRemoveImage(i) => state.goal_remove_image(i),
        Command::CloseFocused => {
            // Asks first (unless the human turned that off), then parks rather than kills —
            // see `State::request_close_pane`.
            if !state.request_close_focused(mgr) {
                return Effect::Quit;
            }
        }
        Command::ClosePane(i) => {
            let ti = state.active;
            if !state.request_close_pane(ti, i, mgr) {
                return Effect::Quit;
            }
        }
        Command::FocusPane(i) => state.focus_pane(i),
        Command::ViewNavigate(i, target) => state.view_navigate(i, target),
        Command::FocusDir(d) => state.focus_dir(d),
        Command::SetLayout(l) => state.set_layout(l),
        Command::CycleLayout => {
            let cur = state.active_tab().layout;
            let i = LAYOUT_CYCLE.iter().position(|l| *l == cur).unwrap_or(0);
            state.set_layout(LAYOUT_CYCLE[(i + 1) % LAYOUT_CYCLE.len()]);
        }
        Command::ToggleZoom => state.toggle_zoom(),
        Command::ToggleFullscreen => {
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::FontZoom(delta) => state.font_zoom(delta),
        Command::FontReset => state.font_reset(),
        Command::ResizeDivider { kind, index, delta } => state.resize_divider(kind, index, delta),
        Command::NewTab => state.new_tab(mgr),
        Command::CloseTab(i) => {
            // Asks first, then parks (sessions alive) on the recently-closed history; closing
            // the last tab still kills + quits.
            if !state.request_close_tab(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::SwitchTab(i) => state.switch_tab(i),
        Command::NextTab => state.cycle_tab(1),
        Command::PrevTab => state.cycle_tab(-1),
        Command::BeginRename(i) => state.begin_rename(i),
        Command::RenameTab(i, t) => state.rename_tab(i, &t),
        Command::BeginRenamePane(i) => state.begin_rename_pane(i),
        Command::RenamePane(i, t) => state.rename_pane(i, &t),
        // ---- pane context-menu actions ----
        Command::RecolorPane(i, swatch) => state.recolor_pane(i, swatch),
        Command::SetPaneFrame(i, on) => state.set_pane_frame(i, on),
        Command::SetPaneDot(i, on) => state.set_pane_dot(i, on),
        Command::ToggleMuteAi(i) => state.toggle_mute_ai(i),
        Command::ToggleTalk(i) => state.toggle_talk(i),
        Command::ToggleDictation(i) => {
            return match state.active_tab().panes.get(i) {
                // A view pane is read-only: there is no pty to type the transcript into
                // (D3), so dictating into one could only ever throw the recording away.
                // The mic is hidden there too — this is the belt to that braces, for the
                // palette and any other caller that reaches the command directly.
                Some(p) if p.kind.is_pty() => Effect::ToggleDictation(p.uid.clone()),
                _ => Effect::None,
            };
        }
        Command::ViewSelect(i, row, extend) => state.view_select(i, row, extend),
        Command::SpeechStopNow => return Effect::SpeechStopNow,
        Command::RestartApp => return Effect::RestartApp,
        Command::ViewToggleNode(i, node) => crate::datatree::toggle_pane(state, i, &node),
        Command::SpeechToggleMuted => return Effect::SpeechToggleMuted,
        Command::SpeechToggleFocusedOnly => return Effect::SpeechToggleFocusedOnly,
        Command::ZoomPane(i) => state.zoom_pane(i),
        Command::FullscreenPane(i) => {
            state.focus_pane(i);
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::RestartPane(i) => {
            if let Some((old, new)) = state.restart_pane(i, mgr) {
                return Effect::Rebound(old, new);
            }
        }
        Command::RefreshEnvPane(i) => {
            if let Some((old, new)) = state.refresh_env_pane(i, mgr) {
                return Effect::Rebound(old, new);
            }
        }
        Command::RevealPaneCwd(i) => {
            // Open the pane's live cwd (reported by shell integration) in the OS file explorer.
            // This used to branch `explorer` / `xdg-open` inline, which meant it did nothing
            // at all on macOS (no `xdg-open` there); `core::open` owns the per-OS launch now.
            if let Some(cwd) = state.active_tab().panes.get(i).and_then(|p| p.cwd.clone()) {
                if let Err(e) = avada_core::open::reveal_path(std::path::Path::new(&cwd)) {
                    tracing::debug!("RevealPaneCwd {cwd}: {e}");
                }
            }
        }
        Command::OpenLink(url) => {
            // The routing itself lives in `State::open_link` (it may mount an overlay, so it
            // needs `&mut State`); all that's left here is saying why a link went nowhere.
            if let Err(e) = state.open_link(&url) {
                tracing::debug!("OpenLink: {e}");
            }
        }
        Command::PickBrowser(i) => {
            if let Err(e) = state.pick_browser(i) {
                tracing::debug!("PickBrowser {i}: {e}");
            }
        }
        Command::OpenFileBrowser(i) => {
            // The pane's live cwd if shell integration reported one, else its configured
            // cwd, else home — a browser with nowhere to start is worse than one that
            // starts somewhere obvious.
            let start = state
                .active_tab()
                .panes
                .get(i)
                .and_then(|p| p.cwd.clone().filter(|c| !c.is_empty()))
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .or_else(|| std::env::var("USERPROFILE").ok())
                });
            let label = start
                .as_deref()
                .map(std::path::Path::new)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some("Files".to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    // A view pane's target IS its cwd — see `State::view_navigate`.
                    cwd: start,
                    command: None,
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(avada_core::tools::kind::PaneKind::FileBrowser),
                    // A view pane holds a directory, not a conversation.
                    session: None,
                },
            );
        }

        // ---- reveal a path in whatever module owns the `files` rail entry ----
        Command::RevealInFiles { path, line, col } => {
            state.reveal_in_files(std::path::Path::new(&path), line, col);
        }
        Command::FilesSetRoot(dir) => {
            // "Open as Root" re-anchors the window's project and reveals the directory, so
            // the module re-roots there on its own terms. The host no longer owns a tree to
            // point at one.
            let dir = std::path::PathBuf::from(dir);
            state.set_project_root(dir.clone());
            state.reveal_in_files(&dir, None, None);
        }
        // ---- left panel: the module rail (H4) ----
        Command::RailActivate(key) => state.rail_activate(&key),
        Command::RailBack => state.rail_deactivate(),
        Command::RailRow(key, row, gesture) => {
            let Some(g) = crate::leftpanel::RailGesture::parse(&gesture) else {
                return Effect::None;
            };
            state.rail_row(&key, &row, g);
        }
        Command::ModulePaneRow(key, row, gesture) => {
            let (Some((module, surface)), Some(g)) = (
                crate::leftpanel::split_key(&key),
                crate::leftpanel::RailGesture::parse(&gesture),
            ) else {
                return Effect::None;
            };
            state.module_pane_row(&module, surface, &row, g);
        }
        Command::RailContext(key, row, x, y) => state.rail_context(&key, &row, x, y),
        Command::RailQuery(q) => state.rail_query(&q),
        // ---- module capability rights (H2) ----
        Command::Rights(cmd) => state.rights_apply(&cmd),
        // ---- git links (J) ----
        Command::ShowCommit { cwd, hash } => {
            if !state.show_commit(&cwd, &hash) {
                state.toast_active(&format!(
                    "No commit {} in this repository",
                    &hash[..hash.len().min(9)]
                ));
            }
        }
        Command::GitDiff { root, rev, path } => {
            // `--color=always` in both shapes: once a pager is in the chain git no longer
            // sees a terminal on its own end and turns colour off by itself.
            let (mut cmd, name) = match &rev {
                Some(rev) => (
                    format!(
                        "git -C {} show --color=always {}",
                        quote_arg(&root),
                        quote_arg(rev)
                    ),
                    rev.chars().take(7).collect::<String>(),
                ),
                None => (
                    format!("git -C {} diff --color=always HEAD", quote_arg(&root)),
                    "diff".to_string(),
                ),
            };
            let label = match &path {
                Some(p) => {
                    cmd.push_str(&format!(" -- {}", quote_arg(p)));
                    format!("{} · {}", name, p.rsplit('/').next().unwrap_or(p))
                }
                None => name,
            };
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label: Some(label),
                    cwd: Some(root),
                    command: Some(cmd),
                    ..Default::default()
                },
            );
        }
        Command::FilesOpen(path) => {
            let p = std::path::PathBuf::from(&path);
            if p.is_dir() {
                state.set_project_root(p);
                return Effect::None;
            }
            // `.md` gets the renderer, everything else the plain viewer — the same split the
            // pane menu makes, so a file opens the same way however it was reached.
            let md = p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown")
            });
            let kind = if md {
                avada_core::tools::kind::PaneKind::Markdown
            } else {
                avada_core::tools::kind::PaneKind::FileViewer
            };
            let label = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some("File".to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    // A view pane's target IS its cwd — see `State::view_navigate`.
                    cwd: Some(path),
                    command: None,
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(kind),
                    // A view pane holds a file, not a conversation.
                    session: None,
                },
            );
        }
        Command::OpenPathWith { path, tool } => {
            // The tool's resolved binary, so a user override in Preferences → Tools is what
            // actually runs. Falling back to the registry's bare bin name lets PATH decide,
            // which is right when the override was cleared but the tool is still installed.
            let Some(def) = avada_core::tools::registry::by_id(&tool) else {
                tracing::debug!("OpenPathWith: unknown tool {tool}");
                return Effect::None;
            };
            let bin = avada_core::tools::detect::resolve(def, &state.settings.tool_paths)
                .map(|r| r.path.display().to_string())
                .unwrap_or_else(|| def.bin.to_string());
            let p = std::path::PathBuf::from(&path);
            // The editor runs *in* the file's directory, so its own file-relative commands
            // (`:e ../other`, a project search) mean what the human expects.
            let cwd = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string());
            let label = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .or_else(|| Some(def.name.to_string()));
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label,
                    cwd,
                    command: Some(format!("{} {}", quote_arg(&bin), quote_arg(&path))),
                    shell: None,
                    accent: None,
                    show_frame: None,
                    show_dot: None,
                    env: None,
                    startup: None,
                    kind: Some(avada_core::tools::kind::PaneKind::Tool(tool)),
                    // "Open this file in vim" starts an editor on a file — there is no
                    // conversation to come back to.
                    session: None,
                },
            );
        }
        Command::CopyPathText(path) => {
            let f = state.active_tab().focused;
            state.copy_link_text(f, &path);
        }
        Command::OpenPathInApp { path, app } => {
            if let Err(e) = avada_core::open::open_path_with(&app, std::path::Path::new(&path)) {
                tracing::debug!("OpenPathInApp {path} in {app}: {e}");
            }
        }
        Command::RunPath(path) => {
            let p = std::path::PathBuf::from(&path);
            let dir = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string());
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            // Relative to the pane's own cwd, which is the file's directory — so the
            // command reads the way a human would type it standing there.
            let target = format!("./{name}");
            let typed = match run_prefix(&p) {
                Some(bin) => format!("{bin} {}", quote_arg(&target)),
                None => quote_arg(&target),
            };
            state.add_pane_opts(
                mgr,
                NewPaneOpts {
                    label: Some(name),
                    cwd: dir,
                    // No trailing carriage return: the pane shows the command and waits.
                    startup: Some(typed),
                    ..Default::default()
                },
            );
        }
        Command::TerminalAt(path) => {
            let p = std::path::PathBuf::from(&path);
            let dir = if p.is_dir() {
                Some(p)
            } else {
                p.parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(|d| d.to_path_buf())
            };
            state.add_pane_cwd(mgr, dir.map(|d| d.display().to_string()), None);
        }
        Command::RevealPath(path) => {
            if let Err(e) = avada_core::open::reveal_path(std::path::Path::new(&path)) {
                tracing::debug!("RevealPath {path}: {e}");
            }
        }
        Command::SearchPane(i) => state.open_search(i),
        Command::SearchFocused => {
            let f = state.active_tab().focused;
            state.open_search(f);
        }
        Command::CopyPane(i) => state.copy_pane(i),
        Command::CopyFocused => {
            let f = state.active_tab().focused;
            state.copy_pane(f);
        }
        Command::PastePane(i) => state.paste_pane(i, mgr),
        Command::PasteFocused => {
            let f = state.active_tab().focused;
            state.paste_pane(f, mgr);
        }
        Command::PasteImageFocused => {
            let f = state.active_tab().focused;
            state.paste_image_focused(f, mgr);
        }
        Command::SelectAllPane(i) => state.select_all_pane(i),
        Command::ClearPane(i) => state.clear_pane(i),
        // ---- reminder panes ----
        Command::RemindPane(i, off) => state.remind_pane(i, off),
        Command::ToggleReminders => state.toggle_reminders(),
        Command::RestoreReminder(uid) => state.restore_reminder(&uid, mgr),
        Command::DismissReminderToast(uid) => state.dismiss_reminder_toast(&uid),
        Command::MovePaneToNewTab(i) => state.move_pane_to_new_tab(i, mgr),
        Command::MovePaneToTab(i, t) => state.move_pane_to_tab(i, t, mgr),
        // ---- tab context-menu actions ----
        Command::DuplicateTab(i) => state.duplicate_tab(i, mgr),
        Command::CloseOtherTabs(i) => state.close_other_tabs(i, mgr),
        Command::CloseTabsToRight(i) => state.close_tabs_to_right(i, mgr),
        Command::ReopenClosedTab => state.reopen_closed(mgr),
        Command::ToggleClosed => state.toggle_closed(),
        Command::RestoreClosed(id) => state.restore_closed(id, mgr),
        Command::DiscardClosed(id) => state.discard_closed(id, mgr),
        Command::ConfirmCloseGo => {
            if !state.confirm_close_go(mgr) {
                return Effect::Quit;
            }
        }
        Command::SetConfirmClose(on) => state.set_confirm_close(on),
        Command::SetTabLayout(i, l) => state.set_tab_layout(i, l),
        Command::MoveTabToNewWindow(i) => {
            if let Some((tab, source_alive)) = state.detach_tab(i) {
                return Effect::MoveTabToNewWindow { tab, source_alive };
            }
        }
        // ---- context-menu lifecycle ----
        Command::OpenPaneContext(i, x, y) => state.open_pane_context(i, x, y),
        Command::OpenTaskbarContext(i, x, y) => state.open_taskbar_context(i, x, y),
        Command::OpenTabContext(i, x, y) => state.open_tab_context(i, x, y),
        Command::OpenAppContext(x, y) => state.open_app_context(x, y),
        Command::CloseContext => state.close_context(),
        // ---- workspace file (application menu) ----
        Command::OpenWorkspace => state.open_workspace(mgr),
        Command::SaveWorkspace => state.save_workspace(),
        // ---- workspace library + sets (M6) ----
        Command::SaveWorkspaceAs => state.save_workspace_as(),
        Command::SaveProject => state.save_project(),
        Command::SaveSet => state.save_set(),
        Command::OpenSet => state.open_set(mgr),
        // ---- multi-window ----
        Command::NewWindow => return Effect::NewWindow,
        Command::MovePaneToNewWindow => {
            if let Some((det, source_alive)) = state.detach_focused(mgr) {
                return Effect::MoveToNewWindow {
                    det: Box::new(det),
                    source_alive,
                };
            }
        }
        // ---- Wave-2 overlays ----
        Command::CloseOverlay => state.close_overlay(),
        // Ctrl+Shift+P TOGGLES (the binding id is `palette.toggle`, matching the renderer):
        // pressed with the palette already up it dismisses instead of resetting the query.
        Command::PaletteOpen => {
            if state.overlay == crate::state::Overlay::Palette {
                state.close_overlay();
            } else {
                state.open_palette();
            }
        }
        Command::PaletteQuery(q) => state.palette_set_query(&q),
        Command::PaletteNav(d) => state.palette_nav(d),
        Command::PaletteSelect(i) => state.palette_select(i),
        Command::PaletteActivate => {
            // Run the highlighted entry's command through the same dispatch, then close.
            if let Some(inner) = state.palette_command() {
                state.close_overlay();
                return dispatch(state, inner, mgr);
            }
            state.close_overlay();
        }
        Command::PrefsOpen => state.open_prefs(),
        Command::ApplySetting(s) => state.apply_setting(s),
        Command::DraftSetting(s) => state.draft_setting(s),
        Command::PrefsDone => state.prefs_done(),
        Command::PrefsConfirm(a) => state.prefs_confirm_resolve(a),
        Command::FontSelect(i) => state.font_select(i),
        Command::FontCustomValue(v) => state.font_custom_value(v),
        Command::ToggleSidebar => state.toggle_sidebar(),
        Command::ToggleProjects => state.toggle_projects(),
        Command::OpenProject(i) => state.open_project(i, mgr),
        Command::SetProjectColor(i, swatch) => state.set_project_color(i, swatch),
        Command::RenameProject(i, name) => state.rename_project(i, &name),
        Command::RemoveProject(i) => state.remove_project(i),
        Command::OpenAddProject => state.open_add_project(),
        Command::SubmitAddProject(path) => state.submit_add_project(&path),
        // ---- the left slide-out panel ----
        Command::ToggleLeftPanel => state.toggle_left_panel(),
        Command::LeftFocusPane(ti, i) => state.focus_pane_in_tab(ti, i),
        Command::LeftMovePane(from, i, to, at) => {
            state.move_pane_between_tabs_at(from, i, to, at, mgr)
        }
        Command::LeftReorderPane(ti, from, to) => state.reorder_pane_in(ti, from, to),
        Command::LeftSaveWorkspace => state.save_workspace_to_library(),
        Command::LeftAdoptSession(uid) => state.adopt_detached_session(&uid, mgr),
    }
    Effect::None
}

/// Map a layout menu id (from the Slint picker) to a `SetLayout` command.
#[tracing::instrument(level = "debug", ret)]
pub fn set_layout_from_id(id: i32) -> Command {
    Command::SetLayout(theme::layout_from_id(id))
}

/// The interpreter to type in front of a file to run it, or `None` when the file runs
/// itself (an executable with no shebang, or one whose kind we don't recognise).
///
/// A shebang beats the table, because the file has said what it wants. Only the first line
/// is read, and only when it looks like one: a binary's "first line" can be the whole file.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn run_prefix(path: &std::path::Path) -> Option<String> {
    if let Some(line) = shebang(path) {
        return Some(line);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let bin = match ext.as_str() {
        "py" => "python3",
        "rb" => "ruby",
        "pl" => "perl",
        "php" => "php",
        "js" | "mjs" | "cjs" => "node",
        "lua" => "lua",
        "r" => "Rscript",
        "ps1" => "pwsh",
        "jar" => "java -jar",
        "sh" | "bash" | "zsh" | "fish" => &ext,
        _ => return None,
    };
    Some(bin.to_string())
}

/// The command a file's `#!` line names, without the `#!`. `None` when there isn't one, or
/// when what follows isn't a plain command line.
#[tracing::instrument(level = "debug", ret)]
fn shebang(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .ok()?;
    let head = head.get(..n)?;
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|b| *b == b'\n' || *b == b'\r').next()?;
    let line = std::str::from_utf8(line).ok()?.trim();
    (!line.is_empty() && line.len() <= 200 && !line.chars().any(|c| c.is_control()))
        .then(|| line.to_string())
}

/// Wrap a path (or a program path) for the shell that runs a pane's `command`. Single quotes
/// with `'\''` for an embedded quote — the one form that is literal for every character in
/// POSIX shells, which matters here because filenames chosen by humans contain spaces,
/// parentheses and `$` far more often than they contain apostrophes.
///
/// A bare word is left alone so the common case reads as itself in the pane header.
#[tracing::instrument(level = "debug", ret)]
fn quote_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '=' | ':' | '~')
        });
    if plain {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod quote_tests {
    use super::quote_arg;

    #[test]
    fn an_ordinary_path_is_left_as_it_reads() {
        assert_eq!(quote_arg("/usr/bin/vim"), "/usr/bin/vim");
        assert_eq!(quote_arg("src/state.rs"), "src/state.rs");
    }

    #[test]
    fn a_path_with_shell_metacharacters_is_quoted_whole() {
        assert_eq!(quote_arg("/tmp/my notes.md"), "'/tmp/my notes.md'");
        assert_eq!(quote_arg("/tmp/$HOME (1).txt"), "'/tmp/$HOME (1).txt'");
    }

    #[test]
    fn an_apostrophe_closes_and_reopens_the_quoting() {
        // The classic: 'it'\''s' — four tokens the shell concatenates back into `it's`.
        assert_eq!(quote_arg("it's"), r"'it'\''s'");
    }
}

#[cfg(test)]
mod rename_from_menu_tests {
    //! Regression: "Rename…" in the tab / pane-label context menu did nothing.
    //!
    //! A menu pick runs the picked command and then dismisses the menu, both in the same
    //! click (`App::on_ctx_pick`). `dispatch`'s cancel guards treat every command other
    //! than the rename pair as "the user interacted elsewhere" and clear the editor — so
    //! the `CloseContext` that closed the menu also cancelled the rename the pick had
    //! just started, one command later. The inline edit box never appeared.
    use super::*;
    use avada_core::session_manager::SessionManager;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    /// Replay the menu pick exactly as `on_ctx_pick` does: run the row's command, then
    /// close the menu. The editor must still be open afterwards.
    #[test]
    fn picking_rename_from_the_tab_menu_leaves_the_editor_open() {
        let mgr = mgr();
        let mut st = fresh();
        st.open_tab_context(0, 0.0, 0.0);

        dispatch(&mut st, Command::BeginRename(0), &mgr);
        assert_eq!(st.editing_tab, 0, "the pick itself opens the editor");
        dispatch(&mut st, Command::CloseContext, &mgr);
        assert_eq!(st.editing_tab, 0, "dismissing the menu must not cancel it");
    }

    // Needs a reactor: `add_pane` spawns the pty read loop.
    #[tokio::test]
    async fn picking_rename_from_the_pane_menu_leaves_the_editor_open() {
        let mgr = mgr();
        let mut st = fresh();
        st.add_pane(&mgr);
        st.open_pane_context(0, 0.0, 0.0);

        dispatch(&mut st, Command::BeginRenamePane(0), &mgr);
        assert_eq!(st.editing_pane, 0, "the pick itself opens the editor");
        dispatch(&mut st, Command::CloseContext, &mgr);
        assert_eq!(st.editing_pane, 0, "dismissing the menu must not cancel it");
    }

    /// The pane microphone resolves the clicked INDEX to the pane's session uid before it
    /// leaves `dispatch`: the `ControlHost` above `State` keys dictation by session, and a
    /// row index would go stale the moment a pane is closed or dragged elsewhere.
    #[tokio::test]
    async fn the_microphone_bubbles_up_a_session_uid_not_a_row_index() {
        let mgr = mgr();
        let mut st = fresh();
        st.add_pane(&mgr);
        let uid = st.active_tab().panes[0].uid.clone();

        match dispatch(&mut st, Command::ToggleDictation(0), &mgr) {
            Effect::ToggleDictation(got) => assert_eq!(got, uid),
            other => panic!("expected a dictation effect, got {other:?}"),
        }
    }

    /// A stale index (the pane closed between the click and the dispatch) must be a quiet
    /// no-op — never a panic, and never a microphone opened on the wrong pane.
    #[test]
    fn the_microphone_on_a_pane_that_is_gone_does_nothing() {
        let mgr = mgr();
        let mut st = fresh();
        assert!(matches!(
            dispatch(&mut st, Command::ToggleDictation(9), &mgr),
            Effect::None
        ));
    }

    /// A view pane is read-only: there is no pty for a transcript to be typed into (D3),
    /// so the microphone is absent from its header and its menu. This is the belt to that
    /// braces — the command itself refuses, whoever reaches it.
    #[test]
    fn the_microphone_on_a_read_only_pane_does_nothing() {
        let mgr = mgr();
        let mut st = fresh();
        st.add_pane_opts(
            &mgr,
            NewPaneOpts {
                kind: Some(avada_core::tools::PaneKind::Markdown),
                ..Default::default()
            },
        )
        .expect("view pane added");
        assert!(matches!(
            dispatch(&mut st, Command::ToggleDictation(0), &mgr),
            Effect::None
        ));
    }

    /// The guards still do their job: anything that isn't the rename pair (or the menu
    /// dismissal that rides along with the pick) closes a lingering editor.
    #[test]
    fn an_unrelated_command_still_cancels_an_open_editor() {
        let mgr = mgr();
        let mut st = fresh();
        dispatch(&mut st, Command::BeginRename(0), &mgr);
        assert_eq!(st.editing_tab, 0);
        dispatch(&mut st, Command::OpenTabContext(0, 0.0, 0.0), &mgr);
        assert_eq!(st.editing_tab, -1);
    }
}

#[cfg(test)]
mod restart_rebinds_the_control_alias_tests {
    //! Regression: "Restart" / "Refresh Env" from the pane menu swapped the pane's session
    //! for a fresh one but threw the `(old, new)` uid pair away, so the control-plane pane-id
    //! alias kept pointing at the dead session and was pruned — the pane got a NEW id
    //! mid-conversation. The exit-fallback path rebinds; this one must too.
    use super::*;
    use avada_core::session_manager::SessionManager;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    #[tokio::test]
    async fn restart_pane_bubbles_the_uid_swap() {
        let mgr = mgr();
        let mut st = fresh();
        st.add_pane(&mgr);
        let before = st.active_tab().panes[0].uid.clone();

        match dispatch(&mut st, Command::RestartPane(0), &mgr) {
            Effect::Rebound(old, new) => {
                assert_eq!(
                    old, before,
                    "old side of the swap is the pane's previous uid"
                );
                assert_eq!(
                    new,
                    st.active_tab().panes[0].uid,
                    "new side is what the pane holds now"
                );
                assert_ne!(old, new);
            }
            other => panic!("expected a rebound effect, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_env_bubbles_the_uid_swap() {
        let mgr = mgr();
        let mut st = fresh();
        st.add_pane(&mgr);
        let before = st.active_tab().panes[0].uid.clone();

        match dispatch(&mut st, Command::RefreshEnvPane(0), &mgr) {
            Effect::Rebound(old, new) => {
                assert_eq!(old, before);
                assert_eq!(new, st.active_tab().panes[0].uid);
            }
            other => panic!("expected a rebound effect, got {other:?}"),
        }
    }

    /// A stale index (the pane closed between the click and the dispatch) is a quiet no-op.
    #[test]
    fn restarting_a_pane_that_is_gone_is_a_no_op() {
        let mgr = mgr();
        let mut st = fresh();
        assert!(matches!(
            dispatch(&mut st, Command::RestartPane(7), &mgr),
            Effect::None
        ));
        assert!(matches!(
            dispatch(&mut st, Command::RefreshEnvPane(7), &mgr),
            Effect::None
        ));
    }
}

#[cfg(test)]
mod git_diff_tests {
    //! End-to-end over "Show Diff": a real repository, a real row origin, the real menu
    //! builder, the real pick dispatch. Every link in this chain type-checks on its own and
    //! the feature still misbehaved, which is exactly the seam a per-function unit test
    //! cannot see. These tests never skip — a machine without git is a failure, not a pass,
    //! because a silently-skipped test is indistinguishable from a green one.
    //!
    //! The rows themselves now come from `bshuler/avada-git`, so what is under test here is
    //! the half the host kept: a `data.git` object turning into a menu row, and that row
    //! turning into a pane running the right git command.
    use super::*;
    use crate::state::GitOrigin;
    use avada_core::session_manager::SessionManager;
    use std::path::PathBuf;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    /// Guards a throwaway repo so a panicking assert still cleans up.
    struct Repo {
        root: PathBuf,
        hash: String,
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A repo with one commit touching a file in a subdirectory. `tag` keeps parallel
    /// tests off each other's directory.
    fn repo(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("hp-commitdiff-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("temp dir");
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("sub/a.txt"), "one\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "the subject"]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(!hash.is_empty(), "no commit hash");
        // The service resolves through `git rev-parse --show-toplevel`, which on macOS
        // answers the real path (/private/var/...), not the symlinked /var/... one.
        let root = std::fs::canonicalize(&dir).expect("canonicalize");
        Repo { root, hash }
    }

    /// Open the host's row menu over `rel` as the git module would have described it, and
    /// return the labels plus the state, so each test asserts on one menu.
    fn menu_over(st: &mut State, r: &Repo, rel: &str, rev: Option<&str>) -> Vec<String> {
        let origin = GitOrigin {
            root: r.root.clone(),
            rev: rev.map(str::to_string),
            short: rev.map(|h| h[..7].to_string()).unwrap_or_default(),
        };
        let abs = if rel.is_empty() {
            r.root.clone()
        } else {
            r.root.join(rel)
        };
        st.open_file_context_git(&abs, Some(origin), 10.0, 10.0);
        st.ctx
            .as_ref()
            .expect("the row menu opened")
            .entries
            .iter()
            .map(|e| e.label.to_string())
            .collect()
    }

    fn spawned(st: &State) -> String {
        st.active_tab()
            .panes
            .last()
            .expect("the pick opened a pane")
            .spawn_command
            .clone()
            .unwrap_or_default()
    }

    /// Right-click a file row the module listed for a commit, pick "Show Diff in <short>",
    /// and land in a pane running `git show` scoped to that one file.
    #[tokio::test]
    async fn show_diff_on_a_commit_row_opens_a_pane_running_git_show() {
        let r = repo("row");
        let mgr = mgr();
        let mut st = fresh();

        let labels = menu_over(&mut st, &r, "sub/a.txt", Some(&r.hash));
        let row = labels
            .iter()
            .position(|l| l.starts_with("Show Diff in"))
            .unwrap_or_else(|| panic!("no Show Diff row; menu was {labels:?}"));

        let cmd = st.ctx_command(row).expect("the row carries a command");
        dispatch(&mut st, cmd, &mgr);

        let out = spawned(&st);
        assert!(
            out.contains("show") && out.contains(&r.hash) && out.contains("sub/a.txt"),
            "the pane must run git show for this commit and file, got {out:?}"
        );
    }

    /// The repository-root row is the whole-commit variant of the same verb — which is
    /// where the header button's job went when the header left with the panel.
    #[tokio::test]
    async fn the_root_row_opens_the_whole_commit_diff() {
        let r = repo("head");
        let mgr = mgr();
        let mut st = fresh();
        let labels = menu_over(&mut st, &r, "", Some(&r.hash));
        let row = labels
            .iter()
            .position(|l| l.starts_with("Show Diff in"))
            .unwrap_or_else(|| panic!("no Show Diff row; menu was {labels:?}"));
        let cmd = st.ctx_command(row).expect("a command");
        dispatch(&mut st, cmd, &mgr);
        let out = spawned(&st);
        assert!(
            out.contains("show") && out.contains(&r.hash) && !out.contains(" -- "),
            "got {out:?}"
        );
    }

    /// The bug the user hit. They opened the panel on a dirty tree — no commit — and there
    /// was no diff anywhere: not on the header, not on a row.
    #[tokio::test]
    async fn show_diff_on_a_working_tree_row_opens_a_pane_running_git_diff() {
        let r = repo("work");
        std::fs::write(r.root.join("sub/a.txt"), "changed\n").expect("dirty the tree");
        let mgr = mgr();
        let mut st = fresh();

        let labels = menu_over(&mut st, &r, "sub/a.txt", None);
        let row = labels
            .iter()
            .position(|l| l == "Show Diff")
            .unwrap_or_else(|| panic!("no Show Diff row; menu was {labels:?}"));

        let cmd = st.ctx_command(row).expect("a command");
        dispatch(&mut st, cmd, &mgr);
        let out = spawned(&st);
        assert!(
            out.contains("diff") && out.contains("HEAD") && out.contains("sub/a.txt"),
            "the pane should run git diff HEAD on that one file; got {out:?}"
        );
    }

    #[tokio::test]
    async fn the_working_tree_root_row_opens_the_whole_diff() {
        let r = repo("workhead");
        std::fs::write(r.root.join("sub/a.txt"), "changed\n").expect("dirty the tree");
        let mgr = mgr();
        let mut st = fresh();
        let labels = menu_over(&mut st, &r, "", None);
        let row = labels
            .iter()
            .position(|l| l == "Show Diff")
            .unwrap_or_else(|| panic!("no Show Diff row; menu was {labels:?}"));
        let cmd = st.ctx_command(row).expect("a command");
        dispatch(&mut st, cmd, &mgr);
        let out = spawned(&st);
        assert!(
            out.contains("diff") && out.contains("HEAD") && !out.contains(" -- "),
            "the root row diffs the whole tree; got {out:?}"
        );
    }

    /// An untracked file has nothing in HEAD to compare against, so the row must stay off
    /// the menu rather than opening a pane that prints nothing. Answered by asking git,
    /// not by trusting the module's row: a module can say anything, and this one spawns a
    /// process.
    #[tokio::test]
    async fn an_untracked_file_is_offered_no_diff() {
        let r = repo("untracked");
        std::fs::write(r.root.join("sub/new.txt"), "brand new\n").expect("an untracked file");
        let mut st = fresh();
        let labels = menu_over(&mut st, &r, "sub/new.txt", None);
        assert!(
            !labels.iter().any(|l| l == "Show Diff"),
            "an untracked file must not offer a diff; menu was {labels:?}"
        );
    }

    /// A row with no `git` object at all — every other module's rows — gets the ordinary
    /// file menu and no diff verb.
    #[tokio::test]
    async fn a_row_without_a_git_origin_is_offered_no_diff() {
        let r = repo("plain");
        let mut st = fresh();
        st.open_file_context(&r.root.join("sub/a.txt"), 10.0, 10.0);
        let labels: Vec<String> = st
            .ctx
            .as_ref()
            .expect("the row menu opened")
            .entries
            .iter()
            .map(|e| e.label.to_string())
            .collect();
        assert!(
            !labels.iter().any(|l| l.starts_with("Show Diff")),
            "menu was {labels:?}"
        );
    }

    /// The parse the whole feature hangs off: what the module actually puts on the wire.
    #[test]
    fn a_row_git_object_becomes_an_origin() {
        let o = GitOrigin::from_row(Some(&serde_json::json!({
            "root": "/r", "rev": "0123456789abcdef", "short": "0123456",
        })))
        .expect("a complete object parses");
        assert_eq!(o.root, PathBuf::from("/r"));
        assert_eq!(o.rev.as_deref(), Some("0123456789abcdef"));
        assert_eq!(o.short, "0123456");

        let w = GitOrigin::from_row(Some(&serde_json::json!({"root": "/r", "rev": null})))
            .expect("the working tree needs only a root");
        assert_eq!(w.rev, None);

        // A rev with no short form still needs a name in the menu.
        let s = GitOrigin::from_row(Some(
            &serde_json::json!({"root": "/r", "rev": "abcdef123456"}),
        ))
        .expect("parses");
        assert_eq!(s.short, "abcdef1");

        assert_eq!(GitOrigin::from_row(None), None, "no object, no verb");
        assert_eq!(
            GitOrigin::from_row(Some(&serde_json::json!({"rev": "abc"}))),
            None,
            "no root means no repository to run anything in"
        );
    }
}

#[cfg(test)]
mod rail_command_tests {
    //! Track H4: the three rail commands, one test per `dispatch` arm.
    //!
    //! Deleting any arm below makes exactly one of these fail — the arm is the only thing
    //! standing between a click on a module's button and a panel that does nothing.
    use super::*;
    use crate::leftpanel::{entry_key, RailGesture, RailRequest};
    use avada_core::module::{RailEntry, RailEvent, Row};
    use avada_core::session_manager::SessionManager;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn module() -> avada_core::rights::ModuleId {
        // The first-party marketplace: a module id IS its GitHub `owner/repo`.
        avada_core::rights::ModuleId::new("bshuler/avada-marketplace").unwrap()
    }

    /// The wire payload of `host.rail.register`; `tier` is the SDK's `UiTier`, which this
    /// crate cannot name.
    fn entry(id: &str) -> RailEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "label": id.to_uppercase(), "tier": 1, "order": 0
        }))
        .unwrap()
    }

    fn row(id: &str) -> Row {
        Row {
            id: id.into(),
            label: id.into(),
            detail: String::new(),
            depth: 0,
            expandable: true,
            expanded: false,
            icon: None,
            marks: vec![],
            data: serde_json::json!({ "page": 2 }),
        }
    }

    /// A window whose panel already has one module entry with one row under it.
    fn with_a_module() -> State {
        let mut st = State::new(theme::load_font(1.0));
        st.apply_rail_event(RailEvent::Registered {
            module: module(),
            entries: vec![entry("browse")],
        });
        st.apply_rail_event(RailEvent::Rows {
            target: avada_core::module::RowTarget::Rail,
            module: module(),
            entry: "browse".into(),
            rows: vec![row("installed")],
        });
        st
    }

    #[test]
    fn rail_activate_selects_the_entry_and_asks_the_host_for_it() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");

        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        assert_eq!(st.rail.active.as_deref(), Some(key.as_str()));
        assert!(st.left_panel_open, "activating opens the panel it draws in");
        assert_eq!(
            st.take_rail_requests(),
            vec![RailRequest::Activate {
                module: module(),
                entry: "browse".into()
            }]
        );
    }

    /// A button that was unregistered between the paint and the click must be a dropped
    /// click, not a blank panel and not a request to a module that is gone.
    #[test]
    fn rail_activate_on_an_unknown_key_asks_for_nothing() {
        let mgr = mgr();
        let mut st = with_a_module();
        dispatch(
            &mut st,
            Command::RailActivate("bshuler/avada-marketplace#ghost".into()),
            &mgr,
        );
        assert!(st.rail.active.is_none());
        assert!(st.take_rail_requests().is_empty());
    }

    /// `RailBack` clears the active entry and nothing else. That IS the panel going back
    /// to its own frame now: the frame is what an empty `rail.active` draws, so there is no
    /// second piece of view state that could disagree with it.
    #[test]
    fn rail_back_leaves_the_module_and_that_is_the_whole_of_it() {
        let mgr = mgr();
        let mut st = with_a_module();
        dispatch(
            &mut st,
            Command::RailActivate(entry_key(&module(), "browse")),
            &mgr,
        );

        dispatch(&mut st, Command::RailBack, &mgr);
        assert!(st.rail.active.is_none());
        assert!(
            st.left_panel_open,
            "leaving an entry must not also close the panel"
        );
    }

    #[test]
    fn rail_row_carries_the_rows_own_payload_and_gesture_to_the_host() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");
        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        let _ = st.take_rail_requests();

        dispatch(
            &mut st,
            Command::RailRow(key.clone(), "installed".into(), "toggle".into()),
            &mgr,
        );
        assert_eq!(
            st.take_rail_requests(),
            vec![RailRequest::Row {
                module: module(),
                entry: "browse".into(),
                row: "installed".into(),
                target: avada_core::module::RowTarget::Rail,
                data: serde_json::json!({ "page": 2 }),
                gesture: RailGesture::Toggle,
            }],
            "the module hung `data` off the row precisely so it need keep no row table"
        );
    }

    /// A module's pane is its second row surface, and a click there goes back to it the
    /// same way a rail click does. `target` is the whole difference — without it the
    /// module could not tell which of its two surfaces the user touched, and a module
    /// that uses one contribution id for both (the marketplace does) would answer the
    /// wrong one.
    #[test]
    fn a_pane_row_reaches_the_module_tagged_as_a_pane_row() {
        let mgr = mgr();
        let mut st = with_a_module();
        crate::module_ui::rows::set(
            &module(),
            "market",
            vec![avada_core::module::Row {
                id: "installed".into(),
                label: "Installed".into(),
                detail: String::new(),
                depth: 0,
                expandable: true,
                expanded: false,
                icon: None,
                marks: Vec::new(),
                data: serde_json::json!({ "page": 2 }),
            }],
        );
        let _ = st.take_rail_requests();

        dispatch(
            &mut st,
            Command::ModulePaneRow(
                entry_key(&module(), "market"),
                "installed".into(),
                "toggle".into(),
            ),
            &mgr,
        );
        assert_eq!(
            st.take_rail_requests(),
            vec![RailRequest::Row {
                module: module(),
                entry: "market".into(),
                row: "installed".into(),
                target: avada_core::module::RowTarget::Pane,
                data: serde_json::json!({ "page": 2 }),
                gesture: RailGesture::Toggle,
            }],
            "the payload comes from the pane store, not from the click"
        );
    }

    /// The three ways a pane-row click can be nonsense: a key that names no module, a
    /// gesture the `.slint` spelled wrong, and a row the module never sent. Each is a
    /// dropped click — never a request built out of a guess.
    #[test]
    fn a_nonsense_pane_row_click_sends_nothing() {
        let mgr = mgr();
        let mut st = with_a_module();
        crate::module_ui::rows::set(&module(), "market", Vec::new());
        let key = entry_key(&module(), "market");
        let _ = st.take_rail_requests();

        for cmd in [
            Command::ModulePaneRow("no-hash".into(), "installed".into(), "open".into()),
            Command::ModulePaneRow(key.clone(), "installed".into(), "wiggle".into()),
            Command::ModulePaneRow(key, "no-such-row".into(), "open".into()),
        ] {
            dispatch(&mut st, cmd, &mgr);
            assert!(st.take_rail_requests().is_empty());
        }
    }

    /// A gesture spelled wrong in the `.slint` is a dropped click, never a different
    /// gesture sent to a module — `context` and `open` mean very different things.
    #[test]
    fn an_unknown_gesture_sends_nothing() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");
        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        let _ = st.take_rail_requests();

        dispatch(
            &mut st,
            Command::RailRow(key.clone(), "installed".into(), "wiggle".into()),
            &mgr,
        );
        dispatch(
            &mut st,
            Command::RailRow(key, "no-such-row".into(), "open".into()),
            &mgr,
        );
        assert!(st.take_rail_requests().is_empty());
    }

    /// The filter box. A tier-1 module cannot draw a text field, so the panel draws one and
    /// forwards every edit as a `rail.query` host event — the whole box, not a keystroke.
    ///
    /// Notified, never applied: the host must NOT filter the rows it already holds. A file
    /// explorer's matches usually are not loaded yet, so a host-side filter would silently
    /// turn "find in project" into "find among the folders you already opened".
    #[test]
    fn the_filter_box_forwards_the_typed_query_and_filters_nothing_itself() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");
        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        let before = st.rail.rows(&key).len();
        let _ = st.take_module_events();

        dispatch(&mut st, Command::RailQuery("inst".into()), &mgr);

        assert_eq!(
            st.take_module_events(),
            vec![(
                avada_core::module::methods::events::RAIL_QUERY.to_string(),
                serde_json::json!({ "entry": "browse", "query": "inst" })
            )],
            "the entry is named in the payload: one event kind serves every rail entry"
        );
        assert_eq!(
            st.rail.rows(&key).len(),
            before,
            "the rows only ever change when the module sends new ones"
        );
    }

    /// Typing with nothing active is not an error, but it must not invent an entry to
    /// address — an event with the wrong `entry` would filter somebody else's list.
    #[test]
    fn typing_with_no_active_entry_sends_nothing() {
        let mgr = mgr();
        let mut st = with_a_module();
        dispatch(&mut st, Command::RailQuery("inst".into()), &mgr);
        assert!(st.take_module_events().is_empty());
    }

    /// A right-click has two destinations at once (`docs/module-contract.md` §10.6): the
    /// module hears the gesture, AND the host opens its own file menu over the row's
    /// `data.path`. That is how a tier-1 module inherits the app's whole "Open in…" list
    /// without shipping one menu row of its own.
    #[test]
    fn a_right_click_tells_the_module_and_opens_the_hosts_own_file_menu() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");
        // A real path, because the menu is built from what is actually on disk.
        let here = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        st.apply_rail_event(RailEvent::Rows {
            target: avada_core::module::RowTarget::Rail,
            module: module(),
            entry: "browse".into(),
            rows: vec![Row {
                data: serde_json::json!({ "path": here }),
                ..row("manifest")
            }],
        });
        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        let _ = st.take_rail_requests();

        dispatch(
            &mut st,
            Command::RailContext(key.clone(), "manifest".into(), 12.0, 34.0),
            &mgr,
        );

        assert_eq!(
            st.take_rail_requests(),
            vec![RailRequest::Row {
                module: module(),
                entry: "browse".into(),
                row: "manifest".into(),
                target: avada_core::module::RowTarget::Rail,
                data: serde_json::json!({ "path": here }),
                gesture: RailGesture::Context,
            }],
            "the module is told even though the host also drew a menu"
        );
        let menu = st.ctx.as_ref().expect("the host opened its own row menu");
        let labels: Vec<String> = menu.entries.iter().map(|e| e.label.to_string()).collect();
        assert!(
            labels.iter().any(|l| l == "Open in Terminal"),
            "this list is the app's file menu, unchanged: {labels:?}"
        );
    }

    /// A row with no `path` still reaches the module. The host simply has nothing to draw a
    /// file menu over — and must not draw an empty one, which would look like a hang.
    #[test]
    fn a_right_click_on_a_row_without_a_path_reaches_the_module_and_opens_no_menu() {
        let mgr = mgr();
        let mut st = with_a_module();
        let key = entry_key(&module(), "browse");
        dispatch(&mut st, Command::RailActivate(key.clone()), &mgr);
        let _ = st.take_rail_requests();

        dispatch(
            &mut st,
            Command::RailContext(key, "installed".into(), 12.0, 34.0),
            &mgr,
        );

        assert_eq!(
            st.take_rail_requests().len(),
            1,
            "the module still hears it"
        );
        assert!(
            st.ctx.is_none(),
            "and no menu opens over a row with no path"
        );
    }

    /// Reveal-in-files, the one thing the deleted built-in mode did that nothing else in
    /// the app can do: a `path:line:col` hit in a pane opens the explorer ON that path.
    ///
    /// The host does not walk a tree any more — it opens the panel on whatever entry is
    /// called `files` and emits `files.reveal`; the module expands the ancestors and marks
    /// the row. Matching on the ENTRY id, not the module id, is deliberate: a fork that
    /// registers `files` inherits every reveal in the app.
    #[test]
    fn a_reveal_opens_the_files_entry_and_emits_files_reveal() {
        let mgr = mgr();
        let mut st = State::new(theme::load_font(1.0));
        st.apply_rail_event(RailEvent::Registered {
            module: module(),
            entries: vec![entry("files")],
        });
        let _ = st.take_module_events();

        let here = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        dispatch(
            &mut st,
            Command::RevealInFiles {
                path: here.into(),
                line: Some(12),
                col: None,
            },
            &mgr,
        );

        assert_eq!(
            st.rail.active.as_deref(),
            Some(entry_key(&module(), "files").as_str()),
            "a reveal has to bring the explorer to the front, not just message it"
        );
        assert!(st.left_panel_open);

        let events = st.take_module_events();
        let (kind, payload) = events
            .iter()
            .find(|(k, _)| k == avada_core::module::methods::events::FILES_REVEAL)
            .expect("the reveal was emitted");
        assert_eq!(kind, "files.reveal");
        assert_eq!(payload["path"], serde_json::json!(here));
        assert_eq!(payload["line"], serde_json::json!(12));
    }

    /// No files module installed: there is nothing to reveal *into*. Say so, rather than
    /// opening a panel that does nothing — a silent no-op reads as a broken link.
    #[test]
    fn a_reveal_with_no_files_module_tells_the_human_instead_of_going_quiet() {
        let mgr = mgr();
        let mut st = with_a_module();
        dispatch(
            &mut st,
            Command::RevealInFiles {
                path: "/tmp/x.rs".into(),
                line: None,
                col: None,
            },
            &mgr,
        );
        assert!(st.take_module_events().is_empty());
        assert!(st.rail.active.is_none());
    }

    /// The module crashed while its surface was showing: the panel must go back to a
    /// built-in view rather than sit on a head with nothing behind it.
    #[test]
    fn a_module_going_away_takes_the_panel_off_its_surface() {
        let mgr = mgr();
        let mut st = with_a_module();
        dispatch(
            &mut st,
            Command::RailActivate(entry_key(&module(), "browse")),
            &mgr,
        );

        st.apply_rail_event(RailEvent::Gone { module: module() });
        assert!(st.rail.active.is_none());
        assert!(st.rail.entries().is_empty());
    }
}

/// Track H2: the one `dispatch` arm the rights page and the ask toast share.
///
/// The page's nine callbacks are decoded by `prefs::rights::wire` and tested there; what
/// this covers is the half `State` owns — the service the decision is written against, and
/// the queue an answered ask leaves behind for the module host that is blocked on it.
#[cfg(test)]
mod rights_command_tests {
    use super::*;
    use crate::prefs::rights::{Applied, RightsCommand};
    use avada_core::rights::{Capability, RightValue, RightsService};

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    /// A window whose rights service is rooted in a throw-away directory. Never the real
    /// app-support root: these tests write rights files.
    fn with_a_temp_rights_root(tag: &str) -> (State, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("avada-rights-cmd-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = State::new(theme::load_font(1.0));
        st.rights = RightsService::with_root(&dir);
        (st, dir)
    }

    fn module() -> avada_core::rights::ModuleId {
        avada_core::rights::ModuleId::new("bshuler/avada-marketplace").unwrap()
    }

    /// Answering the toast writes the user column AND hands the decision back, because a
    /// module is sitting blocked on the answer: writing the file alone would leave it
    /// waiting forever.
    #[test]
    fn answering_an_ask_writes_the_column_and_queues_the_decision_for_the_host() {
        let (mut st, dir) = with_a_temp_rights_root("answer");
        let id = st.rights.ask(&module(), Capability::WorkspaceRead, None);

        dispatch(
            &mut st,
            Command::Rights(RightsCommand::AskAllowAlways(id)),
            &mgr(),
        );

        assert_eq!(
            st.rights.user_value(&module(), Capability::WorkspaceRead),
            RightValue::Always
        );
        let queued = st.take_rights_effects();
        assert_eq!(queued.len(), 1, "the host never heard the answer");
        assert!(matches!(queued[0], Applied::Answered { .. }));
        assert!(st.take_rights_effects().is_empty(), "the drain kept a copy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A picker click writes and nothing more: there is no module to tell, so the host
    /// queue stays empty and a resync is all that is owed.
    #[test]
    fn a_picker_click_writes_without_queueing_anything() {
        let (mut st, dir) = with_a_temp_rights_root("picker");
        dispatch(
            &mut st,
            Command::Rights(RightsCommand::SetUser {
                module: module(),
                cap: Capability::WorkspaceRead,
                value: RightValue::Never,
            }),
            &mgr(),
        );

        assert_eq!(
            st.rights.user_value(&module(), Capability::WorkspaceRead),
            RightValue::Never
        );
        assert!(st.take_rights_effects().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An id nobody is waiting on (already answered, or dropped when its module left)
    /// changes nothing — it must not fall through to a write or a phantom relay.
    #[test]
    fn answering_an_ask_that_is_already_gone_does_nothing() {
        let (mut st, dir) = with_a_temp_rights_root("stale");
        dispatch(
            &mut st,
            Command::Rights(RightsCommand::AskDeny(4242)),
            &mgr(),
        );
        assert_eq!(
            st.rights.user_value(&module(), Capability::WorkspaceRead),
            RightValue::Ask
        );
        assert!(st.take_rights_effects().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
