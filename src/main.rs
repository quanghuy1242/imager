//! Axum-based HTTP service that performs on-the-fly image processing with cache-friendly responses.

use anyhow::Result;

mod constants;
mod encoding;
mod error;
mod operations;
mod processing;
mod web;

#[tokio::main]
async fn main() -> Result<()> {
    web::run_server().await
}
