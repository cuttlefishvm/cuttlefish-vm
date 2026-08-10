//! Host-side image transforms: the operations that need a decoder.
//!
//! The dividing line this module sits on is the same one `pdf-render` sits
//! on. Reading what an image *says about itself* — dimensions, EXIF, chunk
//! structure — is header parsing, cheap, and available to any script in the
//! guest with no feature flag and no round trip. Reading what an image
//! *looks like* means decoding pixels, which is megabytes of codec that has
//! no business in a wasm guest shipped to every user.
//!
//! So this is behind the `image-ops` feature, off by default. A host built
//! without it rejects [`cuttlefish_abi::Command::ImageOp`] with a message
//! saying exactly that, rather than silently returning the original image —
//! which would look like a working pipeline producing quietly wrong results.

/// Apply one operation, returning re-encoded PNG bytes.
///
/// PNG on the way out regardless of what came in: it is lossless, so a crop
/// fed to a vision model is not also silently re-compressed, and it is what
/// `PageImage` already produces, so every image handle in the system has one
/// encoding rather than two.
#[cfg(feature = "image-ops")]
pub fn apply(bytes: &[u8], op: &cuttlefish_abi::ImageOperation) -> Result<Vec<u8>, anyhow::Error> {
    use cuttlefish_abi::ImageOperation;
    use image::GenericImageView as _;

    let decoded =
        image::load_from_memory(bytes).map_err(|e| anyhow::anyhow!("decoding the image: {e}"))?;

    let out = match op {
        ImageOperation::Resize {
            max_width,
            max_height,
        } => {
            if *max_width == 0 || *max_height == 0 {
                anyhow::bail!("resize bounds must both be non-zero");
            }
            // `thumbnail` preserves aspect ratio but *will* enlarge a small
            // image to fill the box, so the bounds are clamped to the
            // original first. "Fit within these bounds" must never mean
            // upscale: invented detail is detail a vision model then
            // reasons about as though it were real.
            let (w, h) = decoded.dimensions();
            decoded.thumbnail((*max_width).min(w), (*max_height).min(h))
        }
        ImageOperation::Crop {
            x,
            y,
            width,
            height,
        } => {
            if *width == 0 || *height == 0 {
                anyhow::bail!("crop width and height must both be non-zero");
            }
            let (w, h) = decoded.dimensions();
            // Refuse rather than clamp. A crop that silently returns a
            // different region than asked for is worse than an error: the
            // caller reasons about coordinates it did not get.
            if x.saturating_add(*width) > w || y.saturating_add(*height) > h {
                anyhow::bail!(
                    "crop region {width}x{height}+{x}+{y} does not fit inside the \
                     image's {w}x{h}"
                );
            }
            decoded.crop_imm(*x, *y, *width, *height)
        }
    };

    let mut encoded = std::io::Cursor::new(Vec::new());
    out.write_to(&mut encoded, image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("re-encoding the image: {e}"))?;
    Ok(encoded.into_inner())
}

/// Without the feature, say so plainly.
///
/// Not a silent pass-through of the original bytes: that would produce a
/// pipeline that appears to work while every crop and resize quietly did
/// nothing, and the caller would have no way to tell.
#[cfg(not(feature = "image-ops"))]
pub fn apply(
    _bytes: &[u8],
    _op: &cuttlefish_abi::ImageOperation,
) -> Result<Vec<u8>, anyhow::Error> {
    anyhow::bail!(
        "this cuttlefish was built without the `image-ops` feature, so it cannot decode \
         images — rebuild with `--features image-ops`. Image *metadata* (dimensions, EXIF, \
         chunk structure) needs no feature and is available to any script via the \
         `dimensions`/`exif`/`png_chunks` builtins."
    )
}

#[cfg(all(test, feature = "image-ops"))]
mod tests {
    use super::*;
    use cuttlefish_abi::ImageOperation;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let img =
            image::RgbImage::from_fn(width, height, |x, _| image::Rgb([(x % 256) as u8, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn size_of(bytes: &[u8]) -> (u32, u32) {
        use image::GenericImageView as _;
        image::load_from_memory(bytes).unwrap().dimensions()
    }

    #[test]
    fn resize_fits_within_bounds_and_keeps_the_aspect_ratio() {
        let out = apply(
            &png(800, 400),
            &ImageOperation::Resize {
                max_width: 100,
                max_height: 100,
            },
        )
        .unwrap();
        // 2:1 fitted into a square box is 100x50, not 100x100 — a stretched
        // image would make a vision model describe the wrong shapes.
        assert_eq!(size_of(&out), (100, 50));
    }

    #[test]
    fn resize_never_enlarges() {
        // Upscaling invents detail the model would then reason about.
        let out = apply(
            &png(10, 10),
            &ImageOperation::Resize {
                max_width: 500,
                max_height: 500,
            },
        )
        .unwrap();
        assert_eq!(size_of(&out), (10, 10));
    }

    #[test]
    fn crop_returns_exactly_the_requested_region() {
        let out = apply(
            &png(100, 100),
            &ImageOperation::Crop {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            },
        )
        .unwrap();
        assert_eq!(size_of(&out), (30, 40));
    }

    #[test]
    fn a_crop_outside_the_image_is_refused_rather_than_clamped() {
        // Silently returning a different region than asked for is worse
        // than an error: the caller reasons about coordinates it never got.
        let err = apply(
            &png(50, 50),
            &ImageOperation::Crop {
                x: 40,
                y: 40,
                width: 30,
                height: 30,
            },
        )
        .expect_err("an out-of-bounds crop must be refused");
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    #[test]
    fn zero_sized_operations_are_refused() {
        assert!(apply(
            &png(10, 10),
            &ImageOperation::Resize {
                max_width: 0,
                max_height: 10
            }
        )
        .is_err());
        assert!(apply(
            &png(10, 10),
            &ImageOperation::Crop {
                x: 0,
                y: 0,
                width: 0,
                height: 5
            }
        )
        .is_err());
    }

    #[test]
    fn undecodable_bytes_error_rather_than_panic() {
        let err = apply(
            b"not an image at all",
            &ImageOperation::Resize {
                max_width: 10,
                max_height: 10,
            },
        )
        .expect_err("garbage must not decode");
        assert!(err.to_string().contains("decoding"), "{err}");
    }
}
