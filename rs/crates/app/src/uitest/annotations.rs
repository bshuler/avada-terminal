//! Track V4: every row carries an accessibility annotation (the raw walk).
//! Owned by the V4 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! Two kinds of test live here. The lookup tests reach one control of each kind the way
//! a screen reader would — by its label — and read its state back (a directory row says
//! whether it is open, a git row names its path and status, a grid cell announces its
//! text). The raw walk is the net under them: it enumerates every `TouchArea` in the
//! built tree of the left panel and the viewer panes, and fails naming the element when
//! one has no accessible ancestor-or-self within [`MAX_LEVELS`] levels. A row that
//! reacts to the mouse but names nothing is exactly what a reader (and this harness)
//! cannot reach; the walk is what turns that from a review remark into a red test.
#![allow(unused_imports)]

use super::*;
use i_slint_backend_testing::{AccessibleRole, ElementRoot};
use std::ops::ControlFlow;

// ===== the raw TouchArea walk =====

/// How many levels a `TouchArea` may sit below its nearest accessible ancestor. `0`
/// would demand the role on the `TouchArea` itself; `1` is a row whose root carries the
/// role and whose `TouchArea` is a direct child — every row in `leftpanel.slint` and
/// `viewpanes.slint` is built that way, so this is the smallest value that passes.
const MAX_LEVELS: usize = 1;

/// The components whose subtrees the walk judges: the left panel and the three viewer
/// panes. Everything else in the window (top bar, pane grid, sidebar, overlays) belongs
/// to other tracks; a `TouchArea` there is listed in the test output but does not fail.
const OWNED: [&str; 3] = ["LeftPanel", "ViewPane", "ImagePane"];

/// One item of the built tree, as the walk reconstructs it from a pre-order visit.
struct Node {
    id: String,
    ty: String,
    bases: Vec<String>,
    role: Option<AccessibleRole>,
    label: Option<String>,
    parent: Option<usize>,
}

impl Node {
    fn of(h: &ElementHandle, parent: Option<usize>) -> Self {
        Node {
            id: h.id().map(|s| s.to_string()).unwrap_or_default(),
            ty: h.type_name().map(|s| s.to_string()).unwrap_or_default(),
            bases: h
                .bases()
                .map(|it| it.map(|s| s.to_string()).collect())
                .unwrap_or_default(),
            role: h.accessible_role(),
            label: h.accessible_label().map(|s| s.to_string()),
            parent,
        }
    }

    /// A role other than `none`: what makes an item exist to a screen reader at all.
    fn accessible(&self) -> bool {
        self.role.is_some_and(|r| r != AccessibleRole::None)
    }

    /// A `TouchArea`, or anything built on one (`TipArea` inherits it).
    fn is_touch_area(&self) -> bool {
        self.ty == "TouchArea" || self.bases.iter().any(|b| b == "TouchArea")
    }

    fn name(&self) -> String {
        if self.id.is_empty() {
            format!("<{}>", self.ty)
        } else {
            format!("{} <{}>", self.id, self.ty)
        }
    }
}

/// How many handles a `visit_descendants` from `e` yields: the size of its subtree, not
/// counting `e` itself (nor the elements merged into `e`'s own item).
fn subtree_size(e: &ElementHandle) -> usize {
    let mut n = 0;
    e.visit_descendants(|_| {
        n += 1;
        ControlFlow::<()>::Continue(())
    });
    n
}

/// The visible item tree under `w`'s root, parent links restored.
///
/// The testing backend hands out elements in pre-order and knows no parent, so the walk
/// rebuilds the tree from subtree sizes: an item whose size is `s` owns the next `s`
/// handles. One wrinkle: the compiler merges a useless `Rectangle` into its parent's
/// item as a second *element* of the same item, visited right after it with the same
/// subtree size — a "child" larger than what its parent has left, which is how it is
/// told apart and folded into the parent node instead of becoming one.
fn collect(w: &crate::AppWindow) -> Vec<Node> {
    let root = w.root_element();
    let mut handles = Vec::new();
    root.visit_descendants(|e| {
        handles.push(e);
        ControlFlow::<()>::Continue(())
    });
    let sizes: Vec<usize> = handles.iter().map(subtree_size).collect();

    let mut nodes = vec![Node::of(&root, None)];
    // (node index, descendants still to be consumed)
    let mut stack: Vec<(usize, usize)> = vec![(0, handles.len())];
    for (h, &s) in handles.iter().zip(&sizes) {
        while stack.len() > 1 && stack.last().is_some_and(|t| t.1 == 0) {
            stack.pop();
        }
        let top = stack.len() - 1;
        if s + 1 > stack[top].1 {
            // A merged element of the item on top: its parent counted the handle, the
            // item itself did not.
            if top > 0 {
                stack[top - 1].1 -= 1;
            }
            continue;
        }
        stack[top].1 -= s + 1;
        let idx = nodes.len();
        nodes.push(Node::of(h, Some(stack[top].0)));
        stack.push((idx, s));
    }
    nodes
}

/// One `TouchArea` the walk met, with the distance to its nearest accessible
/// ancestor-or-self (`None`: there is none at all) and that ancestor's name and label.
struct Hit {
    state: &'static str,
    name: String,
    owned: bool,
    levels: Option<usize>,
    anchor: String,
}

/// Every `TouchArea` in the tree `w` shows right now.
fn touch_areas(state: &'static str, w: &crate::AppWindow) -> Vec<Hit> {
    let nodes = collect(w);
    let ancestors = |mut i: usize| {
        let mut chain = vec![i];
        while let Some(p) = nodes[i].parent {
            chain.push(p);
            i = p;
        }
        chain
    };
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.is_touch_area())
        .map(|(i, n)| {
            let chain = ancestors(i);
            let owned = chain.iter().any(|&a| OWNED.contains(&nodes[a].ty.as_str()));
            let hit = chain
                .iter()
                .enumerate()
                .find(|(_, &a)| nodes[a].accessible());
            Hit {
                state,
                name: n.name(),
                owned,
                levels: hit.map(|(d, _)| d),
                anchor: hit
                    .map(|(_, &a)| {
                        format!(
                            "{} {:?} {:?}",
                            nodes[a].name(),
                            nodes[a].role.unwrap_or(AccessibleRole::None),
                            nodes[a].label.as_deref().unwrap_or("")
                        )
                    })
                    .unwrap_or_else(|| "-".into()),
            }
        })
        .collect()
}

/// What one state does to a fresh window before the walk looks at it.
type Setup = fn(&crate::AppWindow);

/// The panel and pane states the walk covers. Slint visits only what is visible, so a
/// row inside a mode the panel is not showing is not walked; each state shows one.
fn states() -> Vec<(&'static str, Setup)> {
    vec![
        ("bare window", |_| {}),
        ("left panel: workspace", |w| {
            install_modes(w);
            let lp = w.global::<crate::LeftPanelAdapter>();
            lp.set_mode(crate::paneview::LEFT_MODE_WORKSPACE);
            let pane = |uid: &str, title: &str| crate::LeftPaneRow {
                uid: uid.into(),
                title: title.into(),
                ..Default::default()
            };
            lp.set_tabs(
                std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftTabRow {
                    title: "Tab 1".into(),
                    active: true,
                    panes: std::rc::Rc::new(slint::VecModel::from(vec![
                        pane("p1", "zsh"),
                        pane("p2", "notes.md"),
                    ]))
                    .into(),
                    ..Default::default()
                }]))
                .into(),
            );
            lp.set_workspaces(
                std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftWorkspaceRow {
                    name: "daily".into(),
                    path: "/ws/daily.json".into(),
                    detail: "2 tabs".into(),
                }]))
                .into(),
            );
            lp.set_sets(
                std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftSetRow {
                    name: "release".into(),
                    path: "/ws/release.set".into(),
                    detail: "3 workspaces".into(),
                }]))
                .into(),
            );
            lp.set_detached(
                std::rc::Rc::new(slint::VecModel::from(vec![crate::LeftSessionRow {
                    uid: "s1".into(),
                    label: "build".into(),
                    detail: "idle".into(),
                    live: 0.0,
                }]))
                .into(),
            );
        }),
        ("left panel: a module's rows", |w| {
            install_modes(w);
            install_files(w);
        }),
        ("pane: data tree", |w| {
            install_view_pane_at(w, 7, "package.json", tree_rows(), (-1, -1), "", 14.0);
        }),
        ("pane: grid", |w| {
            install_view_pane_at(
                w,
                8,
                "parts.csv",
                grid_rows("parts.csv", PARTS),
                (-1, -1),
                "",
                14.0,
            );
        }),
    ]
}

/// THE walk. Every `TouchArea` under the left panel or a viewer pane has an accessible
/// ancestor-or-self within [`MAX_LEVELS`] levels; the failure names each one that does
/// not, with the state it was met in. The full table (every state, every `TouchArea`,
/// its distance and anchor) is printed, so `--nocapture` shows what the number means.
#[test]
fn every_touch_area_has_an_accessible_ancestor_within_reach() {
    ui(|| {
        let mut hits = Vec::new();
        for (state, setup) in states() {
            let w = window();
            setup(&w);
            let found = touch_areas(state, &w);
            assert!(
                !found.is_empty(),
                "{state}: the walk met no TouchArea at all — debug info missing?"
            );
            hits.extend(found);
        }
        // The image pane holds no TouchArea; it is covered by its own lookup test.

        eprintln!(
            "{:<32} {:<6} {:<7} {:<44} anchor",
            "state", "owned", "levels", "touch area"
        );
        for h in &hits {
            eprintln!(
                "{:<32} {:<6} {:<7} {:<44} {}",
                h.state,
                h.owned,
                h.levels
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "none".into()),
                h.name,
                h.anchor
            );
        }
        let deepest = hits
            .iter()
            .filter(|h| h.owned)
            .filter_map(|h| h.levels)
            .max()
            .expect("the owned subtrees hold TouchAreas");
        eprintln!("deepest owned TouchArea sits {deepest} level(s) below its anchor (MAX_LEVELS {MAX_LEVELS})");

        let bad: Vec<String> = hits
            .iter()
            .filter(|h| h.owned && h.levels.is_none_or(|l| l > MAX_LEVELS))
            .map(|h| {
                format!(
                    "  [{}] {} — nearest accessible ancestor: {}",
                    h.state,
                    h.name,
                    h.levels
                        .map(|l| format!("{l} levels up ({})", h.anchor))
                        .unwrap_or_else(|| "none".into())
                )
            })
            .collect();
        assert!(
            bad.is_empty(),
            "{} TouchArea(s) without an accessible ancestor-or-self within {MAX_LEVELS} level(s):\n{}",
            bad.len(),
            bad.join("\n")
        );
        assert_eq!(
            deepest, MAX_LEVELS,
            "MAX_LEVELS is meant to be the smallest value that passes; lower it to {deepest}"
        );
    });
}

// ===== fixtures =====

/// A files module showing a project: an open directory, a file inside it, one file the
/// module marked `selected`, and a hidden one it dimmed.
///
/// Pushed through the same `fill_rail` projection `paneview::resync` uses, so what these
/// tests read is what a real `host.rows.set` would put on screen.
fn install_files(w: &crate::AppWindow) {
    let module = avada_core::rights::ModuleId::new("bshuler/avada-files").expect("a module id");
    let mut rail = crate::leftpanel::ModuleRail::default();
    rail.apply(avada_core::module::RailEvent::Registered {
        module: module.clone(),
        entries: vec![serde_json::from_value(serde_json::json!({
            "id": "files", "label": "Files", "tier": 1, "order": 0
        }))
        .expect("a rail entry")],
    });
    let row = |depth: u8, label: &str, detail: &str, expandable, expanded, marks: &[&str]| {
        avada_core::module::Row {
            id: format!("/proj/{label}"),
            label: label.into(),
            detail: detail.into(),
            depth,
            expandable,
            expanded,
            icon: None,
            marks: marks.iter().map(|m| m.to_string()).collect(),
            data: serde_json::json!({}),
        }
    };
    rail.apply(avada_core::module::RailEvent::Rows {
        module: module.clone(),
        entry: "files".into(),
        rows: vec![
            row(0, "src", "", true, true, &[]),
            row(1, "main.rs", "src", false, false, &[]),
            row(0, "target", "", true, false, &[]),
            row(0, ".env", "", false, false, &["hidden"]),
            row(0, "README.md", "", false, false, &["selected"]),
        ],
    });
    rail.activate(&crate::leftpanel::entry_key(&module, "files"));
    w.global::<crate::LeftPanelAdapter>().set_open(true);
    crate::paneview::fill_rail(w, &rail);
    // The rows live behind `if LeftPanelAdapter.mode == -1`, so a rail that is merely
    // *activated* draws nothing: activation is `RailAdapter` state, and the mode is what
    // decides whether the panel is showing the module's list or a built-in section. Without
    // this the whole block is absent from the tree and every lookup below finds zero.
    w.global::<crate::LeftPanelAdapter>()
        .set_mode(crate::paneview::LEFT_MODE_RAIL);
}


/// One node row as `viewpane::model_for` projects a data file: `check` is 1 open, 0
/// folded, -1 for a scalar.
fn tree_node(depth: i32, text: &str, check: i32, detail: &str) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role: 18,
        text: text.into(),
        detail: detail.into(),
        activatable: check >= 0,
        md: slint::StyledText::from_markdown(text).expect("a plain key parses as markdown"),
        indent: depth,
        check,
        ..Default::default()
    }
}

/// `package.json`: `name`, then `deps` open over two children, then `nested` folded.
fn tree_rows() -> Vec<crate::PaneViewRow> {
    vec![
        tree_node(0, "name: \"x\"", -1, ""),
        tree_node(0, "deps", 1, "2 keys"),
        tree_node(1, "serde: \"1\"", -1, ""),
        tree_node(1, "tokio: \"1\"", -1, ""),
        tree_node(0, "nested", 0, "3 keys"),
    ]
}

const PARTS: &str = "name,qty,price\nbolt,4,1.20\n\"nut, brass\",12,0.5\n";

/// The rows `viewpane` projects a CSV into, through the same cache the pump uses.
fn grid_rows(name: &str, body: &str) -> Vec<crate::PaneViewRow> {
    use slint::Model;
    let d = std::env::temp_dir().join(format!("hp-uitest-annotations-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    let p = d.join(name);
    std::fs::write(&p, body).expect("write");
    let model = crate::viewpane::model_for(
        &format!("uitest-annotations-{name}"),
        &avada_core::tools::PaneKind::Table,
        Some(&p.display().to_string()),
        0,
    );
    model.iter().collect()
}

// ===== lookup by label: the left panel's rows =====

/// A module's row is reached by its label; an expandable row says whether it is open; a
/// leaf is neither; the row the module marked `selected` announces itself as selected.
///
/// This is the accessibility contract a tier-1 module gets for free — it never writes a
/// line of Slint, so if the panel does not announce its rows, nothing else will.
#[test]
fn a_module_row_is_reached_by_its_label_and_says_whether_it_is_open() {
    ui(|| {
        let w = window();
        install_modes(&w);
        install_files(&w);

        let src = only(&w, "src", AccessibleRole::Button);
        assert_eq!(src.accessible_expandable(), Some(true));
        assert_eq!(src.accessible_expanded(), Some(true), "src is open");
        assert_eq!(src.accessible_item_selected(), Some(false));

        let target = only(&w, "target", AccessibleRole::Button);
        assert_eq!(
            target.accessible_expanded(),
            Some(false),
            "target is folded"
        );

        let main = only(&w, "main.rs", AccessibleRole::Button);
        assert_eq!(
            main.accessible_expandable(),
            Some(false),
            "a leaf does not open"
        );
        assert_eq!(main.accessible_expanded(), Some(false));
        assert_eq!(
            main.accessible_description().as_deref(),
            Some("src"),
            "the module's `detail` is the description"
        );
        assert_eq!(main.accessible_enabled(), Some(true));

        let readme = only(&w, "README.md", AccessibleRole::Button);
        assert_eq!(
            readme.accessible_item_selected(),
            Some(true),
            "the `selected` mark is what a reveal has to announce"
        );
        // Every mark verbatim, including ones the panel draws nothing for: a module may
        // invent a vocabulary this build has never heard of and it must still be spoken.
        assert_eq!(
            only(&w, ".env", AccessibleRole::Button)
                .accessible_value()
                .as_deref(),
            Some("hidden")
        );
    });
}


// ===== lookup by label: the viewer panes =====

/// A data-tree node is reached by its key and says whether it is open.
#[test]
fn a_tree_node_is_reached_by_its_key_and_says_whether_it_is_open() {
    ui(|| {
        let w = window();
        install_view_pane_at(&w, 7, "package.json", tree_rows(), (-1, -1), "", 14.0);

        let deps = only(&w, "deps", AccessibleRole::ListItem);
        assert_eq!(deps.accessible_expandable(), Some(true));
        assert_eq!(deps.accessible_expanded(), Some(true), "deps is open");
        assert_eq!(deps.accessible_description().as_deref(), Some("2 keys"));

        let nested = only(&w, "nested", AccessibleRole::ListItem);
        assert_eq!(
            nested.accessible_expanded(),
            Some(false),
            "nested is folded"
        );

        let scalar = only(&w, "serde: \"1\"", AccessibleRole::ListItem);
        assert_eq!(
            scalar.accessible_expandable(),
            Some(false),
            "a scalar does not open"
        );
    });
}

/// A grid cell is reached by its verbatim text — every one of the nine, head row
/// included — while the row keeps announcing all its cells at once.
#[test]
fn a_grid_cell_is_reached_by_its_text() {
    ui(|| {
        let w = window();
        install_view_pane_at(
            &w,
            8,
            "parts.csv",
            grid_rows("parts.csv", PARTS),
            (-1, -1),
            "",
            14.0,
        );

        for text in [
            "name",
            "qty",
            "price",
            "bolt",
            "4",
            "1.20",
            "nut, brass",
            "12",
            "0.5",
        ] {
            only(&w, text, AccessibleRole::Text);
        }
        let cells = by_id(&w, "ViewRowView::cell");
        assert_eq!(cells.len(), 9);
        assert_eq!(
            cells[3].accessible_label().as_deref(),
            Some("bolt"),
            "the first body cell announces its text"
        );
        only(&w, "bolt | 4 | 1.20", AccessibleRole::ListItem);
    });
}

/// The image pane's caption is reached by its text, and the picture by its name.
#[test]
fn an_image_caption_is_reached_by_its_text() {
    ui(|| {
        use crate::imagepane::testpng::{encode, TempFile};
        let w = window();
        let f = TempFile::write("cat.png", &encode(40, 20));
        crate::imagepane::attach(&w);
        crate::imagepane::project("a11y-img", Some(f.path()));
        w.set_panes(
            std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
                title: "the pane".into(),
                uid: "a11y-img".into(),
                x: 8.0,
                y: 40.0,
                w: 600.0,
                h: 500.0,
                visible: true,
                focused: true,
                kind: 9,
                is_view: true,
                view_title: crate::imagepane::file_name(f.path()).into(),
                font_px: 14.0,
                ..Default::default()
            }]))
            .into(),
        );
        let caption = crate::imagepane::row("a11y-img")
            .expect("the pane was projected")
            .caption
            .to_string();
        assert!(
            caption.contains("40×20"),
            "caption names the size: {caption}"
        );
        only(&w, &caption, AccessibleRole::Text);
        only(&w, "cat.png, 40×20", AccessibleRole::Image);
        assert!(crate::imagepane::forget("a11y-img"));
    });
}
