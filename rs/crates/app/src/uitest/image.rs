//! Track V3: the image viewer: fit, caption, checkerboard.
//! Owned by the V3 track; `super::*` brings the harness helpers (`ui`, `window`,
//! `click`, `by_label`, `by_id`, ...) into scope.
//!
//! Every claim below is made against the built component tree: the picture is an
//! `Image` element with a size, the caption and the notice are named `Text`s, and the
//! checkerboard is the element behind them. `imagepane::fit` has unit tests of its own;
//! these prove the `.slint` actually asks it, draws the answer, and re-asks when the
//! stage or the zoom changes — the hop no per-function test can see.
#![allow(unused_imports)]

use super::*;
use crate::imagepane::testpng::{encode, TempFile};
use i_slint_backend_testing::AccessibleRole;

/// Publish one `view:image` pane (kind 9) of `w × h` logical px showing `target`, at the
/// per-pane font size the zoom chord moves. Decodes through the same `project` the pump's
/// `pane_item` calls, and binds the adapter the way `Ui::attach` does — without `on_fit`
/// the callback answers all zeros and the picture is drawn 0×0.
fn install_image_pane(
    w: &crate::AppWindow,
    uid: &str,
    target: &str,
    size: (f32, f32),
    font_px: f32,
) {
    crate::imagepane::attach(w);
    crate::imagepane::project(uid, Some(target));
    w.set_panes(
        std::rc::Rc::new(slint::VecModel::from(vec![crate::PaneItem {
            title: "the pane".into(),
            uid: uid.into(),
            x: 8.0,
            y: 40.0,
            w: size.0,
            h: size.1,
            visible: true,
            focused: true,
            kind: 9,
            is_view: true,
            view_title: crate::imagepane::file_name(target).into(),
            font_px,
            ..Default::default()
        }]))
        .into(),
    );
}

/// The one drawn picture named `label`, or a failure saying how many there were.
fn picture(w: &crate::AppWindow, label: &str) -> ElementHandle {
    only(w, label, AccessibleRole::Image)
}

/// The caption Rust wrote for pane `uid` (it carries the byte count, which the test
/// does not want to predict).
fn caption_of(uid: &str) -> String {
    crate::imagepane::row(uid)
        .expect("the pane was projected")
        .caption
        .to_string()
}

/// A 40×20 icon in a 600×500 pane is drawn at 40×20: fitted means scaled DOWN to fit,
/// never up past 1:1. Remove the `min(1.0)` in `imagepane::fit` and the icon fills the
/// stage. The element is also named for a screen reader — an unlabelled image is
/// invisible to a reader and to this test alike.
#[test]
fn a_small_picture_is_drawn_at_one_to_one_and_named() {
    ui(|| {
        let w = window();
        let f = TempFile::write("cat.png", &encode(40, 20));
        install_image_pane(&w, "img-small", f.path(), (600.0, 500.0), 14.0);
        let img = picture(&w, "cat.png, 40×20");
        let sz = img.size();
        assert!(
            (sz.width - 40.0).abs() < 0.5 && (sz.height - 20.0).abs() < 0.5,
            "a 40×20 icon was drawn at {}×{}",
            sz.width,
            sz.height
        );
        assert!(crate::imagepane::forget("img-small"));
    });
}

/// A picture wider than the stage is scaled down to the stage, aspect preserved, and
/// re-fitted when the pane changes shape: an 8:3 picture stays 8:3 in a wide pane and
/// in a tall one, at two different sizes.
#[test]
fn a_large_picture_keeps_its_aspect_across_two_pane_sizes() {
    ui(|| {
        let w = window();
        let f = TempFile::write("wide.png", &encode(800, 300));
        let mut seen = Vec::new();
        for (pw, ph) in [(600.0_f32, 500.0_f32), (300.0, 500.0)] {
            install_image_pane(&w, "img-wide", f.path(), (pw, ph), 14.0);
            let sz = picture(&w, "wide.png, 800×300").size();
            assert!(sz.width > 0.0 && sz.height > 0.0, "drawn 0×0 in {pw}×{ph}");
            assert!(
                sz.width <= pw && sz.height <= ph,
                "{}×{} does not fit a {pw}×{ph} pane",
                sz.width,
                sz.height
            );
            let ratio = sz.width / sz.height;
            assert!(
                (ratio - 800.0 / 300.0).abs() < 0.02,
                "aspect not preserved in {pw}×{ph}: {}×{} ({ratio})",
                sz.width,
                sz.height
            );
            seen.push(sz.width);
        }
        assert!(
            seen[0] > seen[1],
            "the narrower pane must draw a narrower picture: {seen:?}"
        );
        assert!(crate::imagepane::forget("img-wide"));
    });
}

/// Cmd/Ctrl+= moves `PaneItem::font-px`; the image pane reads it the way `ViewPane`
/// does, so twice the font is twice the picture. Stop reading `font-px` in
/// `imagepane.slint` and both sizes come out equal.
#[test]
fn the_zoom_chord_scales_the_picture() {
    ui(|| {
        let w = window();
        let f = TempFile::write("cat.png", &encode(40, 20));
        install_image_pane(&w, "img-zoom", f.path(), (600.0, 500.0), 14.0);
        let base = picture(&w, "cat.png, 40×20").size();
        install_image_pane(&w, "img-zoom", f.path(), (600.0, 500.0), 28.0);
        let doubled = picture(&w, "cat.png, 40×20").size();
        assert!(
            (doubled.width - base.width * 2.0).abs() < 0.5
                && (doubled.height - base.height * 2.0).abs() < 0.5,
            "twice the font is twice the picture: {}×{} → {}×{}",
            base.width,
            base.height,
            doubled.width,
            doubled.height
        );
        assert!(crate::imagepane::forget("img-zoom"));
    });
}

/// The caption under the picture names the file, its pixel dimensions, its size on disk
/// and its format, and is a named text so a reader can announce it.
#[test]
fn the_caption_names_file_dimensions_size_and_format() {
    ui(|| {
        let w = window();
        let f = TempFile::write("cat.png", &encode(40, 20));
        install_image_pane(&w, "img-cap", f.path(), (600.0, 500.0), 14.0);
        let caption = caption_of("img-cap");
        assert!(
            caption.starts_with("cat.png · 40×20 · ") && caption.ends_with(" · PNG"),
            "{caption}"
        );
        let el = only(&w, &caption, AccessibleRole::Text);
        assert!(el.size().width > 0.0 && el.size().height > 0.0);
        assert!(crate::imagepane::forget("img-cap"));
    });
}

/// The checkerboard is laid out under the picture at the stage's full size, so a
/// transparent corner reads as transparent rather than as the pane background.
#[test]
fn the_checkerboard_fills_the_stage_behind_the_picture() {
    ui(|| {
        let w = window();
        let f = TempFile::write("cat.png", &encode(40, 20));
        install_image_pane(&w, "img-checker", f.path(), (600.0, 500.0), 14.0);
        let boards = by_id(&w, "ImagePane::checker");
        assert_eq!(boards.len(), 1, "one checkerboard per image pane");
        let board = boards[0].size();
        let img = picture(&w, "cat.png, 40×20").size();
        assert!(
            board.width > img.width && board.height > img.height,
            "the board ({}×{}) must be the stage, not the picture ({}×{})",
            board.width,
            board.height,
            img.width,
            img.height
        );
        assert!(crate::imagepane::forget("img-checker"));
    });
}

/// A file that does not decode shows the notice **instead of** a picture: no `Image`
/// element carries the file's name, and the named text says why.
#[test]
fn a_broken_file_shows_the_notice_instead_of_the_picture() {
    ui(|| {
        let w = window();
        let f = TempFile::write("bad.png", b"hello, not a picture\n");
        install_image_pane(&w, "img-bad", f.path(), (600.0, 500.0), 14.0);
        let pictures: Vec<_> = ElementHandle::find_by_accessible_label(&w, "bad.png, 0×0")
            .filter(|e| e.accessible_role() == Some(AccessibleRole::Image))
            .collect();
        assert!(pictures.is_empty(), "a broken file must not draw a picture");
        let notice = only(
            &w,
            "Cannot show bad.png: not a supported image format",
            AccessibleRole::Text,
        );
        assert!(notice.size().width > 0.0 && notice.size().height > 0.0);
        assert!(crate::imagepane::forget("img-bad"));
    });
}

/// The `ImagePaneAdapter.fit` callback is what the `.slint` asks for the rectangle;
/// unbound it answers zeros. Delete the `on_fit` handler in `imagepane::attach` and
/// this — and every picture above — is drawn 0×0.
#[test]
fn the_fit_callback_answers_from_rust() {
    ui(|| {
        let w = window();
        crate::imagepane::attach(&w);
        let f = w
            .global::<crate::ImagePaneAdapter>()
            .invoke_fit(40.0, 20.0, 600.0, 500.0, 1.0);
        assert_eq!((f.w, f.h), (40.0, 20.0), "{f:?}");
        assert_eq!((f.x, f.y), (280.0, 240.0), "{f:?}");
        let f = w
            .global::<crate::ImagePaneAdapter>()
            .invoke_fit(800.0, 300.0, 600.0, 500.0, 2.0);
        assert!(
            (f.w - 1200.0).abs() < 0.01 && (f.h - 450.0).abs() < 0.01,
            "{f:?}"
        );
    });
}
