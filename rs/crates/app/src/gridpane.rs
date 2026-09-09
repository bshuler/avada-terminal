//! The tier-5 module grid pane's projection (docs/modules-fanout-plan.md, track H4).
//!
//! The shape mirrors [`crate::imagepane`]: Rust decides, the `.slint` only paints. A module
//! sends whole frames of styled *spans*; this file flattens them into one flat run model
//! keyed by pane uid, because a Slint model is flat and a nested model per line would be a
//! new `VecModel` allocation per frame — at an editor's frame rate, on the window thread.
//!
//! Two things are deliberately *not* resolved here:
//!
//! * **Theme roles.** A span may name `accent` or `red`, and what those are lives in
//!   `theme.slint` and changes with the user's palette. Rust cannot read a Slint global, so
//!   a role travels as a number and `gridpane.slint` answers it. Only a literal `#rrggbb`
//!   arrives as a colour.
//! * **Geometry.** Runs carry cell coordinates; the cell *metrics* ride on the
//!   [`GridPaneItem`] once per frame, so a font-size change moves every run without
//!   touching one of them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use avada_core::module::grid::{CursorShape, GridFrame};
use avada_core::tools::kind::ModulePaneRef;
use slint::{ComponentHandle as _, Model, ModelRc, VecModel};

use crate::{AppWindow, GridPaneAdapter, GridPaneItem, GridRun};

/// A theme role name, as a number `gridpane.slint`'s `ink()` understands. `0` is "the pane's
/// own ink", which is also where an unknown name lands: the SDK is explicit that an
/// unstyled frame beats an unreadable one, so a typo must not resolve to black.
#[tracing::instrument(level = "debug", ret)]
fn role_of(name: &str) -> i32 {
    match name {
        "fg" | "text" => 2,
        "subtext" | "dim" => 3,
        "faint" => 4,
        "accent" | "blue" => 5,
        "danger" | "red" => 6,
        "ok" | "green" => 7,
        "warn" | "yellow" => 8,
        "link" | "teal" | "cyan" => 9,
        "border" => 10,
        "surface2" | "surface" => 11,
        "mantle" => 12,
        "bg" => 13,
        _ => 0,
    }
}

/// A span's colour as the `.slint` wants it: `(role, colour)`, where role `1` means "use the
/// colour" and anything else means "look the role up, the colour is a placeholder".
///
/// `None` — the span said nothing — is role `0`, which reads as the pane's ink for a
/// foreground and as *nothing at all* for a background. That asymmetry is the whole reason
/// the two are not one function's return value read twice.
#[tracing::instrument(level = "debug", ret)]
fn paint(spec: Option<&str>) -> (i32, slint::Color) {
    let Some(spec) = spec else {
        return (0, slint::Color::default());
    };
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                let [_, r, g, b] = v.to_be_bytes();
                return (1, slint::Color::from_rgb_u8(r, g, b));
            }
        }
        // A malformed hex is a typo, not a colour: fall through to the pane's ink rather
        // than to whatever `from_str_radix` half-parsed.
        return (0, slint::Color::default());
    }
    (role_of(&spec.to_ascii_lowercase()), slint::Color::default())
}

/// Flatten one frame into runs. Column positions are accumulated here rather than sent,
/// because [`avada_core::module::grid::GridLine`] says each span begins where the last
/// ended — deriving it is the only way that stays true when a module miscounts.
#[tracing::instrument(level = "debug", skip(frame))]
fn runs_of(uid: &str, frame: &GridFrame) -> Vec<GridRun> {
    let mut out = Vec::new();
    // A module that overruns its own row count must not push the pane's furniture around,
    // so the extra lines are dropped exactly as the contract says.
    for (line, gl) in frame.lines.iter().take(frame.rows as usize).enumerate() {
        let mut col = 0usize;
        for span in &gl.spans {
            let len = span.text.chars().count();
            if len == 0 {
                continue;
            }
            let (fg_role, fg) = paint(span.fg.as_deref());
            let (bg_role, bg) = paint(span.bg.as_deref());
            out.push(GridRun {
                uid: uid.into(),
                line: line as i32,
                col: col as i32,
                len: len as i32,
                text: span.text.as_str().into(),
                fg,
                bg,
                fg_role,
                bg_role,
                bold: span.bold,
                italic: span.italic,
                underline: span.underline,
            });
            col += len;
        }
    }
    out
}

/// What a pane's projection was built from. The frame's revision covers the module's side;
/// the metrics cover the host's, because a font-size change moves every run without the
/// module sending anything at all.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Fingerprint {
    revision: u64,
    cell_w: f32,
    cell_h: f32,
    font_px: f32,
}

struct Cache {
    /// Per pane uid: its row index in `grids`, what that row was built from, and the runs
    /// it contributed to the shared flat model.
    by_uid: HashMap<String, (usize, Fingerprint, Vec<GridRun>)>,
    grids: Rc<VecModel<GridPaneItem>>,
    runs: Rc<VecModel<GridRun>>,
}

thread_local! {
    static GRIDS: RefCell<Cache> = RefCell::new(Cache {
        by_uid: HashMap::new(),
        grids: Rc::new(VecModel::default()),
        runs: Rc::new(VecModel::default()),
    });
}

/// Rebuild the shared flat run model from every pane's contribution.
///
/// One uid's runs are not a contiguous slice anyone tracks, so a change rebuilds the whole
/// list. That is cheap because a window has at most a handful of grid panes and expensive
/// only in the case that cannot happen — and it removes the index bookkeeping that a
/// per-uid splice would need, which is the class of bug `imagepane::forget` already has to
/// hand-roll for its one row.
fn rebuild_runs(c: &Cache) {
    let mut all: Vec<GridRun> = Vec::new();
    for (_, _, runs) in c.by_uid.values() {
        all.extend(runs.iter().cloned());
    }
    c.runs.set_vec(all);
}

/// Keep pane `uid`'s grid row current. Called from `paneview::pane_item`, so it must be
/// cheap when nothing changed: one revision read and one comparison. Returns whether it
/// rebuilt — what the tests assert on, rather than timing.
#[tracing::instrument(level = "debug", ret)]
pub fn project(uid: &str, m: &ModulePaneRef, cell_w: f32, cell_h: f32, font_px: f32) -> bool {
    let fp = Fingerprint {
        revision: crate::module_ui::grid::generation(&m.id, &m.surface),
        cell_w,
        cell_h,
        font_px,
    };
    GRIDS.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, have, _)) = c.by_uid.get(uid) {
            if *have == fp {
                return false;
            }
        }
        let frame = crate::module_ui::grid::frame(&m.id, &m.surface);
        // A revision of zero is a surface that has never painted; an empty frame at a later
        // revision is a surface that painted nothing, and `forget` produces exactly that
        // when a module dies. Both must fall back to the placeholder, so both are `present:
        // false` — the revision alone would keep a dead module's pane looking alive.
        let present = fp.revision > 0 && frame.as_ref().is_some_and(|f| !f.lines.is_empty());
        let frame = frame.unwrap_or_default();
        let cursor = frame.cursor.unwrap_or_default();
        let item = GridPaneItem {
            uid: uid.into(),
            present,
            cols: frame.cols as i32,
            rows: frame.rows as i32,
            status: frame.status.as_str().into(),
            cursor_line: cursor.line as i32,
            cursor_col: cursor.col as i32,
            cursor_shape: match cursor.shape {
                CursorShape::Block => 0,
                CursorShape::Bar => 1,
                CursorShape::Underline => 2,
            },
            has_cursor: present && frame.cursor.is_some(),
            cell_w,
            cell_h,
            font_px,
        };
        let runs = if present {
            runs_of(uid, &frame)
        } else {
            Vec::new()
        };
        match c.by_uid.get(uid).map(|(i, _, _)| *i) {
            Some(i) => c.grids.set_row_data(i, item),
            None => c.grids.push(item),
        }
        let i = c
            .by_uid
            .get(uid)
            .map(|(i, _, _)| *i)
            .unwrap_or(c.grids.row_count() - 1);
        c.by_uid.insert(uid.to_string(), (i, fp, runs));
        rebuild_runs(&c);
        true
    })
}

/// Drop pane `uid`'s grid row and its runs. Called when the pane closes — unlike the image
/// pane's texture this one must be reclaimed, because a live editor's runs are the largest
/// per-pane allocation in the window and a session opens and closes many panes.
#[tracing::instrument(level = "debug", ret)]
pub fn forget(uid: &str) -> bool {
    GRIDS.with(|c| {
        let mut c = c.borrow_mut();
        let Some((i, _, _)) = c.by_uid.remove(uid) else {
            return false;
        };
        c.grids.remove(i);
        for (j, _, _) in c.by_uid.values_mut() {
            if *j > i {
                *j -= 1;
            }
        }
        rebuild_runs(&c);
        true
    })
}

/// The grid row for `uid`, for the tests that read a projection back without a window.
#[cfg(test)]
pub(crate) fn row(uid: &str) -> Option<GridPaneItem> {
    GRIDS.with(|c| {
        let c = c.borrow();
        c.by_uid.get(uid).and_then(|(i, _, _)| c.grids.row_data(*i))
    })
}

/// Pane `uid`'s runs, in frame order, for the same tests.
#[cfg(test)]
pub(crate) fn runs(uid: &str) -> Vec<GridRun> {
    GRIDS.with(|c| {
        c.borrow()
            .by_uid
            .get(uid)
            .map(|(_, _, r)| r.clone())
            .unwrap_or_default()
    })
}

/// Bind the two models. Called once per window by `paneview::Ui::attach`, beside the image
/// pane's — without it the adapter's models are empty and a grid pane draws its placeholder
/// forever, which is exactly the failure a missing `attach` should look like.
#[tracing::instrument(level = "debug", skip(app))]
pub fn attach(app: &AppWindow) {
    let ad = app.global::<GridPaneAdapter>();
    GRIDS.with(|c| {
        let c = c.borrow();
        ad.set_grids(ModelRc::from(c.grids.clone()));
        ad.set_runs(ModelRc::from(c.runs.clone()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use avada_core::module::grid::{GridCursor, GridLine, GridSpan};

    fn pane(surface: &str) -> ModulePaneRef {
        ModulePaneRef::new("bshuler/avada-editor", surface, None).expect("a valid pane ref")
    }

    fn styled(text: &str, fg: Option<&str>, bg: Option<&str>) -> GridSpan {
        GridSpan {
            text: text.into(),
            fg: fg.map(str::to_string),
            bg: bg.map(str::to_string),
            ..GridSpan::default()
        }
    }

    fn frame(lines: Vec<GridLine>, rows: u16) -> GridFrame {
        GridFrame {
            surface: "editor".into(),
            cols: 80,
            rows,
            lines,
            cursor: None,
            status: String::new(),
        }
    }

    /// The colour contract in one place: a literal wins, a name becomes a number the
    /// `.slint` resolves against the live palette, and neither a typo'd name nor a
    /// malformed hex is allowed to resolve to black on black.
    #[test]
    fn a_span_paints_by_literal_or_by_role_and_a_typo_falls_back_to_the_pane_s_ink() {
        assert_eq!(role_of("accent"), role_of("blue"), "aliases are one role");
        assert_eq!(role_of("nonsense"), 0);
        assert_eq!(
            role_of("FG"),
            0,
            "roles arrive lowercased, not case-folded here"
        );

        let (role, colour) = paint(Some("#1a2b3c"));
        assert_eq!(role, 1, "role 1 means: use the colour beside me");
        assert_eq!(colour, slint::Color::from_rgb_u8(0x1a, 0x2b, 0x3c));

        assert_eq!(
            paint(Some("Red")).0,
            role_of("red"),
            "a name is case-insensitive"
        );
        assert_eq!(paint(None).0, 0, "an unstyled span is the pane's own ink");
        for bad in ["#12345", "#gggggg", "#1a2b3c4d"] {
            assert_eq!(paint(Some(bad)).0, 0, "{bad} is a typo, not a colour");
        }
    }

    /// Columns are derived, never trusted: a module that miscounts its own span lengths
    /// would otherwise tear its text apart, and the contract says spans abut.
    #[test]
    fn runs_accumulate_their_own_columns_and_drop_what_overruns_the_frame() {
        let f = frame(
            vec![
                GridLine {
                    spans: vec![
                        styled("fn ", Some("accent"), None),
                        styled("", None, None),
                        styled("main", Some("#ff0000"), Some("surface")),
                    ],
                },
                GridLine {
                    spans: vec![GridSpan::plain("  ok")],
                },
                GridLine {
                    spans: vec![GridSpan::plain("past the end")],
                },
            ],
            2,
        );
        let runs = runs_of("u1", &f);

        assert_eq!(runs.len(), 3, "the empty span contributes nothing at all");
        assert_eq!((runs[0].line, runs[0].col, runs[0].len), (0, 0, 3));
        assert_eq!(
            (runs[1].line, runs[1].col, runs[1].len),
            (0, 3, 4),
            "the second span starts where the first ended, empty span notwithstanding"
        );
        assert_eq!(runs[1].fg_role, 1);
        assert_eq!(runs[1].bg_role, role_of("surface"));
        assert_eq!(
            (runs[2].line, runs[2].col),
            (1, 0),
            "each line restarts at zero"
        );
        assert!(
            runs.iter().all(|r| r.line < 2),
            "a module that overruns `rows` must not push the pane's furniture around"
        );

        // Cells, not bytes: a run's `len` is what the `.slint` multiplies by `cell_w`.
        let wide = runs_of(
            "u1",
            &frame(
                vec![GridLine {
                    spans: vec![GridSpan::plain("héllo")],
                }],
                1,
            ),
        );
        assert_eq!(wide[0].len, 5);
    }

    /// `project` is called on every pane build, so the cheap path has to be the common one —
    /// and the *expensive* path has to fire for a font change the module never hears about.
    #[test]
    fn a_projection_is_rebuilt_for_a_new_frame_or_new_metrics_and_for_nothing_else() {
        let p = pane("editor");
        crate::module_ui::grid::set_frame(
            &p.id,
            frame(
                vec![GridLine {
                    spans: vec![GridSpan::plain("hi")],
                }],
                24,
            ),
        );

        assert!(
            project("u1", &p, 8.0, 16.0, 13.0),
            "the first build always rebuilds"
        );
        assert!(!project("u1", &p, 8.0, 16.0, 13.0), "nothing moved");
        assert!(
            project("u1", &p, 9.0, 16.0, 13.0),
            "a wider cell moves every run"
        );
        assert!(project("u1", &p, 9.0, 18.0, 13.0));
        assert!(project("u1", &p, 9.0, 18.0, 15.0));
        assert!(!project("u1", &p, 9.0, 18.0, 15.0));

        crate::module_ui::grid::set_frame(
            &p.id,
            frame(
                vec![GridLine {
                    spans: vec![GridSpan::plain("bye")],
                }],
                24,
            ),
        );
        assert!(
            project("u1", &p, 9.0, 18.0, 15.0),
            "a new revision rebuilds"
        );
        assert_eq!(runs("u1")[0].text, "bye");
    }

    /// `present` is the switch between the grid and the placeholder underneath it, and the
    /// two ways a surface can have no picture must both land on the placeholder.
    #[test]
    fn a_surface_that_has_not_painted_or_has_died_shows_the_placeholder_not_a_stale_grid() {
        let p = pane("editor");
        project("u1", &p, 8.0, 16.0, 13.0);
        let item = row("u1").expect("a row exists even for an unpainted surface");
        assert!(!item.present, "nothing has painted yet");
        assert!(runs("u1").is_empty());

        let mut f = frame(
            vec![GridLine {
                spans: vec![GridSpan::plain("hi")],
            }],
            24,
        );
        f.status = "NORMAL".into();
        f.cursor = Some(GridCursor {
            line: 3,
            col: 7,
            shape: CursorShape::Bar,
        });
        crate::module_ui::grid::set_frame(&p.id, f);
        project("u1", &p, 8.0, 16.0, 13.0);
        let item = row("u1").unwrap();
        assert!(item.present && item.has_cursor);
        assert_eq!(
            (item.cursor_line, item.cursor_col, item.cursor_shape),
            (3, 7, 1)
        );
        assert_eq!(item.status, "NORMAL");
        assert_eq!((item.cols, item.rows), (80, 24));
        assert_eq!((item.cell_w, item.cell_h, item.font_px), (8.0, 16.0, 13.0));

        // `forget` on the module store blanks the frame at a *higher* revision — the case
        // the revision alone would render as a live pane full of a dead module's text.
        crate::module_ui::grid::forget(&p.id);
        assert!(project("u1", &p, 8.0, 16.0, 13.0));
        let item = row("u1").unwrap();
        assert!(!item.present && !item.has_cursor);
        assert!(
            runs("u1").is_empty(),
            "and its text is released, not just hidden"
        );
    }

    /// The shared flat run model is the one piece of cross-pane bookkeeping here, so the
    /// indices have to survive a close from the middle.
    #[test]
    fn closing_one_grid_pane_leaves_every_other_pane_s_rows_intact() {
        let p = pane("editor");
        crate::module_ui::grid::set_frame(
            &p.id,
            frame(
                vec![GridLine {
                    spans: vec![GridSpan::plain("hi")],
                }],
                24,
            ),
        );
        for uid in ["a", "b", "c"] {
            project(uid, &p, 8.0, 16.0, 13.0);
        }
        assert_eq!(GRIDS.with(|c| c.borrow().runs.row_count()), 3);

        assert!(forget("b"), "a live pane is forgotten");
        assert!(!forget("b"), "and forgetting it twice is not an error");
        assert!(!forget("never-opened"));

        assert!(row("b").is_none());
        assert_eq!(GRIDS.with(|c| c.borrow().runs.row_count()), 2);
        for uid in ["a", "c"] {
            assert_eq!(
                row(uid).expect("still projected").uid,
                uid,
                "the surviving rows still point at their own grid item"
            );
        }
    }
}
