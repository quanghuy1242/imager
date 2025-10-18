//! Parsing and execution of image processing operations requested by clients.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, ensure};
use photon_rs::{
    PhotonImage,
    conv::gaussian_blur,
    monochrome::grayscale,
    transform::{SamplingFilter, fliph, flipv, resize, rotate},
};

use crate::constants::MAX_DIMENSION;

/// Rich AST describing the image operations requested by the client.
#[derive(Debug, Clone)]
pub enum Operation {
    Resize { width: u32, height: u32 },
    Blur { radius: i32 },
    Grayscale,
    FlipHorizontal,
    FlipVertical,
    Rotate { degrees: f32 },
    ResizeKeepAspectWidth { width: u32 },
    ResizeKeepAspectHeight { height: u32 },
}

pub fn parse_operations(raw: &str) -> Result<Vec<Operation>> {
    raw.split('|')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(parse_operation)
        .collect()
}

fn parse_operation(segment: &str) -> Result<Operation> {
    let mut parts = segment.splitn(2, ':');
    let name = parts
        .next()
        .ok_or_else(|| anyhow!("missing operation name"))?
        .trim()
        .to_lowercase();
    let args = parts.next().map(str::trim).unwrap_or("");

    match name.as_str() {
        "grayscale" => Ok(Operation::Grayscale),
        "blur" => {
            let radius = if args.is_empty() {
                3
            } else {
                let params = parse_key_value_args(args);
                let value = params
                    .get("radius")
                    .or_else(|| params.get("r"))
                    .or_else(|| params.get("sigma"))
                    .map(|value| value.to_owned())
                    .unwrap_or_else(|| args.to_string());
                let parsed: f32 = value
                    .parse()
                    .context("blur radius must be a positive number")?;
                let rounded = parsed.round() as i32;
                if rounded <= 0 {
                    return Err(anyhow!("blur radius must be greater than zero"));
                }
                rounded
            };
            Ok(Operation::Blur { radius })
        }
        "flip" => match args.to_lowercase().as_str() {
            "horizontal" | "h" => Ok(Operation::FlipHorizontal),
            "vertical" | "v" => Ok(Operation::FlipVertical),
            other => Err(anyhow!("unsupported flip variant '{other}'")),
        },
        "rotate" => {
            if args.is_empty() {
                return Err(anyhow!(
                    "rotate expects an angle in degrees (e.g. rotate:90 or rotate:deg=45)"
                ));
            }
            let params = parse_key_value_args(args);
            let value = params
                .get("deg")
                .or_else(|| params.get("degrees"))
                .map(|value| value.to_owned())
                .unwrap_or_else(|| args.to_string());
            let degrees: f32 = value
                .parse()
                .context("rotate angle must be a valid number")?;
            Ok(Operation::Rotate { degrees })
        }
        "resize" => parse_resize(args),
        "ratio" => parse_ratio(args),
        other => Err(anyhow!("unsupported operation '{other}'")),
    }
}

/// Parse resize arguments in either `WxH` or `key=value` form.
fn parse_resize(args: &str) -> Result<Operation> {
    if args.is_empty() {
        return Err(anyhow!(
            "resize expects arguments (e.g. resize:width=800,height=600 or resize:800x600)"
        ));
    }

    if let Some((width, height)) = parse_dimensions(args) {
        validate_dimensions(width, height)?;
        return Ok(Operation::Resize { width, height });
    }

    let params = parse_key_value_args(args);
    let width = params
        .get("width")
        .or_else(|| params.get("w"))
        .ok_or_else(|| anyhow!("resize missing width parameter"))?
        .parse::<u32>()
        .context("resize width must be positive integer")?;
    let height = params
        .get("height")
        .or_else(|| params.get("h"))
        .ok_or_else(|| anyhow!("resize missing height parameter"))?
        .parse::<u32>()
        .context("resize height must be positive integer")?;
    validate_dimensions(width, height)?;

    Ok(Operation::Resize { width, height })
}

/// Interpret compact `800x600` style values.
fn parse_dimensions(value: &str) -> Option<(u32, u32)> {
    let (width_str, height_str) = value.split_once('x')?;
    let width = width_str.trim().parse().ok()?;
    let height = height_str.trim().parse().ok()?;
    Some((width, height))
}

/// Convert CSV-style `key=value` arguments into a lookup map.
fn parse_key_value_args(args: &str) -> HashMap<String, String> {
    args.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((key.trim().to_lowercase(), value.trim().to_string()))
        })
        .collect()
}

/// Confirm requested dimensions are non-zero and within `MAX_DIMENSION`.
fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(anyhow!("resize width and height must be greater than zero"));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(anyhow!(
            "resize dimensions exceed limit of {MAX_DIMENSION} pixels"
        ));
    }
    Ok(())
}

fn parse_ratio(args: &str) -> Result<Operation> {
    if args.is_empty() {
        return Err(anyhow!(
            "ratio expects one dimension (e.g. ratio:width=800,keep=true or ratio:h=600)"
        ));
    }

    let params = parse_key_value_args(args);
    let keep_flag = params
        .get("keep")
        .or_else(|| params.get("keep_ratio"))
        .map(|value| value.to_lowercase());

    if let Some(flag) = keep_flag {
        match flag.as_str() {
            "true" | "1" | "yes" => {}
            "false" | "0" | "no" => {
                return Err(anyhow!(
                    "ratio operation requires keep flag to be true; omit the flag to assume true"
                ));
            }
            other => {
                return Err(anyhow!(
                    "ratio keep flag must be a boolean value, got '{other}'"
                ));
            }
        }
    }

    let width_param = params.get("width").or_else(|| params.get("w"));
    let height_param = params.get("height").or_else(|| params.get("h"));

    match (width_param, height_param) {
        (Some(width_str), None) => {
            let width = width_str
                .parse::<u32>()
                .context("ratio width must be positive integer")?;
            if width == 0 {
                return Err(anyhow!("ratio width must be greater than zero"));
            }
            if width > MAX_DIMENSION {
                return Err(anyhow!(
                    "ratio width exceeds limit of {MAX_DIMENSION} pixels"
                ));
            }
            Ok(Operation::ResizeKeepAspectWidth { width })
        }
        (None, Some(height_str)) => {
            let height = height_str
                .parse::<u32>()
                .context("ratio height must be positive integer")?;
            if height == 0 {
                return Err(anyhow!("ratio height must be greater than zero"));
            }
            if height > MAX_DIMENSION {
                return Err(anyhow!(
                    "ratio height exceeds limit of {MAX_DIMENSION} pixels"
                ));
            }
            Ok(Operation::ResizeKeepAspectHeight { height })
        }
        (Some(_), Some(_)) => Err(anyhow!(
            "ratio expects exactly one dimension (width or height), but both were provided"
        )),
        (None, None) => Err(anyhow!(
            "ratio expects a width or height (e.g. ratio:width=800)"
        )),
    }
}

/// Execute the parsed operation sequence on the given image buffer.
pub fn apply_operations(image: &mut PhotonImage, operations: &[Operation]) -> Result<()> {
    for operation in operations {
        match operation {
            Operation::Resize { width, height } => {
                *image = resize(image, *width, *height, SamplingFilter::Lanczos3);
            }
            Operation::ResizeKeepAspectWidth { width } => {
                ensure!(
                    image.get_width() > 0,
                    "cannot maintain aspect ratio on zero-width image"
                );
                let scale = *width as f32 / image.get_width() as f32;
                let mut new_height = (image.get_height() as f32 * scale).round();
                if new_height < 1.0 {
                    new_height = 1.0;
                }
                let new_height_u32 = new_height as u32;
                validate_dimensions(*width, new_height_u32)?;
                *image = resize(image, *width, new_height_u32, SamplingFilter::Lanczos3);
            }
            Operation::ResizeKeepAspectHeight { height } => {
                ensure!(
                    image.get_height() > 0,
                    "cannot maintain aspect ratio on zero-height image"
                );
                let scale = *height as f32 / image.get_height() as f32;
                let mut new_width = (image.get_width() as f32 * scale).round();
                if new_width < 1.0 {
                    new_width = 1.0;
                }
                let new_width_u32 = new_width as u32;
                validate_dimensions(new_width_u32, *height)?;
                *image = resize(image, new_width_u32, *height, SamplingFilter::Lanczos3);
            }
            Operation::Blur { radius } => {
                gaussian_blur(image, *radius);
            }
            Operation::Grayscale => {
                grayscale(image);
            }
            Operation::FlipHorizontal => {
                fliph(image);
            }
            Operation::FlipVertical => {
                flipv(image);
            }
            Operation::Rotate { degrees } => {
                *image = rotate(image, *degrees);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use photon_rs::PhotonImage;

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

    fn sample_rect_photon_image() -> PhotonImage {
        PhotonImage::new(
            vec![
                255, 0, 0, 255, //
                0, 255, 0, 255, //
                0, 0, 255, 255, //
                255, 255, 0, 255, //
                255, 0, 255, 255, //
                0, 255, 255, 255, //
                128, 128, 128, 255, //
                64, 64, 64, 255,
            ],
            4,
            2,
        )
    }

    #[test]
    fn parse_operations_supports_multiple_variants() {
        let ops =
            parse_operations("resize:4x4|blur:radius=3|flip:h|flip:v|rotate:deg=45|grayscale")
                .expect("operations should parse");
        assert_eq!(ops.len(), 6);
    }

    #[test]
    fn parse_operations_handles_compact_resize() {
        let ops = parse_operations("resize:12x8").expect("compact resize should parse");
        assert!(matches!(
            ops[0],
            Operation::Resize {
                width: 12,
                height: 8
            }
        ));
    }

    #[test]
    fn parse_operations_rejects_missing_resize_height() {
        let err =
            parse_operations("resize:width=12").expect_err("resize without height must error");
        assert!(
            err.to_string().contains("height"),
            "error message should mention missing height"
        );
    }

    #[test]
    fn parse_operations_rejects_unknown_operation() {
        let err = parse_operations("unknown:foo=bar").expect_err("unknown op should fail");
        assert!(err.to_string().contains("unsupported operation"));
    }

    #[test]
    fn parse_operations_parses_blur_variants() {
        let direct = parse_operations("blur:4").expect("direct blur radius should parse");
        let op = direct.into_iter().next().unwrap();
        assert!(matches!(op, Operation::Blur { radius } if radius == 4));

        let keyed = parse_operations("blur:sigma=2").expect("sigma alias should parse");
        let op = keyed.into_iter().next().unwrap();
        assert!(matches!(op, Operation::Blur { radius } if radius == 2));
    }

    #[test]
    fn parse_ratio_accepts_width() {
        let ops = parse_operations("ratio:width=10").expect("ratio width should parse");
        assert!(matches!(
            ops[0],
            Operation::ResizeKeepAspectWidth { width } if width == 10
        ));
    }

    #[test]
    fn parse_ratio_accepts_height() {
        let ops = parse_operations("ratio:h=12").expect("ratio height should parse");
        assert!(matches!(
            ops[0],
            Operation::ResizeKeepAspectHeight { height } if height == 12
        ));
    }

    #[test]
    fn parse_ratio_rejects_both_dimensions() {
        let err = parse_operations("ratio:width=10,height=20")
            .expect_err("ratio with both width and height should fail");
        assert!(
            err.to_string()
                .contains("ratio expects exactly one dimension")
        );
    }

    #[test]
    fn apply_operations_executes_all_paths() {
        let mut image = sample_photon_image();
        let operations = vec![
            Operation::Resize {
                width: 4,
                height: 4,
            },
            Operation::Blur { radius: 3 },
            Operation::FlipHorizontal,
            Operation::FlipVertical,
            Operation::Rotate { degrees: 45.0 },
            Operation::Grayscale,
        ];

        apply_operations(&mut image, &operations).expect("operations should succeed");
        assert!(image.get_width() > 0);
        assert!(image.get_height() > 0);
        assert!(!image.get_raw_pixels().is_empty());
    }

    #[test]
    fn apply_ratio_preserves_aspect_using_width() {
        let mut image = sample_rect_photon_image();
        let operations = vec![Operation::ResizeKeepAspectWidth { width: 8 }];

        apply_operations(&mut image, &operations).expect("ratio resize should succeed");
        assert_eq!(image.get_width(), 8);
        assert_eq!(image.get_height(), 4);
    }

    #[test]
    fn apply_ratio_preserves_aspect_using_height() {
        let mut image = sample_rect_photon_image();
        let operations = vec![Operation::ResizeKeepAspectHeight { height: 6 }];

        apply_operations(&mut image, &operations).expect("ratio resize should succeed");
        assert_eq!(image.get_height(), 6);
        assert_eq!(image.get_width(), 12);
    }
}
