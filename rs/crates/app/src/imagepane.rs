//! Image viewer pane (docs/viewer-panes-plan.md, track V3): fit-never-upscale, caption,
//! checkerboard. Owned by the V3 track.
//!
//! The shape mirrors `viewpane.rs`: Rust decodes and decides, the `.slint` only paints.
//! A `view:image` pane's target is decoded ONCE per (path, mtime, length) into a Slint
//! texture and parked in a per-uid model that `ui/imagepane.slint` filters by uid; the
//! per-tick `pane_item` call is a `stat` and a hash lookup. The geometry is a pure
//! function here rather than a `min()` chain in the `.slint` so the "never past 1:1"
//! clamp has a unit test that fails when it is deleted.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::time::UNIX_EPOCH;

use slint::{ComponentHandle as _, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use crate::{AppWindow, ImageFit, ImagePaneAdapter, ImagePaneItem};

/// The extensions `viewpane::kind_for_file` routes to `PaneKind::Image`. The list is
/// the `image` crate features enabled in Cargo.toml plus nothing: an `.svg` or `.ico`
/// would decode to an error notice, which is worse than the text viewer's NUL heuristic.
#[tracing::instrument(level = "debug", ret)]
pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// One successfully decoded file.
#[derive(Clone)]
pub struct Decoded {
    pub image: slint::Image,
    pub width: u32,
    pub height: u32,
    /// The file's size on disk, for the caption — not the decoded size.
    pub bytes: u64,
    /// The sniffed container ("PNG", "JPEG", ...), never the extension's claim.
    pub format: &'static str,
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoded")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes)
            .field("format", &self.format)
            .finish()
    }
}

/// Decode `path` to an RGBA8 Slint texture. Every failure — missing file, unknown
/// container, truncated data, a zero-sized image — is an `Err(String)` for the caption;
/// nothing here panics on bad bytes, which is the whole reason the decode goes through
/// the `image` crate's `Result`s rather than `Image::load_from_path`.
#[tracing::instrument(level = "debug", ret)]
pub fn decode(path: &Path) -> Result<Decoded, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let bytes = data.len() as u64;
    // Sniff the container from the bytes, never from the extension: a text file called
    // `notes.png` is "not a supported image format", not "Invalid PNG signature", and
    // `ImageReader::with_guessed_format` would fall back to the `.png` hint for it.
    let Ok(fmt) = image::guess_format(&data) else {
        return Err("not a supported image format".to_string());
    };
    let format = format_name(fmt);
    let rgba = image::load_from_memory_with_format(&data, fmt)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err("the image has no pixels".to_string());
    }
    let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), width, height);
    Ok(Decoded {
        image: slint::Image::from_rgba8(buf),
        width,
        height,
        bytes,
        format,
    })
}

#[tracing::instrument(level = "debug", ret)]
fn format_name(f: image::ImageFormat) -> &'static str {
    use image::ImageFormat as F;
    match f {
        F::Png => "PNG",
        F::Jpeg => "JPEG",
        F::Gif => "GIF",
        F::WebP => "WebP",
        F::Bmp => "BMP",
        F::Ico => "ICO",
        F::Tiff => "TIFF",
        _ => "image",
    }
}

/// Where and how large the image is drawn inside an `avail_w × avail_h` stage, in
/// logical pixels: scaled DOWN to fit, never up past 1:1 (a 40×20 icon in a 600×500
/// pane stays 40×20), aspect preserved, centred, then multiplied by the chord zoom.
/// Any non-positive input yields an all-zero fit rather than a NaN — a pane that has
/// not been laid out yet asks with 0×0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fit {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[tracing::instrument(level = "debug", ret)]
pub fn fit(img_w: f32, img_h: f32, avail_w: f32, avail_h: f32, zoom: f32) -> Fit {
    if !(img_w > 0.0 && img_h > 0.0 && avail_w > 0.0 && avail_h > 0.0) {
        return Fit::default();
    }
    let zoom = if zoom > 0.0 && zoom.is_finite() {
        zoom
    } else {
        1.0
    };
    // The clamp the plan's mutation check removes: without the `1.0` a small image
    // would be stretched to the stage.
    let scale = (avail_w / img_w).min(avail_h / img_h).min(1.0) * zoom;
    let w = img_w * scale;
    let h = img_h * scale;
    Fit {
        x: (avail_w - w) / 2.0,
        y: (avail_h - h) / 2.0,
        w,
        h,
    }
}

/// "12.3 KB" — one decimal above bytes, which is what Finder shows.
#[tracing::instrument(level = "debug", ret)]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for u in &UNITS[1..] {
        if v < 1024.0 {
            break;
        }
        v /= 1024.0;
        unit = u;
    }
    format!("{v:.1} {unit}")
}

/// The caption line under the image: "cat.png · 640×480 · 12.3 KB · PNG".
#[tracing::instrument(level = "debug", ret)]
pub fn caption(name: &str, width: u32, height: u32, bytes: u64, format: &str) -> String {
    format!(
        "{name} · {width}×{height} · {} · {format}",
        human_size(bytes)
    )
}

/// The image element's accessible name: file and dimensions, nothing a screen reader
/// would have to parse a unit out of.
#[tracing::instrument(level = "debug", ret)]
pub fn label(name: &str, width: u32, height: u32) -> String {
    format!("{name}, {width}×{height}")
}

/// The notice shown in place of the caption when the file cannot be decoded.
#[tracing::instrument(level = "debug", ret)]
pub fn error_caption(name: &str, err: &str) -> String {
    format!("Cannot show {name}: {err}")
}

/// The file name a caption names — the last path component, or the path itself when it
/// has none (a bare "/" is at least honest).
#[tracing::instrument(level = "debug", ret)]
pub fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// A 16×16 alpha mask with two opaque 8px quadrants. Drawn with `colorize: Theme.<tone>`
/// and repeat tiling it is the checkerboard behind a transparent image — the colour stays
/// in `theme.slint`, only the pattern lives here.
#[tracing::instrument(level = "debug")]
pub fn checker_tile() -> slint::Image {
    const N: u32 = 16;
    const HALF: u32 = N / 2;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(N, N);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % N, i as u32 / N);
        let on = (x < HALF) == (y < HALF);
        *px = Rgba8Pixel {
            r: 255,
            g: 255,
            b: 255,
            a: if on { 255 } else { 0 },
        };
    }
    slint::Image::from_rgba8(buf)
}

/// Build the row for pane `uid` showing `target`: decode, or carry the failure as the
/// caption. Pure apart from the disk read; `project` is the cached front of it.
#[tracing::instrument(level = "debug")]
pub fn item_for(uid: &str, target: Option<&str>) -> ImagePaneItem {
    let target = target.unwrap_or_default();
    let name = file_name(target);
    let base = ImagePaneItem {
        uid: uid.into(),
        ..Default::default()
    };
    if target.is_empty() {
        return ImagePaneItem {
            error: error_caption("this pane", "no file").into(),
            ..base
        };
    }
    match decode(Path::new(target)) {
        Ok(d) => ImagePaneItem {
            ok: true,
            image: d.image,
            w: d.width as i32,
            h: d.height as i32,
            label: label(&name, d.width, d.height).into(),
            caption: caption(&name, d.width, d.height, d.bytes, d.format).into(),
            ..base
        },
        Err(e) => ImagePaneItem {
            error: error_caption(&name, &e).into(),
            ..base
        },
    }
}

/// What a row was decoded from — path, mtime, length — so an edit is noticed and an
/// unchanged file is never decoded twice.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Fingerprint {
    target: String,
    mtime: u64,
    len: u64,
}

#[tracing::instrument(level = "debug", ret)]
fn fingerprint(target: Option<&str>) -> Fingerprint {
    let target = target.unwrap_or_default().to_string();
    let (mtime, len) = fs::metadata(&target)
        .ok()
        .map(|md| {
            let m = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m, md.len())
        })
        .unwrap_or((0, 0));
    Fingerprint { target, mtime, len }
}

struct Cache {
    /// Row index into `model` per pane uid, with what that row was decoded from.
    by_uid: HashMap<String, (usize, Fingerprint)>,
    model: Rc<VecModel<ImagePaneItem>>,
}

thread_local! {
    static IMAGES: RefCell<Cache> = RefCell::new(Cache {
        by_uid: HashMap::new(),
        model: Rc::new(VecModel::default()),
    });
}

/// Keep pane `uid`'s row current for `target`. Called from `pane_item`, so it must be
/// cheap when nothing changed: one `stat`, one lookup. Returns whether a decode ran —
/// the tests assert on that rather than on timing.
#[tracing::instrument(level = "debug", ret)]
pub fn project(uid: &str, target: Option<&str>) -> bool {
    let fp = fingerprint(target);
    IMAGES.with(|c| {
        let mut c = c.borrow_mut();
        if let Some((_, have)) = c.by_uid.get(uid) {
            if *have == fp {
                return false;
            }
        }
        let item = item_for(uid, target);
        match c.by_uid.get(uid).map(|(i, _)| *i) {
            Some(i) => c.model.set_row_data(i, item),
            None => c.model.push(item),
        }
        let i = c
            .by_uid
            .get(uid)
            .map(|(i, _)| *i)
            .unwrap_or(c.model.row_count() - 1);
        c.by_uid.insert(uid.to_string(), (i, fp));
        true
    })
}

/// Drop pane `uid`'s decoded texture. Nothing calls this yet — the tick loop that would
/// know a pane closed is not this track's file — so a closed image pane's texture lives
/// until the window does, exactly as `viewpane`'s row cache does.
#[allow(dead_code)]
#[tracing::instrument(level = "debug", ret)]
pub fn forget(uid: &str) -> bool {
    IMAGES.with(|c| {
        let mut c = c.borrow_mut();
        let Some((i, _)) = c.by_uid.remove(uid) else {
            return false;
        };
        c.model.remove(i);
        for (j, _) in c.by_uid.values_mut() {
            if *j > i {
                *j -= 1;
            }
        }
        true
    })
}

/// The row model, for the tests that read a caption back without a window.
#[cfg(test)]
pub(crate) fn row(uid: &str) -> Option<ImagePaneItem> {
    IMAGES.with(|c| {
        let c = c.borrow();
        c.by_uid.get(uid).and_then(|(i, _)| c.model.row_data(*i))
    })
}

/// Bind the adapter: the row model, the checker tile and the fit callback. Called once
/// per window by `paneview::Ui::attach`, and by every harness test that shows an image —
/// without the `fit` handler the callback returns an all-zero `ImageFit` and the image
/// is drawn 0×0, which is what the tests would then see.
#[tracing::instrument(level = "debug", skip(app))]
pub fn attach(app: &AppWindow) {
    let ad = app.global::<ImagePaneAdapter>();
    ad.set_images(IMAGES.with(|c| ModelRc::from(c.borrow().model.clone())));
    ad.set_checker(checker_tile());
    ad.on_fit(|iw, ih, aw, ah, zoom| {
        let f = fit(iw, ih, aw, ah, zoom);
        ImageFit {
            x: f.x,
            y: f.y,
            w: f.w,
            h: f.h,
        }
    });
}

#[cfg(test)]
pub(crate) mod testpng {
    //! A PNG built at test time, so the tests need no fixture files and no network.
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Encode a `w × h` RGBA PNG whose pixels are a plain gradient (the content is
    /// irrelevant; the dimensions are what the tests measure).
    pub fn encode(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().expect("png header");
            let px: Vec<u8> = (0..w * h)
                .flat_map(|i| [(i % 251) as u8, (i / 7 % 251) as u8, 64, 200])
                .collect();
            wr.write_image_data(&px).expect("png data");
        }
        out
    }

    /// A temp file that dies with the test; unique per process and per call.
    pub struct TempFile(pub PathBuf);

    impl TempFile {
        pub fn write(name: &str, bytes: &[u8]) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("avada-imagepane-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let p = dir.join(name);
            std::fs::write(&p, bytes).expect("temp png");
            TempFile(p)
        }
        pub fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 temp path")
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testpng::{encode, TempFile};
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    // ---- fit ----

    #[test]
    fn a_smaller_image_is_drawn_at_one_to_one_and_centred() {
        let f = fit(40.0, 20.0, 600.0, 500.0, 1.0);
        assert_eq!((f.w, f.h), (40.0, 20.0), "a 40×20 icon was upscaled: {f:?}");
        assert!(close(f.x, 280.0) && close(f.y, 240.0), "not centred: {f:?}");
    }

    #[test]
    fn a_larger_image_is_scaled_down_preserving_aspect() {
        let f = fit(800.0, 300.0, 600.0, 500.0, 1.0);
        assert!(close(f.w, 600.0), "did not fit the width: {f:?}");
        assert!(close(f.h, 225.0), "aspect not preserved: {f:?}");
        assert!(close(f.x, 0.0) && close(f.y, 137.5), "not centred: {f:?}");
        // The other axis binding.
        let f = fit(300.0, 800.0, 600.0, 400.0, 1.0);
        assert!(
            close(f.h, 400.0) && close(f.w, 150.0),
            "height-bound fit: {f:?}"
        );
    }

    #[test]
    fn a_zero_sized_stage_or_image_yields_nothing_not_nan() {
        for (iw, ih, aw, ah) in [
            (0.0, 0.0, 600.0, 500.0),
            (800.0, 300.0, 0.0, 0.0),
            (800.0, 300.0, 600.0, 0.0),
            (-1.0, 300.0, 600.0, 500.0),
        ] {
            let f = fit(iw, ih, aw, ah, 1.0);
            assert_eq!(f, Fit::default(), "{iw}×{ih} in {aw}×{ah} gave {f:?}");
        }
    }

    #[test]
    fn zoom_scales_the_fitted_size_and_a_bad_zoom_means_unzoomed() {
        let base = fit(800.0, 300.0, 600.0, 500.0, 1.0);
        let big = fit(800.0, 300.0, 600.0, 500.0, 2.0);
        assert!(
            close(big.w, base.w * 2.0) && close(big.h, base.h * 2.0),
            "{big:?}"
        );
        // Zoom is allowed past the stage — the human asked — but stays centred.
        assert!(close(big.x, (600.0 - big.w) / 2.0), "{big:?}");
        assert_eq!(fit(800.0, 300.0, 600.0, 500.0, 0.0), base);
        assert_eq!(fit(800.0, 300.0, 600.0, 500.0, f32::NAN), base);
    }

    // ---- caption ----

    #[test]
    fn the_caption_names_file_dimensions_size_and_format() {
        assert_eq!(
            caption("cat.png", 640, 480, 12_600, "PNG"),
            "cat.png · 640×480 · 12.3 KB · PNG"
        );
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(label("cat.png", 640, 480), "cat.png, 640×480");
        assert_eq!(file_name("/a/b/cat.png"), "cat.png");
        assert_eq!(file_name("/"), "/");
    }

    #[test]
    fn the_extension_list_is_what_the_build_can_decode() {
        for ext in ["png", "PNG", "jpg", "jpeg", "gif", "webp", "bmp"] {
            assert!(is_image_ext(ext), "{ext} should open as an image");
        }
        for ext in ["svg", "ico", "tiff", "rs", "md", ""] {
            assert!(!is_image_ext(ext), "{ext} must not open as an image");
        }
    }

    /// The routing arm in `viewpane::kind_for_file`: a bitmap opens here, whatever the
    /// case of its extension, and the things this build cannot decode keep their old
    /// pane rather than opening to an error notice.
    #[test]
    fn a_bitmap_activated_from_a_listing_opens_as_an_image_pane() {
        use crate::viewpane::kind_for_file;
        use avada_core::tools::PaneKind;
        for p in [
            "/a/cat.png",
            "/a/CAT.PNG",
            "/a/b.jpg",
            "/a/c.jpeg",
            "/a/d.gif",
            "/a/e.webp",
            "/a/f.bmp",
        ] {
            assert_eq!(kind_for_file(Path::new(p)), PaneKind::Image, "{p}");
        }
        assert_ne!(kind_for_file(Path::new("/a/logo.svg")), PaneKind::Image);
        assert_ne!(kind_for_file(Path::new("/a/main.rs")), PaneKind::Image);
    }

    // ---- decode ----

    #[test]
    fn a_two_by_two_png_decodes_to_its_dimensions() {
        let f = TempFile::write("tiny.png", &encode(2, 2));
        let d = decode(&f.0).expect("a valid png decodes");
        assert_eq!((d.width, d.height), (2, 2));
        assert_eq!(d.format, "PNG");
        assert_eq!(d.bytes, std::fs::metadata(&f.0).unwrap().len());
        let sz = d.image.size();
        assert_eq!((sz.width, sz.height), (2, 2));
    }

    #[test]
    fn a_truncated_png_is_an_error_not_a_panic() {
        let whole = encode(64, 64);
        let f = TempFile::write("cut.png", &whole[..whole.len() / 2]);
        let e = decode(&f.0).expect_err("half a png must not decode");
        assert!(!e.is_empty());
    }

    #[test]
    fn an_empty_file_a_text_file_and_a_missing_file_each_explain_themselves() {
        let empty = TempFile::write("empty.png", b"");
        assert!(decode(&empty.0).is_err());
        let text = TempFile::write("notes.png", b"hello, not a picture\n");
        assert_eq!(decode(&text.0).unwrap_err(), "not a supported image format");
        assert!(decode(Path::new("/nonexistent/avada-imagepane/x.png")).is_err());
    }

    #[test]
    fn the_checker_tile_is_a_two_quadrant_alpha_mask() {
        let t = checker_tile();
        assert_eq!((t.size().width, t.size().height), (16, 16));
    }

    // ---- item / cache ----

    #[test]
    fn an_item_carries_the_caption_or_the_error() {
        let f = TempFile::write("cat.png", &encode(40, 20));
        let it = item_for("u1", Some(f.path()));
        assert!(it.ok);
        assert_eq!((it.w, it.h), (40, 20));
        assert_eq!(it.label.as_str(), "cat.png, 40×20");
        assert!(
            it.caption.as_str().starts_with("cat.png · 40×20 · ") && it.caption.ends_with("· PNG"),
            "{}",
            it.caption
        );
        assert_eq!(it.error.as_str(), "");
        let bad = TempFile::write("bad.png", b"nope");
        let it = item_for("u2", Some(bad.path()));
        assert!(!it.ok);
        assert_eq!((it.w, it.h), (0, 0));
        assert_eq!(
            it.error.as_str(),
            "Cannot show bad.png: not a supported image format"
        );
        let none = item_for("u3", None);
        assert!(!none.ok && none.error.as_str().contains("no file"));
    }

    #[test]
    fn project_decodes_once_per_file_state_and_notices_an_edit() {
        let f = TempFile::write("cat.png", &encode(8, 8));
        assert!(project("p-cache", Some(f.path())), "first sight decodes");
        assert!(!project("p-cache", Some(f.path())), "same file: cached");
        assert_eq!(row("p-cache").map(|r| (r.w, r.h)), Some((8, 8)));
        // A rewrite with a different length is a different fingerprint.
        std::fs::write(&f.0, encode(16, 4)).unwrap();
        assert!(project("p-cache", Some(f.path())), "an edit re-decodes");
        assert_eq!(row("p-cache").map(|r| (r.w, r.h)), Some((16, 4)));
        // Two panes on two files keep separate rows; forgetting one leaves the other.
        let g = TempFile::write("dog.png", &encode(3, 5));
        assert!(project("p-other", Some(g.path())));
        assert!(forget("p-cache"));
        assert!(!forget("p-cache"), "already gone");
        assert_eq!(row("p-other").map(|r| (r.w, r.h)), Some((3, 5)));
        assert!(forget("p-other"));
    }
}
