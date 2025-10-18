//! Axum-based HTTP service that performs on-the-fly image processing with cache-friendly responses.

use std::{collections::HashMap, net::SocketAddr, sync::LazyLock, time::Duration};

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::Query,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use photon_rs::{
    PhotonImage,
    conv::gaussian_blur,
    helpers::dyn_image_from_raw,
    monochrome::grayscale,
    native::open_image_from_bytes,
    transform::{SamplingFilter, fliph, flipv, resize, rotate},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, signal, task};
use urlencoding::decode;

/// Upper bound on bytes we are willing to download from an upstream source.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024; // 5 MiB
/// Guardrail for both input and output width/height (prevents oversized transforms).
const MAX_DIMENSION: u32 = 4096;
/// How long downstream caches (browser/CDN) may reuse a processed image.
const CACHE_MAX_AGE_SECONDS: u32 = 300;

/// Single reqwest client reused across requests so connections are pooled.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client")
});

/// Deserialized query string payload (e.g. `?url=...&ops=...`).
#[derive(Debug, Deserialize, Clone)]
struct ProcessQuery {
    url: String,
    ops: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

/// Rich AST describing the image operations requested by the client.
#[derive(Debug, Clone)]
enum Operation {
    Resize { width: u32, height: u32 },
    Blur { radius: i32 },
    Grayscale,
    FlipHorizontal,
    FlipVertical,
    Rotate { degrees: f32 },
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, message)
    }

    fn upstream_error(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.message,
        }));
        (self.status, body).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Router is intentionally minimal; additional routes can mount more services here.
    let app = Router::new().route("/process", get(process_image));

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Listening on http://{addr}");

    let listener = TcpListener::bind(addr)
        .await
        .context("failed to bind TCP listener")?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

/// Wait for either Ctrl+C or (on Unix) SIGTERM before shutting the server down.
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).expect("failed to install signal");

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Handle `/process` requests end-to-end, wiring together download, transformation, and response assembly.
async fn process_image(Query(query): Query<ProcessQuery>) -> Result<Response, AppError> {
    let url_str = decode(&query.url)
        .map_err(|err| AppError::bad_request(format!("invalid URL encoding: {err}")))?
        .into_owned();

    let url = reqwest::Url::parse(&url_str)
        .map_err(|err| AppError::bad_request(format!("invalid URL: {err}")))?;

    let ops = if let Some(ops_str) = query.ops.as_deref() {
        if ops_str.trim().is_empty() {
            Vec::new()
        } else {
            parse_operations(ops_str).map_err(|err| AppError::bad_request(err.to_string()))?
        }
    } else {
        Vec::new()
    };

    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|err| AppError::upstream_error(format!("failed to download image: {err}")))?
        .error_for_status()
        .map_err(|err| AppError::upstream_error(format!("upstream error: {err}")))?;

    if let Some(length) = response.content_length()
        && length as usize > MAX_IMAGE_BYTES
    {
        return Err(AppError::payload_too_large(format!(
            "image exceeds {MAX_IMAGE_BYTES} bytes limit"
        )));
    }

    let body = read_with_limit(response).await.map_err(|err| match err {
        ReadError::TooLarge => {
            AppError::payload_too_large(format!("image exceeds {MAX_IMAGE_BYTES} bytes limit"))
        }
        ReadError::Upstream(message) => AppError::upstream_error(message),
    })?;

    let format = query.format.as_deref().unwrap_or("png").to_string();
    let operations = ops.clone();
    let processed = task::spawn_blocking(move || process_image_bytes(body, operations, &format))
        .await
        .map_err(|err| AppError::internal(format!("processing task failed: {err}")))??;

    build_response(processed)
}

fn parse_operations(raw: &str) -> Result<Vec<Operation>> {
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

/// Execute the parsed operation sequence on the given image buffer.
fn apply_operations(image: &mut PhotonImage, operations: &[Operation]) -> Result<()> {
    for operation in operations {
        match operation {
            Operation::Resize { width, height } => {
                *image = resize(image, *width, *height, SamplingFilter::Lanczos3);
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

#[derive(Debug, Clone)]
enum EncodeImageError {
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
fn encode_image(image: &PhotonImage, format: &str) -> Result<(String, Bytes), EncodeImageError> {
    use image::ImageOutputFormat;
    use std::io::Cursor;

    let dyn_img = dyn_image_from_raw(image);
    let format_lc = format.to_lowercase();
    let (mime, image_format) = match format_lc.as_str() {
        "png" => ("image/png", ImageOutputFormat::Png),
        "jpeg" | "jpg" => ("image/jpeg", ImageOutputFormat::Jpeg(85)),
        "webp" => ("image/webp", ImageOutputFormat::WebP),
        other => return Err(EncodeImageError::UnsupportedFormat(other.to_string())),
    };

    let mut cursor = Cursor::new(Vec::new());
    dyn_img
        .write_to(&mut cursor, image_format)
        .map_err(|err| EncodeImageError::EncodeFailed(err.to_string()))?;

    Ok((mime.to_string(), Bytes::from(cursor.into_inner())))
}

fn build_response((mime, data): (String, Bytes)) -> Result<Response, AppError> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(data.clone()))
        .map_err(|err| AppError::internal(format!("failed to build response: {err}")))?;

    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .map_err(|_| AppError::internal("failed to build content type header"))?,
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&format!("public, max-age={CACHE_MAX_AGE_SECONDS}"))
            .map_err(|_| AppError::internal("failed to build cache header"))?,
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&compute_etag(&data))
            .map_err(|_| AppError::internal("failed to build etag header"))?,
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );

    Ok(response)
}

enum ReadError {
    TooLarge,
    Upstream(String),
}

async fn read_with_limit(response: reqwest::Response) -> Result<Bytes, ReadError> {
    let content_length = response.content_length();
    let mut stream = response.bytes_stream();
    let mut buffer = BytesMut::with_capacity(content_length.unwrap_or(0) as usize);
    let mut total = 0usize;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| ReadError::Upstream(err.to_string()))?;
        total += chunk.len();
        if total > MAX_IMAGE_BYTES {
            return Err(ReadError::TooLarge);
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok(buffer.freeze())
}

fn process_image_bytes(
    bytes: Bytes,
    ops: Vec<Operation>,
    format: &str,
) -> Result<(String, Bytes), AppError> {
    let mut image = open_image_from_bytes(&bytes)
        .map_err(|err| AppError::bad_request(format!("failed to decode image: {err}")))?;

    if image.get_width() > MAX_DIMENSION || image.get_height() > MAX_DIMENSION {
        return Err(AppError::payload_too_large(format!(
            "image dimensions exceed {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }

    apply_operations(&mut image, &ops)
        .map_err(|err| AppError::bad_request(format!("failed to apply operations: {err}")))?;

    if image.get_width() > MAX_DIMENSION || image.get_height() > MAX_DIMENSION {
        return Err(AppError::payload_too_large(format!(
            "resulting image dimensions exceed {MAX_DIMENSION}x{MAX_DIMENSION}"
        )));
    }

    match encode_image(&image, format) {
        // Map encoder outcomes into HTTP-friendly errors.
        Ok((mime, data)) => Ok((mime, data)),
        Err(EncodeImageError::UnsupportedFormat(fmt)) => Err(AppError::bad_request(format!(
            "unsupported output format '{fmt}' (supported: png, jpeg, webp)"
        ))),
        Err(EncodeImageError::EncodeFailed(err)) => {
            Err(AppError::internal(format!("failed to encode image: {err}")))
        }
    }
}

fn compute_etag(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("\"{:x}\"", hash)
}

#[cfg(test)]
/// Regression tests covering parser edge cases, size limits, and happy paths.
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use httpmock::prelude::*;
    use image::{ImageBuffer, ImageOutputFormat, Rgba};
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

    fn sample_png_bytes() -> Vec<u8> {
        let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(2, 2, |x, y| match (x, y) {
                (0, 0) => Rgba([255, 0, 0, 255]),
                (1, 0) => Rgba([0, 255, 0, 255]),
                (0, 1) => Rgba([0, 0, 255, 255]),
                _ => Rgba([255, 255, 0, 255]),
            });

        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut cursor, ImageOutputFormat::Png)
            .unwrap();
        cursor.into_inner()
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
    fn encode_image_supports_formats() {
        let image = sample_photon_image();
        let (mime_png, data_png) = encode_image(&image, "png").expect("png encoding should work");
        assert_eq!(mime_png, "image/png");
        assert!(!data_png.is_empty());

        let (mime_jpeg, data_jpeg) =
            encode_image(&image, "jpeg").expect("jpeg encoding should work");
        assert_eq!(mime_jpeg, "image/jpeg");
        assert!(!data_jpeg.is_empty());

        let (mime_webp, data_webp) =
            encode_image(&image, "webp").expect("webp encoding should work");
        assert_eq!(mime_webp, "image/webp");
        assert!(!data_webp.is_empty());
    }

    #[test]
    fn encode_image_rejects_unknown_format() {
        let image = sample_photon_image();
        let err = encode_image(&image, "gif").expect_err("gif should not be supported");
        match err {
            EncodeImageError::UnsupportedFormat(fmt) => assert_eq!(fmt, "gif"),
            _ => panic!("expected unsupported format error"),
        }
    }

    #[tokio::test]
    async fn process_image_success_pipeline() {
        let server = MockServer::start();
        let image_bytes = sample_png_bytes();

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/image.png");
            then.status(200)
                .header("content-type", "image/png")
                .body(image_bytes.clone());
        });

        let query = ProcessQuery {
            url: server.url("/image.png"),
            ops: Some("resize:4x4|blur:radius=2|flip:h|rotate:90".to_string()),
            format: Some("jpeg".to_string()),
        };

        let response = process_image(Query(query))
            .await
            .expect("processing should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).expect("content type set"),
            "image/jpeg"
        );
        let cache_control = headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .expect("cache control set");
        assert_eq!(
            cache_control,
            format!("public, max-age={CACHE_MAX_AGE_SECONDS}")
        );
        assert!(headers.get(header::ETAG).is_some(), "etag should be set");

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body to collect")
            .to_bytes();
        assert!(!bytes.is_empty());
    }

    #[tokio::test]
    async fn process_image_handles_invalid_encoding() {
        let query = ProcessQuery {
            url: "%ZZ".to_string(),
            ops: None,
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("invalid encoding should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn process_image_handles_invalid_url() {
        let query = ProcessQuery {
            url: "%25".to_string(),
            ops: None,
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("bad url should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid URL"));
    }

    #[tokio::test]
    async fn process_image_propagates_upstream_error() {
        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/oops");
            then.status(500);
        });

        let query = ProcessQuery {
            url: server.url("/oops"),
            ops: None,
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("upstream failure should be reported");
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
        assert!(err.message.contains("upstream error"));
    }

    #[tokio::test]
    async fn process_image_rejects_unknown_operation() {
        let server = MockServer::start();
        let image_bytes = sample_png_bytes();

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/image");
            then.status(200).body(image_bytes.clone());
        });

        let query = ProcessQuery {
            url: server.url("/image"),
            ops: Some("unknown:foo=bar".to_string()),
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("unknown op should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("unsupported operation"));
    }

    #[tokio::test]
    async fn process_image_rejects_unsupported_format() {
        let server = MockServer::start();
        let image_bytes = sample_png_bytes();

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/image");
            then.status(200).body(image_bytes.clone());
        });

        let query = ProcessQuery {
            url: server.url("/image"),
            ops: Some("grayscale".to_string()),
            format: Some("gif".to_string()),
        };

        let err = process_image(Query(query))
            .await
            .expect_err("unsupported format should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("unsupported output format 'gif'"));
    }

    #[tokio::test]
    async fn process_image_rejects_large_payload() {
        let server = MockServer::start();
        let oversized = vec![0u8; super::MAX_IMAGE_BYTES + 1];

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/large");
            then.status(200)
                .header("content-type", "image/png")
                .header("content-length", (super::MAX_IMAGE_BYTES + 1).to_string())
                .body(oversized.clone());
        });

        let query = ProcessQuery {
            url: server.url("/large"),
            ops: None,
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("large payload should fail");
        assert_eq!(err.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn process_image_rejects_excessive_dimensions() {
        let wide_image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(super::MAX_DIMENSION + 1, 10, Rgba([0, 0, 0, 255]));
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(wide_image)
            .write_to(&mut cursor, ImageOutputFormat::Png)
            .unwrap();
        let encoded = cursor.into_inner();

        let server = MockServer::start();
        let _mock = server.mock(|when, then| {
            when.method(GET).path("/wide");
            then.status(200)
                .header("content-type", "image/png")
                .body(encoded.clone());
        });

        let query = ProcessQuery {
            url: server.url("/wide"),
            ops: None,
            format: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("dimensions over limit should fail");
        assert_eq!(err.status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
