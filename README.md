# Imager

`imager` is an HTTP microservice for on-demand image processing. It downloads an image from a user-supplied URL, runs a configurable pipeline of Photon-rs operations, and returns the transformed pixels with CDN-friendly cache headers. The service is designed for real-time scenarios where UIs need to render formatted media quickly and consistently.

## Installation

Prerequisites:
- Rust toolchain 1.70+ (tested with stable)
- Cargo

Clone the repository and build:

```bash
cargo build
```

Run the service (listens on `0.0.0.0:3000` by default):

```bash
cargo run
```

Execute tests:

```bash
cargo test
```

## Code Logic Flow

1. **HTTP entrypoint** (`/process` in `src/main.rs`):
   - Parses query parameters (`url`, `ops`, `format`).
   - Validates the URL and the operations list.
2. **Download**:
   - Reuses a single `reqwest::Client` instance.
   - Streams the remote body with a strict 5 MiB limit; rejects larger responses up front.
3. **Blocking work** (offloaded with `tokio::task::spawn_blocking`):
   - Decodes the image with Photon-rs (rejecting files wider or taller than 4096 px).
   - Applies the parsed operations sequentially.
   - Re-encodes the result (`png`, `jpeg`, or `webp`).
   - Generates an ETag from the encoded bytes.
4. **Response**:
   - Returns the bytes with `Content-Type`, `ETag`, and `Cache-Control: public, max-age=300`.

## Adding a New Image Operation

1. **Extend the `Operation` enum** (`src/main.rs`).
2. **Update the parser**:
   - Modify `parse_operation` to recognize the new operation string and convert it into your enum variant.
   - Validate arguments (e.g., numeric ranges) and return `AppError::bad_request` on invalid input.
3. **Apply the effect**:
   - Update the `apply_operations` match block to call the corresponding Photon-rs (or custom) routine.
4. **Tests**:
   - Add/adjust unit tests in `#[cfg(test)]` to cover parsing, execution, and error paths.

## Usage

### Endpoint

```
GET /process
```

### Query Parameters

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url`     | Yes      | URL of the source image. Must be accessible by the service. |
| `ops`     | No       | Pipe-delimited operations. Example: `resize:800x600|blur:radius=2|grayscale`. |
| `format`  | No       | Output format (`png`, `jpeg`, `webp`). Defaults to `png`. |

### Supported Operations

- `resize:<width>x<height>` or `resize:width=800,height=600` (limits: 1 ≤ dimension ≤ 4096)
- `blur:<radius>` or `blur:radius=4` or `blur:sigma=4` (radius must be > 0)
- `flip:h` / `flip:horizontal`
- `flip:v` / `flip:vertical`
- `rotate:<degrees>` (any angle, e.g., `rotate:90`, `rotate:deg=45`)
- `grayscale`

Multiple operations can be chained with `|` in the order they should be applied.

### Example

```bash
curl "http://localhost:3000/process?url=https%3A%2F%2Fexample.com%2Fphoto.jpg&ops=resize:800x600|grayscale&format=webp" \
  --output photo.webp
```

### Limits & Errors

- Payload size: requests fail with `413 Payload Too Large` if the remote body exceeds 5 MiB or the decoded image exceeds 4096×4096.
- Unsupported formats return `400 Bad Request`.
- Upstream failures surface as `502 Bad Gateway`.

### Development & Testing

- Run locally: `cargo run`
- Unit/integration tests: `cargo test`
- Adjust `CACHE_MAX_AGE_SECONDS` or size limits near the top of `src/main.rs` to match deployment requirements.
