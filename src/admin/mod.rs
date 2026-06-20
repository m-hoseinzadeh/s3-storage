//! Embedded web admin panel.
//!
//! Installed as an [`s3s::route::S3Route`] so requests under the configured prefix
//! (default `/admin`) are intercepted *before* path-style bucket resolution. The
//! panel serves a single-page app and a JSON API (`{prefix}/api/*`) that reuses the
//! storage backend's `s3s::S3` implementation directly, so it never duplicates
//! storage logic and stays behaviourally identical to the S3 wire API.
//!
//! Authentication is by the server's own access/secret key (see [`auth`]); a signed
//! session cookie gates every `/api/*` route except login.

mod api;
mod assets;
mod auth;
mod presign;

use std::sync::Arc;

use async_trait::async_trait;
use hyper::http::Extensions;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{HeaderMap, Method, StatusCode, Uri};
use s3s::auth::Credentials;
use s3s::route::S3Route;
use s3s::{Body, S3Request, S3Response, S3Result};

use crate::backend::FileSystem;
use crate::config::Config;

use self::auth::Sessions;

/// Shared, immutable state for the admin panel.
pub struct AdminState {
    pub(crate) fs: Arc<FileSystem>,
    pub(crate) sessions: Sessions,
    pub(crate) access_key: String,
    pub(crate) secret_key: String,
    pub(crate) prefix: String,
    pub(crate) public_buckets: Vec<String>,
    pub(crate) domains: Vec<String>,
    pub(crate) domain_map: Vec<String>,
    pub(crate) version: &'static str,
}

impl AdminState {
    /// Build admin state. Requires credentials to be configured (callers gate on
    /// [`Config::admin_active`]).
    #[must_use]
    pub fn new(fs: Arc<FileSystem>, config: &Config) -> Self {
        let (access_key, secret_key) = config.credentials().expect("admin requires credentials");
        let prefix = config.admin_prefix();
        let sessions = Sessions::new(
            access_key.clone(),
            secret_key.clone(),
            config.admin_session_ttl_secs,
            prefix.clone(),
        );
        Self {
            fs,
            sessions,
            access_key,
            secret_key,
            prefix,
            public_buckets: config.public_buckets.clone(),
            domains: config.domains.clone(),
            domain_map: config.domain_map.clone(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Build an authenticated [`S3Request`] for the backend, carrying the server's
    /// own credentials so write paths and multipart ownership checks succeed.
    pub(crate) fn s3_request<T>(&self, input: T) -> S3Request<T> {
        S3Request {
            input,
            method: Method::GET,
            uri: Uri::default(),
            headers: HeaderMap::new(),
            extensions: Extensions::new(),
            credentials: Some(Credentials {
                access_key: self.access_key.clone(),
                secret_key: self.secret_key.clone().into(),
            }),
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    pub(crate) fn is_public(&self, bucket: &str) -> bool {
        self.public_buckets.iter().any(|b| b == bucket)
    }
}

/// The [`S3Route`] that owns all admin handling.
#[derive(Clone)]
pub struct AdminRoute {
    state: Arc<AdminState>,
    prefix: String,
}

impl AdminRoute {
    #[must_use]
    pub fn new(state: Arc<AdminState>) -> Self {
        let prefix = state.prefix.clone();
        Self { state, prefix }
    }
}

#[async_trait]
impl S3Route for AdminRoute {
    fn is_match(&self, _method: &Method, uri: &Uri, _headers: &HeaderMap, _ext: &mut Extensions) -> bool {
        let path = uri.path();
        path == self.prefix || path.starts_with(&format!("{}/", self.prefix))
    }

    // The panel authenticates via its own session cookie, so bypass the default
    // SigV4-credentials requirement.
    async fn check_access(&self, _req: &mut S3Request<Body>) -> S3Result<()> {
        Ok(())
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let rel = req.uri.path().strip_prefix(&self.prefix).unwrap_or("").to_owned();
        if rel.starts_with("/api/") || rel == "/api" {
            Ok(api::dispatch(&self.state, req, &rel).await)
        } else {
            // Redirect bare `{prefix}` to `{prefix}/` so relative asset URLs resolve.
            if rel.is_empty() {
                let mut resp = S3Response::new(Body::empty());
                resp.status = Some(StatusCode::FOUND);
                resp.headers.insert(
                    hyper::header::LOCATION,
                    HeaderValue::from_str(&format!("{}/", self.prefix))
                        .unwrap_or_else(|_| HeaderValue::from_static("/admin/")),
                );
                return Ok(resp);
            }
            Ok(assets::serve(&rel))
        }
    }
}

// ---- shared response/error helpers (used by the api submodule) ----

/// A JSON error with an HTTP status, rendered as `{ "error": { code, message } }`.
pub(crate) struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.to_owned(), message: message.into() }
    }
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BadRequest", message)
    }
    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized", message)
    }
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NotFound", message)
    }
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal", message)
    }

    pub(crate) fn into_response(self) -> S3Response<Body> {
        json(
            self.status,
            &serde_json::json!({ "error": { "code": self.code, "message": self.message } }),
        )
    }
}

impl From<s3s::S3Error> for ApiError {
    fn from(e: s3s::S3Error) -> Self {
        let status = e.status_code().unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let code = e.code().as_str().to_owned();
        let message = e.message().map(ToOwned::to_owned).unwrap_or_else(|| code.clone());
        Self { status, code, message }
    }
}

/// Build a JSON response with the given status.
pub(crate) fn json(status: StatusCode, value: &serde_json::Value) -> S3Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut resp = S3Response::new(Body::from(body));
    resp.status = Some(status);
    resp.headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

pub(crate) fn json_ok(value: serde_json::Value) -> S3Response<Body> {
    json(StatusCode::OK, &value)
}

/// Collapse a handler result into a response, rendering errors as JSON.
pub(crate) fn finish(result: Result<S3Response<Body>, ApiError>) -> S3Response<Body> {
    match result {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}
