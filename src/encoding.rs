//! Encoding helpers to convert Photon images into client-requested formats.

use bytes::Bytes;
use photon_rs::PhotonImage;

#[derive(Debug, Clone)]
pub enum EncodeImageError {
    UnsupportedFormat(String),
    EncodeFailed(String),
}

impl std::fmt::Display for EncodeImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(fmt) => write!(f, "unsupported format '{fmt}'"),
            Self::EncodeFailed(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for EncodeImageError {}

/// Re-encode the processed Photon image into the requested format.
pub fn encode_image(
    image: &PhotonImage,
    format: &str,
    quality: u8,
) -> Result<(String, Bytes), EncodeImageError> {
    use image::{
        ColorType, ImageOutputFormat,
        codecs::{
            jpeg::JpegEncoder,
            webp::{WebPEncoder, WebPQuality},
        },
    };
    use photon_rs::helpers::dyn_image_from_raw;
    use std::io::Cursor;

    let dyn_img = dyn_image_from_raw(image);
    let format_lc = format.to_lowercase();
    let mut cursor = Cursor::new(Vec::new());

    match format_lc.as_str() {
        "png" => {
            dyn_img
                .write_to(&mut cursor, ImageOutputFormat::Png)
                .map_err(|err| EncodeImageError::EncodeFailed(err.to_string()))?;
            Ok(("image/png".to_string(), Bytes::from(cursor.into_inner())))
        }
        "jpeg" | "jpg" => {
            let rgba = dyn_img.to_rgba8();
            let (width, height) = rgba.dimensions();
            let data = rgba.into_raw();
            let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality);
            encoder
                .encode(&data, width, height, ColorType::Rgba8)
                .map_err(|err| EncodeImageError::EncodeFailed(err.to_string()))?;
            Ok(("image/jpeg".to_string(), Bytes::from(cursor.into_inner())))
        }
        "webp" => {
            let rgba = dyn_img.to_rgba8();
            let (width, height) = rgba.dimensions();
            let data = rgba.into_raw();
            if quality == 100 {
                WebPEncoder::new_lossless(&mut cursor)
                    .encode(&data, width, height, ColorType::Rgba8)
                    .map_err(|err| EncodeImageError::EncodeFailed(err.to_string()))?;
            } else {
                #[allow(deprecated)]
                WebPEncoder::new_with_quality(&mut cursor, WebPQuality::lossy(quality))
                    .encode(&data, width, height, ColorType::Rgba8)
                    .map_err(|err| EncodeImageError::EncodeFailed(err.to_string()))?;
            }
            Ok(("image/webp".to_string(), Bytes::from(cursor.into_inner())))
        }
        other => Err(EncodeImageError::UnsupportedFormat(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageOutputFormat, Rgba};
    use photon_rs::PhotonImage;
    use std::io::Cursor;

    fn sample_photon_image() -> PhotonImage {
        PhotonImage::new(
            vec![
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255, //
                255, 255, 0, 255,
            ],
            2,
            2,
        )
    }

    #[test]
    fn encode_image_supports_formats() {
        let image = sample_photon_image();
        let (mime_png, data_png) =
            encode_image(&image, "png", 100).expect("png encoding should work");
        assert_eq!(mime_png, "image/png");
        assert!(!data_png.is_empty());

        let (mime_jpeg, data_jpeg) =
            encode_image(&image, "jpeg", 90).expect("jpeg encoding should work");
        assert_eq!(mime_jpeg, "image/jpeg");
        assert!(!data_jpeg.is_empty());

        let (mime_webp, data_webp) =
            encode_image(&image, "webp", 90).expect("webp encoding should work");
        assert_eq!(mime_webp, "image/webp");
        assert!(!data_webp.is_empty());
    }

    #[test]
    fn encode_image_rejects_unknown_format() {
        let image = sample_photon_image();
        let err = encode_image(&image, "gif", 100).expect_err("gif should not be supported");
        match err {
            EncodeImageError::UnsupportedFormat(fmt) => assert_eq!(fmt, "gif"),
            _ => panic!("expected unsupported format error"),
        }
    }

    #[test]
    fn encode_image_respects_quality_levels() {
        let image = sample_photon_image();
        let (_, high_quality) =
            encode_image(&image, "jpeg", 100).expect("high quality jpeg should encode");
        let (_, low_quality) =
            encode_image(&image, "jpeg", 30).expect("low quality jpeg should encode");
        assert!(
            low_quality.len() <= high_quality.len(),
            "lower quality should not increase size"
        );
    }

    #[test]
    fn png_encoding_helper_builds_valid_samples() {
        // Ensure helper used in other tests produces decodable PNG bytes.
        let image = ImageBuffer::<Rgba<u8>, _>::from_fn(2, 2, |x, y| match (x, y) {
            (0, 0) => Rgba([255, 0, 0, 255]),
            (1, 0) => Rgba([0, 255, 0, 255]),
            (0, 1) => Rgba([0, 0, 255, 255]),
            _ => Rgba([255, 255, 0, 255]),
        });
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, ImageOutputFormat::Png)
            .unwrap();
        assert!(!cursor.into_inner().is_empty());
    }
}
