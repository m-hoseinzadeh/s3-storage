//! Embedded web admin panel.
//!
//! Served on its own dedicated port (see [`crate::Config::admin_port`]) as an
//! [`s3s::route::S3Route`] that matches every request, so the panel owns the whole
//! port: it serves a single-page app at `/` and a JSON API under `/api/*` that
//! reuses the storage backend's `s3s::S3` implementation directly, so it never
//! duplicates storage logic and stays behaviourally identical to the S3 wire API.
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
    pub(crate) public_buckets: Vec<String>,
    pub(crate) domains: Vec<String>,
    pub(crate) domain_map: Vec<String>,
    /// Public base URL of the S3 API used for presigned links; see
    /// [`crate::Config::api_public_url`]. `None` falls back to the request `Host`.
    pub(crate) api_public_url: Option<String>,
    pub(crate) version: &'static str,
}

impl AdminState {
    /// Build admin state. Requires credentials to be configured (callers gate on
    /// [`Config::admin_active`]).
    #[must_use]
    pub fn new(fs: Arc<FileSystem>, config: &Config) -> Self {
        let (access_key, secret_key) = config.credentials().expect("admin requires credentials");
        // The panel owns the whole port, so the session cookie is scoped to root.
        let sessions = Sessions::new(
            access_key.clone(),
            secret_key.clone(),
            config.admin_session_ttl_secs,
            "/".to_owned(),
        );
        Self {
            fs,
            sessions,
            access_key,
            secret_key,
            public_buckets: config.public_buckets.clone(),
            domains: config.domains.clone(),
            domain_map: config.domain_map.clone(),
            api_public_url: config.api_public_url.clone(),
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

/// The [`S3Route`] that owns all admin handling. Installed on the dedicated admin
/// port, where it matches every request so nothing falls through to S3.
#[derive(Clone)]
pub struct AdminRoute {
    state: Arc<AdminState>,
}

impl AdminRoute {
    #[must_use]
    pub fn new(state: Arc<AdminState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl S3Route for AdminRoute {
    // The admin port serves only the panel; claim every request.
    fn is_match(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap, _ext: &mut Extensions) -> bool {
        true
    }

    // The panel authenticates via its own session cookie, so bypass the default
    // SigV4-credentials requirement.
    async fn check_access(&self, _req: &mut S3Request<Body>) -> S3Result<()> {
        Ok(())
    }

    async fn call(&self, req: S3Request<Body>) -> S3Result<S3Response<Body>> {
        let rel = req.uri.path().to_owned();
        if rel.starts_with("/api/") || rel == "/api" {
            Ok(api::dispatch(&self.state, req, &rel).await)
        } else {
            // `rel` is "/" or a client-side route; assets::serve maps "/" to
            // index.html and falls back to the SPA shell for unknown routes.
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
