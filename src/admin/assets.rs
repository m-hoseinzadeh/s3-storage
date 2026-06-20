//! Embedded admin SPA assets with single-page-app fallback.
//!
//! The built frontend in `admin-ui/dist` is compiled into the binary by
//! `rust-embed`. Unknown sub-paths fall back to `index.html` so client-side
//! routes deep-link correctly. Hashed asset files are cached aggressively;
//! `index.html` is never cached.

use hyper::StatusCode;
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use rust_embed::RustEmbed;
use s3s::{Body, S3Response};

#[derive(RustEmbed)]
#[folder = "admin-ui/dist"]
struct Assets;

/// Serve a static asset for `rel_path` (already stripped of the admin prefix),
/// falling back to the SPA shell for unknown routes.
#[must_use]
pub fn serve(rel_path: &str) -> S3Response<Body> {
    let path = rel_path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Assets::get(path) {
        let mime = file.metadata.mimetype().to_owned();
        return asset_response(path, &mime, file.data.into_owned());
    }
    if let Some(file) = Assets::get("index.html") {
        return asset_response("index.html", "text/html", file.data.into_owned());
    }

    let mut resp = S3Response::new(Body::from(
        "Admin UI is not built. Run `npm run build` in admin-ui/ and rebuild.".to_owned(),
    ));
    resp.status = Some(StatusCode::NOT_FOUND);
    resp
}

fn asset_response(path: &str, mime: &str, data: Vec<u8>) -> S3Response<Body> {
    let mut resp = S3Response::new(Body::from(data));
    resp.headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    resp.headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}
