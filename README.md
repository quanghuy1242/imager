# Imager

`imager` is an Axum-based HTTP microservice for on-demand image transformation.  
Give it a public image URL and a pipeline of operations; it downloads the pixels, applies the requested mutations with Photon-rs, and streams the result back with cache-friendly headers that work well behind CDNs.

---

## At a Glance

| Feature | Details |
|---------|---------|
| **Language / Runtime** | Rust (Tokio async runtime) |
| **Core Dependencies** | `axum`, `reqwest`, `photon-rs`, `image`, `tokio` |
| **Default Port** | `0.0.0.0:3000` |
| **Payload Limits** | 5 MiB download cap, 4096×4096 dimension guardrail |
| **Output Formats** | `png`, `jpeg`, `webp` |
| **Cache Headers** | `Cache-Control: public, max-age=300`, strong `ETag` |

---

## Getting Started

### Prerequisites

- Rust toolchain 1.70+ (tested with stable)
- Cargo

### Build & Run

```bash
git clone <repo-url>
cd imager
cargo run
```

The service listens on `0.0.0.0:3000`. Override this by wrapping `web::run_server` or running behind a reverse proxy (nginx, fly.io, etc.).

### Tests & Tooling

```bash
cargo fmt     # Format code
cargo test    # Run unit + integration tests
cargo clippy  # Optional linting
```

All operations, parsers, and HTTP integration paths have dedicated tests under `src/`.

---

## Endpoint Summary

`GET /process`

| Query Param | Required | Description |
|-------------|----------|-------------|
| `url`       | Yes      | Absolute URL to the upstream image. Must be reachable by the service. |
| `ops`       | No       | Pipe separated list of operations. See below for supported syntax. |
| `format`    | No       | Output format: `png` (default), `jpeg`, or `webp`. |
| `quality`   | No       | Integer 1–100. Applies to lossy encoders (`jpeg`, `webp`). `100` keeps max fidelity. |

Example:

```bash
curl "http://localhost:3000/process?url=https%3A%2F%2Fexample.com%2Fphoto.jpg&ops=ratio:width=800|blur:radius=2|grayscale&format=webp&quality=80" \
  --output processed.webp
```

---

## Supported Operations

Operations run in the order supplied. Combine multiple instructions with `|`.

| Operation | Syntax | Notes |
|-----------|--------|-------|
| Resize (fixed) | `resize:800x600` or `resize:width=800,height=600` | Both dimensions required. 1 ≤ width,height ≤ 4096. |
| Resize (keep ratio) | `ratio:width=800` or `ratio:height=600` or shorthand `ratio:w=800` | Computes the missing dimension to preserve aspect ratio. The optional `keep=true` flag is assumed. |
| Blur | `blur:4`, `blur:radius=4`, `blur:sigma=2.5` | Radius must be > 0. Value is rounded to an integer for Photon-rs. |
| Flip | `flip:h` / `flip:horizontal`, `flip:v` / `flip:vertical` | Mirrors the image along the selected axis. |
| Rotate | `rotate:90`, `rotate:deg=45` | Accepts any floating-point degree value. |
| Grayscale | `grayscale` | Converts to monochrome. |

Unknown or malformed operations result in `400 Bad Request` with a descriptive JSON error payload.

---

## Error Handling & Limits

| Scenario | HTTP Status | Description |
|----------|-------------|-------------|
| Remote body > 5 MiB | `413 Payload Too Large` | Enforced before buffering to protect memory. |
| Source / output dimensions exceed 4096 | `413 Payload Too Large` | Applies before and after transformations. |
| Unsupported format | `400 Bad Request` | When encoder cannot satisfy `format`. |
| Invalid ops / params | `400 Bad Request` | Parsing failures, out-of-range quality, etc. |
| Download errors | `502 Bad Gateway` | Bubbles up upstream HTTP failures / timeouts. |
| Internal issues | `500 Internal Server Error` | Reserved for unexpected conditions (spawn failures, encoder panic). |

Responses include a JSON body: `{"error": "<message>"}`.

---

## Project Layout

```
src/
├── main.rs        # Thin binary entrypoint; delegates to web::run_server()
├── constants.rs   # Size limits, cache durations, and other tunables
├── encoding.rs    # Output format encoding (PNG/JPEG/WebP) + tests
├── error.rs       # Shared AppError type implementing IntoResponse
├── operations.rs  # Parser + executor for resize/ratio/blur/... operations
├── processing.rs  # Blocking pipeline (decode -> apply -> encode)
└── web.rs         # Axum router, handler, request validation, integration tests
```

This modular layout makes it easy to extend one layer without disturbing others—for example, adding a new operation involves touching only `operations.rs` and optionally `processing.rs`.

---

## Extending the Service

1. **Add Operation Variant**  
   - Extend `Operation` in `src/operations.rs`.  
   - Update `parse_operation` (and helpers) to handle the new syntax.  
   - Implement the behavior inside `apply_operations`.
2. **Guardrails & Limits**  
   - Reference `src/constants.rs` for shared bounds.  
   - Use `AppError::bad_request` / `payload_too_large` for user-facing validation messages.
3. **Tests**  
   - Unit tests in the same module (`#[cfg(test)]`).  
   - Add an integration test in `src/web.rs` if the change affects the HTTP response.

---

## Deployment Notes

- **Concurrency:** Heavy image work is offloaded to `tokio::task::spawn_blocking` to keep async executors responsive.
- **Caching:** Responses include strong ETags; combine with `Cache-Control` for CDN re-use. Clients can revalidate with `If-None-Match`.
- **Timeouts:** The shared `reqwest::Client` has a 10s request timeout and 5s connect timeout—tune in one place (`src/web.rs`).
- **Observability:** Add tracing/middleware in `web::router()` if you need structured logs or metrics.
- **Security:** Only fetch from trusted upstreams. In production, put the service behind network rules or validate hostnames to avoid SSRF.
