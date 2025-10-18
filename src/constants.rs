//! Shared configuration constants for image processing limits and cache behavior.

/// Upper bound on bytes we are willing to download from an upstream source.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024; // 5 MiB

/// Guardrail for both input and output width/height (prevents oversized transforms).
pub const MAX_DIMENSION: u32 = 4096;

/// How long downstream caches (browser/CDN) may reuse a processed image.
pub const CACHE_MAX_AGE_SECONDS: u32 = 300;
