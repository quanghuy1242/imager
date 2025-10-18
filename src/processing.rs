//! CPU-bound image processing pipeline executed on blocking workers.

use bytes::Bytes;
use photon_rs::PhotonImage;
use photon_rs::native::open_image_from_bytes;
use std::result::Result as StdResult;

use crate::{
    constants::MAX_DIMENSION,
    encoding::{EncodeImageError, encode_image},
    error::AppError,
    operations::{Operation, apply_operations},
};

pub fn process_image_bytes(
    bytes: Bytes,
    ops: Vec<Operation>,
    format: &str,
    quality: u8,
) -> StdResult<(String, Bytes), AppError> {
    let mut image = open_image_from_bytes(&bytes)
        .map_err(|err| AppError::bad_request(format!("failed to decode image: {err}")))?;

    enforce_dimension_limits(&image)?;
    apply_operations(&mut image, &ops)
        .map_err(|err| AppError::bad_request(format!("failed to apply operations: {err}")))?;
    enforce_dimension_limits(&image)?;

    match encode_image(&image, format, quality) {
        Ok((mime, data)) => Ok((mime, data)),
        Err(EncodeImageError::UnsupportedFormat(fmt)) => Err(AppError::bad_request(format!(
            "unsupported output format '{fmt}' (supported: png, jpeg, webp)"
        ))),
        Err(EncodeImageError::EncodeFailed(err)) => {
            Err(AppError::internal(format!("failed to encode image: {err}")))
        }
    }
}

fn enforce_dimension_limits(image: &PhotonImage) -> StdResult<(), AppError> {
    if image.get_width() > MAX_DIMENSION || image.get_height() > MAX_DIMENSION {
        return Err(AppError::payload_too_large(format!(
            "image dimensions exceed {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }
    Ok(())
}
