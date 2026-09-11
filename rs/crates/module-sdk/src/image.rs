//! The image surface a module fills with a picture (UI tier 5, one-directional).
//!
//! This is the deliberate exception to the [document](crate::doc) rule that a module ships
//! source and never pixels. A picture has no source the host could re-render from — it
//! *is* its pixels — so the module that owns an image surface sniffs and decodes the file
//! (through the shared `avada-image-decode` crate, the same decode the host's built-in
//! image view runs) and ships the finished RGBA. The host parks it as a texture and keeps
//! everything downstream of the pixels: the fit-never-upscale geometry, the zoom chord,
//! the caption, the transparency checkerboard.
//!
//! **The module owns the decode; the host owns the geometry.** The split is the same one
//! [`crate::doc`] and [`crate::grid`] strike, drawn one step later: for a document the
//! host renders the blocks, for a grid the host lays out the cells, for an image the host
//! has nothing left to render but everything left to *place*.
//!
//! Like a doc, an image surface has no `module.image.*` reply — a rendered picture takes
//! no keystrokes — and is not incremental: a [`SetImage`] replaces a [`SetImage`], sent
//! host ← module through [`crate::contract::methods::HOST_IMAGE_SET`].
//!
//! A decode the module could not complete is not a protocol fault — a truncated file, an
//! unsupported container — so it rides [`SetImage::error`] rather than tearing the module
//! down, and the host shows the reader a caption in place of the picture. Exactly one of
//! [`SetImage::rgba_b64`] and [`SetImage::error`] carries the payload; the other is empty.

use serde::{Deserialize, Serialize};

/// A whole image: `host.image.set` params.
///
/// Sending a new [`SetImage`] for the same `surface` replaces the last one outright.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetImage {
    /// The surface id — the same contribution id the pane was spawned with.
    pub surface: String,
    /// The file name for the caption ("cat.png"), not the full path.
    #[serde(default)]
    pub name: String,
    /// Decoded width in pixels. Zero on an error.
    #[serde(default)]
    pub width: u32,
    /// Decoded height in pixels. Zero on an error.
    #[serde(default)]
    pub height: u32,
    /// The sniffed container ("PNG", "JPEG", ...) for the caption. Empty on an error.
    #[serde(default)]
    pub format: String,
    /// The size of the encoded file in bytes, for the caption. Zero on an error.
    #[serde(default)]
    pub bytes: u64,
    /// The decoded pixels: `width * height * 4` bytes of row-major RGBA8, base64
    /// (STANDARD). Empty when [`error`](Self::error) is set.
    #[serde(default)]
    pub rgba_b64: String,
    /// The reason the file could not be shown, or empty on success. When set, the host
    /// shows this as the caption and draws no picture.
    #[serde(default)]
    pub error: String,
}

impl std::fmt::Debug for SetImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The base64 payload is megabytes; naming its length keeps the Debug readable.
        f.debug_struct("SetImage")
            .field("surface", &self.surface)
            .field("name", &self.name)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("bytes", &self.bytes)
            .field("rgba_b64_len", &self.rgba_b64.len())
            .field("error", &self.error)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setimage_round_trips_through_json() {
        let img = SetImage {
            surface: "image".into(),
            name: "cat.png".into(),
            width: 2,
            height: 2,
            format: "PNG".into(),
            bytes: 84,
            rgba_b64: "AAAA".into(),
            error: String::new(),
        };
        let json = serde_json::to_value(&img).expect("serialize");
        let back: SetImage = serde_json::from_value(json).expect("deserialize");
        assert_eq!(img, back);
    }

    #[test]
    fn an_error_setimage_carries_no_pixels() {
        let img = SetImage {
            surface: "image".into(),
            name: "bad.png".into(),
            error: "not a supported image format".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&img).expect("serialize");
        let back: SetImage = serde_json::from_value(json).expect("deserialize");
        assert!(back.rgba_b64.is_empty() && !back.error.is_empty());
        assert_eq!((back.width, back.height), (0, 0));
    }
}
