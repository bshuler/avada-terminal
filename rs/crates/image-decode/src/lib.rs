//! `avada-image-decode`: sniff an image container from its bytes and decode it to RGBA8.
//!
//! This is the one decoder in the tree. Both the app's built-in image view
//! (`crates/app/src/imagepane.rs`, which wraps the RGBA in a Slint texture) and the
//! `avada-image` module (which base64-ships the RGBA to the host over the wire) call
//! [`decode`], so the format sniffing, the error taxonomy and the extension list cannot
//! drift between the two paths.
//!
//! The IR is deliberately neutral: [`Decoded`] is a plain `Vec<u8>` of RGBA plus its
//! dimensions and metadata, with **no** dependency on Slint or any UI toolkit. The host
//! turns it into a `SharedPixelBuffer`; the module base64-encodes it; the crate itself
//! knows about neither.

/// The extensions the build can decode — the `image` crate features enabled in this
/// crate's `Cargo.toml` and nothing else. An `.svg` or `.ico` is deliberately excluded:
/// it would decode to an error notice, which is worse than leaving it to the text viewer.
///
/// Both the host's `viewpane::kind_for_file` routing and the module's manifest `opens`
/// list are checked against this function, so a file that routes to an image pane is
/// always one this decoder can actually read.
#[tracing::instrument(level = "debug", ret)]
pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// One successfully decoded image: its pixels as tightly packed RGBA8 (`width * height * 4`
/// bytes, row-major, no padding) plus the metadata a caption needs.
#[derive(Clone, PartialEq, Eq)]
pub struct Decoded {
    /// RGBA8 pixels, `width * height * 4` bytes, row-major.
    pub rgba: Vec<u8>,
    /// Decoded width in pixels.
    pub width: u32,
    /// Decoded height in pixels.
    pub height: u32,
    /// The size of the encoded input in bytes, for the caption — not the decoded size.
    pub bytes: u64,
    /// The sniffed container ("PNG", "JPEG", ...), never the extension's claim.
    pub format: &'static str,
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The rgba buffer is megabytes; naming its length keeps `ret` instrumentation
        // and test failures readable.
        f.debug_struct("Decoded")
            .field("rgba_len", &self.rgba.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes)
            .field("format", &self.format)
            .finish()
    }
}

/// Decode `data` to RGBA8. Every failure — unknown container, truncated data, a
/// zero-sized image — is an `Err(String)` suitable for a caption; nothing here panics on
/// bad bytes, which is the whole reason the decode goes through the `image` crate's
/// `Result`s rather than a load-from-path shortcut.
#[tracing::instrument(level = "debug", ret, skip(data))]
pub fn decode(data: &[u8]) -> Result<Decoded, String> {
    let bytes = data.len() as u64;
    // Sniff the container from the bytes, never from an extension: a text file called
    // `notes.png` is "not a supported image format", not "Invalid PNG signature".
    let Ok(fmt) = image::guess_format(data) else {
        return Err("not a supported image format".to_string());
    };
    let format = format_name(fmt);
    let rgba = image::load_from_memory_with_format(data, fmt)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err("the image has no pixels".to_string());
    }
    Ok(Decoded {
        rgba: rgba.into_raw(),
        width,
        height,
        bytes,
        format,
    })
}

/// The display name of a sniffed container. Kept exhaustive over the formats `image` can
/// name so a build that enables another feature only has to add its arm, not hunt for a
/// silent `"image"` fallback.
#[tracing::instrument(level = "debug", ret)]
pub fn format_name(f: image::ImageFormat) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w × h` RGBA PNG built at test time, so the tests need no fixture files.
    fn png(w: u32, h: u32) -> Vec<u8> {
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

    #[test]
    fn a_two_by_two_png_decodes_to_its_dimensions_and_rgba() {
        let data = png(2, 2);
        let d = decode(&data).expect("a valid png decodes");
        assert_eq!((d.width, d.height), (2, 2));
        assert_eq!(d.format, "PNG");
        assert_eq!(d.bytes, data.len() as u64);
        assert_eq!(d.rgba.len(), 2 * 2 * 4, "tightly packed rgba");
    }

    #[test]
    fn a_truncated_png_is_an_error_not_a_panic() {
        let whole = png(64, 64);
        let e = decode(&whole[..whole.len() / 2]).expect_err("half a png must not decode");
        assert!(!e.is_empty());
    }

    #[test]
    fn an_empty_input_and_a_text_input_each_explain_themselves() {
        assert!(decode(b"").is_err());
        assert_eq!(
            decode(b"hello, not a picture\n").unwrap_err(),
            "not a supported image format"
        );
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

    #[test]
    fn format_names_are_the_sniffed_container() {
        assert_eq!(format_name(image::ImageFormat::Png), "PNG");
        assert_eq!(format_name(image::ImageFormat::Jpeg), "JPEG");
        assert_eq!(format_name(image::ImageFormat::WebP), "WebP");
    }
}
