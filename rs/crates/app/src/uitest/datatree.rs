//! Track V1: the data viewer: node rows expand, collapse and report their role.
//! Owned by the V1 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! The row contract under test is the one `viewpane::role::DATA_NODE` documents: role
//! 18, `indent` the depth, `check` the disclosure (`-1` scalar, `0` folded, `1` open),
//! `detail` the folded child count, `text` the key or `key: value`, `md` the same words
//! inked. The tests here build rows by hand so a layout bug is a layout bug and not a
//! parser bug; the one that clicks through to a re-projection uses the real file path.
#![allow(unused_imports)]

use super::*;

/// One node row as `viewpane::model_for` would project it.
fn node(depth: i32, text: &str, check: i32, detail: &str) -> crate::PaneViewRow {
    crate::PaneViewRow {
        role: 18,
        text: text.into(),
        detail: detail.into(),
        // A container is activatable so its click reaches Rust and becomes a toggle;
        // a scalar is inert and selects.
        activatable: check >= 0,
        md: slint::StyledText::from_markdown(text)
            .expect("a plain key has to parse as markdown, or the row is blank"),
        indent: depth,
        check,
        ..Default::default()
    }
}

/// `package.json`, three levels deep, as its rows: `name`, then `deps` open over two
/// children, then `nested` folded (it would have children, and says how many).
fn package_rows() -> Vec<crate::PaneViewRow> {
    vec![
        node(0, "name: \"x\"", -1, ""),
        node(0, "deps", 1, "2 keys"),
        node(1, "serde: \"1\"", -1, ""),
        node(1, "tokio: \"1\"", -1, ""),
        node(0, "nested", 0, "3 keys"),
    ]
}

/// The `StyledText` a node row draws its words in. Empty on every other role.
fn node_texts(w: &crate::AppWindow) -> Vec<ElementHandle> {
    by_id(w, "ViewRowView::node")
}

/// Role 18 sits past the old `role > 17` fallback, and Slint `if` blocks are not
/// exclusive: without the fallback moving to `> 18` every node would draw twice — once
/// as a tree row and once as a numbered plain line on top of it. The two branches are
/// told apart by the child-count text: the tree draws it only on a folded row, the
/// fallback would draw `detail` in its gutter on every row.
#[test]
fn a_data_pane_draws_node_rows_once_and_a_plain_pane_draws_none() {
    ui(|| {
        let w = window();
        install_view_pane_at(&w, 7, "package.json", package_rows(), (-1, -1), "", 14.0);
        assert_eq!(
            node_texts(&w).len(),
            5,
            "every node row draws its words through the tree branch"
        );
        assert_eq!(
            by_role(&w, "3 keys", AccessibleRole::Text).len(),
            1,
            "a folded container shows what it hides, once"
        );
        assert!(
            by_role(&w, "2 keys", AccessibleRole::Text).is_empty(),
            "an open container does not, and the plain-line fallback must not draw it \
             in a gutter either"
        );

        install_view_pane_at(&w, 3, "a.log", vec![line(1, "name")], (-1, -1), "", 14.0);
        assert!(
            node_texts(&w).is_empty(),
            "a plain viewer line must not reach the tree branch"
        );
    });
}

/// A container announces that it folds and whether it is open; a scalar announces
/// neither. And the name is the words — the key, or `key: value` — never the glyph and
/// never the `Line N:` prefix the other fixed-height roles carry: a data row is a node,
/// not a line, and its number means nothing to anyone.
#[test]
fn a_container_row_says_it_folds_and_a_scalar_row_does_not() {
    ui(|| {
        let w = window();
        install_view_pane(&w, 7, "package.json", package_rows(), (-1, -1), "");

        let open = only(&w, "deps", AccessibleRole::ListItem);
        assert_eq!(open.accessible_expandable(), Some(true));
        assert_eq!(
            open.accessible_expanded(),
            Some(true),
            "it is open, and says so"
        );

        let folded = only(&w, "nested", AccessibleRole::ListItem);
        assert_eq!(folded.accessible_expandable(), Some(true));
        assert_eq!(
            folded.accessible_expanded(),
            Some(false),
            "it is folded, and says so"
        );

        let scalar = only(&w, "name: \"x\"", AccessibleRole::ListItem);
        assert_ne!(
            scalar.accessible_expandable(),
            Some(true),
            "a scalar has nothing to unfold"
        );
        assert_ne!(scalar.accessible_expanded(), Some(true));

        for glyph in ["▸ nested", "▾ deps", "Line 3 keys: nested", "Line : deps"] {
            assert!(
                by_role(&w, glyph, AccessibleRole::ListItem).is_empty(),
                "the label is the words, not the glyph or a line number: found {glyph:?}"
            );
        }
    });
}

/// The click on a container: it is activatable, so the row fires `pane-view-activate`
/// with its own index — the same callback a listing's directory row fires, which is how
/// the toggle reaches Rust without a new Slint callback. Then the rest of the round trip
/// against a real file: the row the callback named is the one `viewpane::row_at` finds,
/// its `node` is what the toggle takes, and the re-projection has fewer rows.
#[test]
fn clicking_a_container_fires_activate_and_the_toggle_shrinks_the_tree() {
    ui(|| {
        use slint::Model;
        let w = window();
        let dir = std::env::temp_dir().join(format!("hp-uitest-datatree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let file = dir.join("pkg.json");
        std::fs::write(
            &file,
            "{\"name\": \"x\", \"deps\": {\"serde\": \"1\", \"tokio\": \"1\"}, \"ok\": true}\n",
        )
        .expect("write");
        let target = file.display().to_string();
        let uid = "uitest-datatree-click";
        crate::datatree::forget(uid);
        crate::viewpane::forget(uid);

        let project = || {
            let m = crate::viewpane::model_for(
                uid,
                &avada_core::tools::PaneKind::Data,
                Some(&target),
                0,
            );
            (0..m.row_count())
                .map(|i| m.row_data(i).unwrap())
                .collect::<Vec<_>>()
        };
        let rows = project();
        assert_eq!(rows.len(), 5, "name, deps, serde, tokio, ok");
        install_view_pane(&w, 7, "pkg.json", rows, (-1, -1), "");

        let saw = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let saw = saw.clone();
            w.on_pane_view_activate(move |pane, row| saw.borrow_mut().push((pane, row)));
        }
        click(&w, &only(&w, "deps", AccessibleRole::ListItem));
        assert_eq!(
            *saw.borrow(),
            vec![(0, 1)],
            "the container row of the first pane, by its own index"
        );

        // What `app::on_pane_view_activate` then does with (pane, row).
        let (_, row) = saw.borrow()[0];
        let r = crate::viewpane::row_at(uid, row as usize).expect("the clicked row");
        assert_eq!(r.role, crate::viewpane::role::DATA_NODE);
        assert_eq!(r.node, "$.deps");
        crate::datatree::toggle(uid, &r.node);

        let rows = project();
        assert_eq!(rows.len(), 3, "name, deps (folded), ok");
        install_view_pane(&w, 7, "pkg.json", rows, (-1, -1), "");
        let deps = only(&w, "deps", AccessibleRole::ListItem);
        assert_eq!(deps.accessible_expanded(), Some(false));
        assert!(
            by_role(&w, "serde: \"1\"", AccessibleRole::ListItem).is_empty(),
            "the folded subtree is gone from the pane"
        );
        assert_eq!(node_texts(&w).len(), 3);

        crate::datatree::forget(uid);
        crate::viewpane::forget(uid);
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// A scalar has nothing to unfold and nothing to open, so a click on it selects — the
/// same fork every inert row takes. If a scalar were ever marked activatable the click
/// would reach `on_pane_view_activate`, which would try to open the file in a new pane.
#[test]
fn clicking_a_scalar_selects_it_and_opens_nothing() {
    ui(|| {
        let w = window();
        install_view_pane(&w, 7, "package.json", package_rows(), (-1, -1), "");

        let opened = std::rc::Rc::new(std::cell::Cell::new(false));
        let picked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let opened = opened.clone();
            w.on_pane_view_activate(move |_, _| opened.set(true));
            let picked = picked.clone();
            w.on_pane_view_select(move |pane, row, ext| picked.borrow_mut().push((pane, row, ext)));
        }

        click(&w, &only(&w, "tokio: \"1\"", AccessibleRole::ListItem));
        assert_eq!(
            *picked.borrow(),
            vec![(0, 3, false)],
            "a plain click selects"
        );
        assert!(!opened.get(), "a scalar must not claim to open anything");
    });
}

/// Depth is paid as left padding on the disclosure column, and both the padding and the
/// column are in zoom units. Measured as geometry that can only move if the zoom did:
/// the glyph column's own width, and the horizontal distance between a depth-0 row's
/// disclosure and a depth-2 row's.
#[test]
fn the_tree_indent_and_its_disclosure_scale_with_zoom() {
    ui(|| {
        let w = window();
        let rows = || {
            vec![
                node(0, "a", 1, "1 key"),
                node(1, "b", 1, "1 key"),
                node(2, "c", 0, "1 key"),
            ]
        };
        let measure = |px: f32| {
            install_view_pane_at(&w, 7, "deep.json", rows(), (-1, -1), "", px);
            let marks = by_id(&w, "ViewRowView::disclosure");
            assert_eq!(marks.len(), 3, "one disclosure column per node row");
            let x0 = marks[0].absolute_position().x;
            let x2 = marks[2].absolute_position().x;
            (marks[0].size().width, x2 - x0)
        };

        let (col, indent) = measure(14.0);
        let (col2, indent2) = measure(28.0);
        assert!(col > 0.0 && indent > 0.0, "the row has to exist first");
        assert!(
            (col2 - col * 2.0).abs() < 0.5,
            "the disclosure column ignored the zoom: {col} → {col2}"
        );
        assert!(
            (indent2 - indent * 2.0).abs() < 0.5,
            "two levels of indent ignored the zoom: {indent} → {indent2}"
        );
    });
}
