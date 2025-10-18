//! Axum route handlers and HTTP-specific helpers.

use std::result::Result as StdResult;
use std::{net::SocketAddr, sync::LazyLock, time::Duration};

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::Query,
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, signal, task};
use urlencoding::decode;

use crate::{
    constants::{CACHE_MAX_AGE_SECONDS, MAX_IMAGE_BYTES},
    error::AppError,
    operations::parse_operations,
    processing::process_image_bytes,
};

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
pub struct ProcessQuery {
    pub url: String,
    pub ops: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    /// Optional compression quality (1-100); defaults to 100 (best quality).
    pub quality: Option<u8>,
}

pub fn router() -> Router {
    Router::new().route("/process", get(process_image))
}

pub async fn serve(addr: SocketAddr, router: Router) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .context("failed to bind TCP listener")?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

pub async fn process_image(Query(query): Query<ProcessQuery>) -> StdResult<Response, AppError> {
    let url_str = decode(&query.url)
        .map_err(|err| AppError::bad_request(format!("invalid URL encoding: {err}")))?
        .into_owned();

    let url =
        Url::parse(&url_str).map_err(|err| AppError::bad_request(format!("invalid URL: {err}")))?;

    let ops = if let Some(ops_str) = query.ops.as_deref() {
        if ops_str.trim().is_empty() {
            Vec::new()
        } else {
            parse_operations(ops_str).map_err(|err| AppError::bad_request(err.to_string()))?
        }
    } else {
        Vec::new()
    };

    let format = query.format.as_deref().unwrap_or("png").to_string();
    let quality = query.quality.unwrap_or(100);

    if quality == 0 || quality > 100 {
        return Err(AppError::bad_request(
            "quality must be between 1 and 100 (100 retains original quality)",
        ));
    }

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

    let operations = ops.clone();
    let processed =
        task::spawn_blocking(move || process_image_bytes(body, operations, &format, quality))
            .await
            .map_err(|err| AppError::internal(format!("processing task failed: {err}")))??;

    build_response(processed)
}

fn build_response((mime, data): (String, Bytes)) -> StdResult<Response, AppError> {
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

fn compute_etag(data: &[u8]) -> String {
    let hash = Sha256::digest(data);
    format!("\"{:x}\"", hash)
}

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

pub async fn run_server() -> anyhow::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Listening on http://{addr}");

    let app = router();
    serve(addr, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use http_body_util::BodyExt;
    use httpmock::prelude::*;
    use image::{ImageBuffer, ImageOutputFormat, Rgba};
    use std::io::Cursor;

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
            quality: Some(75),
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
            quality: None,
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
            quality: None,
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
            when.method(GET).path("/image");
            then.status(500);
        });

        let query = ProcessQuery {
            url: server.url("/image"),
            ops: None,
            format: None,
            quality: None,
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
            quality: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("unknown op should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("unsupported operation"));
    }

    #[tokio::test]
    async fn process_image_rejects_invalid_quality() {
        let query = ProcessQuery {
            url: "https://example.com/image.png".to_string(),
            ops: None,
            format: Some("jpeg".to_string()),
            quality: Some(0),
        };

        let err = process_image(Query(query))
            .await
            .expect_err("quality below range should fail");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(err.message.contains("quality must be between 1 and 100"));
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
            quality: None,
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
        let oversized = vec![0u8; MAX_IMAGE_BYTES + 1];

        let _mock = server.mock(|when, then| {
            when.method(GET).path("/large");
            then.status(200)
                .header("content-type", "image/png")
                .header("content-length", (MAX_IMAGE_BYTES + 1).to_string())
                .body(oversized.clone());
        });

        let query = ProcessQuery {
            url: server.url("/large"),
            ops: None,
            format: None,
            quality: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("large payload should fail");
        assert_eq!(err.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn process_image_rejects_excessive_dimensions() {
        let wide_image: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(
            crate::constants::MAX_DIMENSION + 1,
            10,
            Rgba([0, 0, 0, 255]),
        );
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
            quality: None,
        };

        let err = process_image(Query(query))
            .await
            .expect_err("dimensions over limit should fail");
        assert_eq!(err.status, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
