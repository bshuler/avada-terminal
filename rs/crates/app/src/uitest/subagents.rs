//! Track B2: subagent panes end to end — a Claude session's subagents on disk become panes
//! under their orchestrator, the TopBar toggle hides and shows them, and it breathes while
//! one works. `super::*` brings the harness helpers into scope.
//!
//! Discovery reads a real directory pair (the hook directory and the session's
//! `subagents/` transcript directory), so a format drift fails here rather than live.
#![allow(unused_imports)]

use super::*;

/// A fake session with two subagents — one the hook recorded, one only the transcript
/// directory knows — under an orchestrator pane: reconcile opens exactly two subagent panes
/// right after it, clicking Hide takes their tiles away and grows the orchestrator's, the
/// button keeps breathing while hidden, Show brings both back, and once both stop it goes still.
#[test]
fn a_sessions_subagents_open_beside_the_orchestrator_and_the_toggle_hides_them() {
    use crate::state::{DetachedPane, State, SubagentScan};
    use avada_core::claude_subagents::subagents_from;
    use avada_core::tools::PaneKind;
    use slint::Model;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    let root = std::env::temp_dir().join(format!("hp-uitest-subagents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let hook_dir = root.join("claude-subagents/pane-orch");
    let transcripts = root.join("projects/-work/sess-1/subagents");
    std::fs::create_dir_all(&hook_dir).unwrap();
    std::fs::create_dir_all(&transcripts).unwrap();
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    // a1: the hook saw it start; its meta names it.
    let a1 = transcripts.join("agent-a1.jsonl");
    std::fs::write(&a1, "").unwrap();
    std::fs::write(
        transcripts.join("agent-a1.meta.json"),
        r#"{"agentType":"Explore","description":"map the repo","spawnDepth":1}"#,
    )
    .unwrap();
    std::fs::write(
        hook_dir.join("a1.json"),
        serde_json::json!({
            "agentId": "a1",
            "agentType": "Explore",
            "transcript": a1.to_string_lossy(),
            "startedAt": now_ms - 2000,
        })
        .to_string(),
    )
    .unwrap();
    // a2: no hook record — the transcript-directory fallback finds it.
    std::fs::write(transcripts.join("agent-a2.jsonl"), "").unwrap();
    std::fs::write(
        transcripts.join("agent-a2.meta.json"),
        r#"{"agentType":"general-purpose","description":"write the test","spawnDepth":1}"#,
    )
    .unwrap();

    let (hook, tr) = (hook_dir.clone(), transcripts.clone());
    ui(move || {
        let w = window();
        let models = crate::paneview::Ui::new();
        models.attach(&w);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = avada_core::session_manager::SessionManager::new(tx);
        let state = Rc::new(RefCell::new(State::new(crate::theme::load_font(1.0))));
        state.borrow_mut().adopt_pane(
            &mgr,
            DetachedPane {
                uid: "orch".into(),
                title: "orch".into(),
                subtitle: None,
                pinned_accent: None,
                show_frame: None,
                show_dot: None,
                font_px: 14.0,
                spawn_command: None,
                spawn_args: None,
                spawn_shell: None,
                kind: PaneKind::Tool("claude".into()),
                tool_session: None,
                origin: Default::default(),
                parent_uid: None,
                subagent: None,
                cwd: None,
            },
        );

        let scan = |stopped: bool| {
            let mut records = subagents_from(&hook, Some(&tr));
            if stopped {
                for r in &mut records {
                    r.stopped_at = Some(SystemTime::now());
                }
            }
            SubagentScan { records }
        };
        let t0 = Instant::now();
        let opened = state
            .borrow_mut()
            .reconcile_subagents(&mgr, t0, |uid| (uid == "orch").then(|| scan(false)));
        assert!(opened, "two subagents on disk must open panes");

        let rows = |st: &State| -> Vec<(String, Option<String>, bool)> {
            st.active_tab()
                .panes
                .iter()
                .map(|p| {
                    (
                        p.uid.clone(),
                        p.parent_uid.clone(),
                        p.kind == PaneKind::Subagent,
                    )
                })
                .collect()
        };
        assert_eq!(
            rows(&state.borrow()),
            vec![
                ("orch".to_string(), None, false),
                ("sub:a1".to_string(), Some("orch".to_string()), true),
                ("sub:a2".to_string(), Some("orch".to_string()), true),
            ],
            "exactly two subagent panes, right after their orchestrator, in start order"
        );
        let titles: Vec<String> = state.borrow().active_tab().panes[1..]
            .iter()
            .map(|p| p.title.to_string())
            .collect();
        assert_eq!(titles, ["orch › map the repo", "orch › write the test"]);

        {
            let state = state.clone();
            w.on_toggle_subagents(move || state.borrow_mut().toggle_subagents());
        }
        let project = |at: Instant| -> Vec<(String, bool, f32)> {
            let mut st = state.borrow_mut();
            crate::paneview::resync(&mut st, &w, &models, (1280.0, 800.0), 1.0, &mgr);
            crate::paneview::drive_subagent_breath(&w, &mut st, at);
            (0..models.panes.row_count())
                .filter_map(|i| models.panes.row_data(i))
                .map(|p| (p.uid.to_string(), p.hidden, p.w))
                .collect()
        };
        let breaths = |from: u64| -> Vec<f32> {
            [0u64, 400, 800, 1100]
                .iter()
                .map(|ms| {
                    let at = t0 + Duration::from_millis(from + ms);
                    crate::paneview::drive_subagent_breath(&w, &mut state.borrow_mut(), at);
                    w.get_subagent_breath()
                })
                .collect()
        };
        let moving = |b: &[f32]| {
            let mut d = b.to_vec();
            d.dedup_by(|a, b| (*a - *b).abs() < 0.01);
            d.len() >= 3 && b.iter().all(|x| *x > 0.0)
        };

        let shown = project(t0);
        assert_eq!(shown.iter().filter(|p| !p.1).count(), 3, "all three tiled: {shown:?}");
        let orch_w = shown[0].2;
        let hide = "Hide this tab's subagent panes. Drag to move the window";
        let show = "Show this tab's subagent panes. Drag to move the window";
        let b = by_label(&w, hide);
        assert_eq!(b.len(), 1, "a tab with subagent panes offers Hide");
        click(&w, &b[0]);
        assert!(state.borrow().active_tab().hide_subagents, "the click must reach the tab");

        let hidden = project(t0 + Duration::from_millis(2000));
        assert_eq!(
            hidden.iter().map(|p| p.1).collect::<Vec<_>>(),
            [false, true, true],
            "both subagent panes lose their tiles: {hidden:?}"
        );
        assert_eq!(state.borrow().active_tab().visible_layout().indices, [0]);
        assert!(hidden[0].2 > orch_w, "the orchestrator takes the room: {orch_w} → {}", hidden[0].2);
        let b = breaths(2000);
        assert!(moving(&b), "the button breathes while its subagents are hidden: {b:?}");

        let s = by_label(&w, show);
        assert_eq!(s.len(), 1, "hidden subagents offer Show");
        click(&w, &s[0]);
        let back = project(t0 + Duration::from_millis(4000));
        assert_eq!(back.iter().filter(|p| !p.1).count(), 3, "both back: {back:?}");
        assert!((back[0].2 - orch_w).abs() < 0.5, "the orchestrator gives the room back");

        // Both stopped: finished panes close at once (Bert, 2026-10-10), whatever the
        // orchestrator's turn is doing; the orchestrator keeps the tab.
        state.borrow_mut().reconcile_subagents(&mgr, t0 + Duration::from_secs(5), |uid| {
            (uid == "orch").then(|| scan(true))
        });
        assert_eq!(state.borrow().active_tab().panes.len(), 1, "both finished panes closed");
        let still = breaths(6000);
        assert!(
            still.iter().all(|x| *x == 0.0),
            "no working subagent, no breath: {still:?}"
        );
    });
    let _ = std::fs::remove_dir_all(&root);
}
