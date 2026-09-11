//! The picture a tier-5 module ships decoded (contract `host.image.set`), the second
//! reader-only shape of tier 5.
//!
//! A [document](super::doc) is a stream of blocks the host typesets; an *image* is one
//! already-decoded texture the host only places. This is the deliberate exception to the
//! doc rule that a module ships source and never pixels — a picture has no source the host
//! could re-render from, so the module (through the shared `avada-image-decode` crate, the
//! same decode the built-in [image view](crate::imagepane) runs) sniffs and decodes the file
//! and hands over the finished RGBA. **The module owns the decode, the host owns the
//! geometry**: the fit-never-upscale clamp, the zoom chord, the caption, the checkerboard all
//! stay in [`crate::imagepane`], drawn one step past where the doc split ends.
//!
//! This is the store, not the projection. An image replaces an image — the contract has no
//! damage list, for the same reason a [doc](super::doc) does not: the two processes restart
//! independently and a whole picture is the only statement true no matter what the other side
//! missed. What turns the stored [`ImageData`] into the pane's texture is
//! [`crate::imagepane::project_module`], reached from `pane_item`.
//!
//! Thread-local, exactly like [`super::doc`], [`super::rows`] and [`super::grid`]: this is
//! window-thread UI state, and the fold-in, the projection and the pane's cache all already
//! run there.

use avada_core::module::host::ImageData;
use avada_core::rights::ModuleId;
use std::cell::RefCell;
use std::collections::HashMap;

/// One surface's last picture plus the counter the pane's cache watches. Same shape and same
/// reason as [`super::doc::Surface`]: an image is not a file, so there is no mtime to compare
/// — the revision is the only thing that says the content moved.
#[derive(Default)]
struct Surface {
    image: ImageData,
    revision: u64,
}

thread_local! {
    static SURFACES: RefCell<HashMap<String, Surface>> = RefCell::new(HashMap::new());
}

/// The store key, the same `<owner/repo>#<id>` shape the rail, the row store, the grid store
/// and the doc store use, so the stores are read the same way even though they never share an
/// entry for one surface.
fn key(module: &ModuleId, surface: &str) -> String {
    crate::leftpanel::entry_key(module, surface)
}

/// Replace `image.surface`'s picture. Pictures replace pictures — the contract has no
/// incremental update, because a module that restarted cannot know what the host still has on
/// screen and a whole picture is the only message correct from both sides of a restart.
pub fn set(module: &ModuleId, image: ImageData) {
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(key(module, &image.surface)).or_default();
        e.image = image;
        e.revision += 1;
    });
}

/// What `surface` last shipped; `None` when it has never spoken. An error picture (no pixels,
/// a caption) is a real message (`Some`), distinct from a surface that never sent one.
pub fn image(module: &ModuleId, surface: &str) -> Option<ImageData> {
    SURFACES.with(|s| s.borrow().get(&key(module, surface)).map(|e| e.image.clone()))
}

/// How many pictures `surface` has shipped. Zero for a surface that never has — the same
/// value every non-image pane reports, so `pane_item` treats "never spoke" and "not an image
/// pane" alike, and only a surface that has shipped at least one picture draws as one.
pub fn generation(module: &ModuleId, surface: &str) -> u64 {
    SURFACES.with(|s| {
        s.borrow()
            .get(&key(module, surface))
            .map_or(0, |e| e.revision)
    })
}

/// Drop every picture belonging to `module`. Called when the host says the module is gone: a
/// dead module's last picture must not keep sitting on screen as if it were live.
///
/// Cleared, not removed, for the reason [`super::doc::forget`] clears: the pane is still open
/// and its projection only notices a *new* revision, so removing the entry would let the cache
/// hand back the picture the module left behind.
pub fn forget(module: &ModuleId) {
    let prefix = format!("{}#", module.as_str());
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        for k in s
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(e) = s.get_mut(&k) {
                e.image = ImageData::default();
                e.revision += 1;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("a valid module id")
    }

    fn pic(surface: &str, w: u32, h: u32) -> ImageData {
        ImageData {
            surface: surface.into(),
            name: "x.png".into(),
            width: w,
            height: h,
            format: "PNG".into(),
            bytes: 1,
            rgba: vec![0u8; (w * h * 4) as usize],
            error: String::new(),
        }
    }

    /// An image surface is per-module and per-id, exactly like the doc store: the store must
    /// not let one module's `image` be another's, nor two surfaces of one module collide.
    #[test]
    fn a_surface_is_per_module_and_per_id() {
        let (a, b) = (id("bshuler/avada-image"), id("acme/avada-pics"));
        set(&a, pic("image", 2, 2));
        set(&b, pic("image", 4, 4));
        set(&a, pic("other", 1, 1));
        assert_eq!(image(&a, "image").unwrap().width, 2);
        assert_eq!(image(&b, "image").unwrap().width, 4);
        assert_eq!(image(&a, "other").unwrap().width, 1);
        assert!(
            image(&a, "never-spoken").is_none(),
            "a surface that never spoke has no picture at all"
        );
    }

    /// The revision is the cache key: a picture that changed without it moving would leave the
    /// pane showing the previous one forever, and a gone module must empty its panes.
    #[test]
    fn every_replacement_moves_the_revision_and_a_gone_module_empties_its_panes() {
        let m = id("bshuler/avada-image");
        assert_eq!(generation(&m, "image"), 0, "an unspoken surface is at zero");
        set(&m, pic("image", 8, 8));
        let first = generation(&m, "image");
        assert!(first > 0);
        // Same picture, said again: still a new revision. The store cannot tell an idempotent
        // resend from a real change without comparing megabytes, and redrawing is cheaper.
        set(&m, pic("image", 8, 8));
        assert!(generation(&m, "image") > first);

        let before = generation(&m, "image");
        forget(&m);
        assert!(
            image(&m, "image").unwrap().rgba.is_empty(),
            "a dead module keeps no pixels"
        );
        assert!(
            generation(&m, "image") > before,
            "and the emptying is itself a change the projection must see"
        );
    }
}
