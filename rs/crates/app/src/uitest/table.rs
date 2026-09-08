//! Track V2: the table viewer: header and cell rows.
//! Owned by the V2 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! These drive the real projection (`viewpane::model_for` on a file written to a scratch
//! directory) into the real `ViewRowView`, and read back what the view drew: the rows by
//! their accessible label, the row-number gutter by its text, and the cells by element id
//! — a `StyledText` cannot label itself from a `styled-text`, so a cell's *geometry* is
//! what proves the column layout (see the frozen-file note in the track report).

use super::*;
use avada_core::tools::PaneKind;
use slint::Model;

/// A scratch file for one test, rewritten on every run.
fn scratch_file(name: &str, body: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("hp-uitest-table-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    let p = d.join(name);
    std::fs::write(&p, body).expect("write");
    p
}

/// The rows `viewpane` projects a table file into, through the same cache the pump uses.
fn grid_rows(name: &str, body: &str) -> Vec<crate::PaneViewRow> {
    let p = scratch_file(name, body);
    let model = crate::viewpane::model_for(
        &format!("uitest-table-{name}"),
        &PaneKind::Table,
        Some(&p.display().to_string()),
        0,
    );
    model.iter().collect()
}

/// The table pane at a font size: `kind` 8 is `PaneKind::Table`.
fn install_grid(w: &crate::AppWindow, rows: Vec<crate::PaneViewRow>, font_px: f32) {
    install_view_pane_at(w, 8, "data.csv", rows, (-1, -1), "", font_px);
}

/// Every cell the view drew, in tree order: row by row, column by column.
fn cells(w: &crate::AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_id(w, "ViewRowView::cell").collect()
}

const PARTS: &str = "name,qty,price\nbolt,4,1.20\n\"nut, brass\",12,0.5\n";

/// The head row and every body row render, each announcing its verbatim cells, with the
/// record number in the gutter beside it.
#[test]
fn a_grid_renders_its_head_row_and_every_body_row() {
    ui(|| {
        let w = window();
        install_grid(&w, grid_rows("parts.csv", PARTS), 14.0);

        only(&w, "name | qty | price", AccessibleRole::ListItem);
        only(&w, "bolt | 4 | 1.20", AccessibleRole::ListItem);
        only(&w, "nut, brass | 12 | 0.5", AccessibleRole::ListItem);
        for n in ["1", "2", "3"] {
            only(&w, n, AccessibleRole::Text);
        }
        assert_eq!(cells(&w).len(), 9, "three rows of three cells each");
    });
}

/// Each cell is its own element, and the columns line up: a cell sits at the same x as
/// the one above it, and a column is as wide as its content — the ten-character name
/// column is wider than the three-character quantity column, which an even split of the
/// row would never give.
#[test]
fn cells_are_individually_addressable_and_line_up_in_content_width_columns() {
    ui(|| {
        let w = window();
        install_grid(&w, grid_rows("parts.csv", PARTS), 14.0);
        let c = cells(&w);
        assert_eq!(c.len(), 9);
        for col in 0..3 {
            let head = c[col].absolute_position().x;
            for row in 1..3 {
                let x = c[row * 3 + col].absolute_position().x;
                assert!(
                    (x - head).abs() < 0.5,
                    "column {col} drifted between rows: {head} vs {x}"
                );
            }
        }
        let name = c[0].size().width;
        let qty = c[1].size().width;
        assert!(
            name > 0.0 && qty > 0.0,
            "cells have to be drawn to be measured"
        );
        assert!(
            name > qty * 2.0,
            "the name column ({name}) must be wider than the qty column ({qty}), not an even share"
        );
        // Left to right, with a gap between columns.
        assert!(c[1].absolute_position().x > c[0].absolute_position().x + name);
        assert!(c[2].absolute_position().x > c[1].absolute_position().x + qty);
    });
}

/// The grid branch must not touch a markdown table: the document table has no record
/// number (`detail` is empty), so it draws no gutter and its cells still stretch to the
/// row's right edge, where a grid's cells stop at their content and leave the slack.
#[test]
fn a_markdown_table_row_keeps_its_stretched_columns_and_has_no_gutter() {
    ui(|| {
        let w = window();
        // Where the last cell of the head row ends, relative to the row's right padding.
        let slack = |w: &crate::AppWindow| {
            let row = only(w, "name | qty | price", AccessibleRole::ListItem);
            let last = &cells(w)[2];
            (row.absolute_position().x + row.size().width - 8.0)
                - (last.absolute_position().x + last.size().width)
        };

        install_grid(&w, grid_rows("parts.csv", PARTS), 14.0);
        let grid_slack = slack(&w);
        assert!(
            grid_slack > 20.0,
            "a grid's cells stop at their content: slack {grid_slack}"
        );

        let mut rows = grid_rows("parts.csv", PARTS);
        for r in &mut rows {
            r.detail = "".into();
        }
        install_view_pane_at(&w, 4, "doc.md", rows, (-1, -1), "", 14.0);
        assert!(
            by_role(&w, "1", AccessibleRole::Text).is_empty(),
            "a document table has no row numbers"
        );
        assert_eq!(cells(&w).len(), 9);
        let md_slack = slack(&w);
        assert!(
            md_slack.abs() < 1.0,
            "a markdown table's cells stretch to the edge: slack {md_slack}"
        );
    });
}

/// `Cmd+=` reaches the cells and the gutter, not just the row: a cell's width follows the
/// font size, so a column at 28px is twice as wide as at 14px.
#[test]
fn column_widths_follow_the_zoom_chord() {
    ui(|| {
        let w = window();
        let measure = |px: f32| {
            install_grid(&w, grid_rows("parts.csv", PARTS), px);
            let c = cells(&w);
            let gutter = only(&w, "2", AccessibleRole::Text);
            (c[0].size().width, c[1].size().width, gutter.size().width)
        };
        let (name, qty, gutter) = measure(14.0);
        let (name2, qty2, gutter2) = measure(28.0);
        assert!(name > 0.0 && qty > 0.0 && gutter > 0.0);
        for (what, a, b) in [
            ("name", name, name2),
            ("qty", qty, qty2),
            ("gutter", gutter, gutter2),
        ] {
            assert!(
                (b - a * 2.0).abs() < a * 0.1,
                "the {what} column ignored the zoom: {a} → {b}"
            );
        }
    });
}

/// A thousand rows is an ordinary file, well under the row cap: every row is projected
/// and none is replaced by a "more rows" notice.
#[test]
fn a_thousand_row_file_renders_without_hitting_the_row_cap() {
    ui(|| {
        let w = window();
        let mut body = String::from("n,sq\n");
        for i in 1..=1_000 {
            body.push_str(&format!("{i},{}\n", i * i));
        }
        let rows = grid_rows("thousand.csv", &body);
        assert_eq!(rows.len(), 1_001);
        assert!(
            rows.iter().all(|r| r.role == 13 || r.role == 14),
            "no notice row: nothing was cut"
        );
        install_grid(&w, rows, 14.0);
        only(&w, "n | sq", AccessibleRole::ListItem);
        only(&w, "1 | 1", AccessibleRole::ListItem);
        assert!(
            by_label(&w, "… 0 more rows not shown").is_empty()
                && by_label(&w, "… 1 more rows not shown").is_empty()
        );
    });
}

/// The degraded path: a file that is not CSV opens as one inert notice that says why —
/// and says it as a row the reader can find, not as an empty pane.
#[test]
fn a_file_that_is_not_csv_opens_as_one_notice_saying_why() {
    ui(|| {
        let w = window();
        let rows = grid_rows("bad.csv", "a,b\n1,\"never closed\n2,3\n");
        assert_eq!(rows.len(), 1);
        install_grid(&w, rows, 14.0);
        only(
            &w,
            "Cannot parse as CSV: unterminated quoted field starting on line 2",
            AccessibleRole::ListItem,
        );
        assert!(cells(&w).is_empty(), "a notice draws no cells");
    });
}
