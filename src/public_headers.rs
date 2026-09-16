//! Response-header policy for the public endpoint.
//!
//! Browsers fetch `@font-face` fonts (and other CORS-gated subresources) in CORS
//! mode even for a plain `GET`, and discard the response unless it carries an
//! `Access-Control-Allow-Origin` header matching the page's origin. The S3 layer
//! never emits one, so this thin wrapper stamps it — and answers `OPTIONS`
//! preflights — whenever the request's `Origin` is permitted by the admin-configured
//! allow-list ([`SettingsStore::cors_decision`](crate::settings::SettingsStore::cors_decision)).
//!
//! It also stamps `X-Content-Type-Options: nosniff`. Public buckets serve
//! caller-supplied bytes under a caller-supplied `Content-Type`, and without it a
//! browser may sniff a response into something more dangerous than what was
//! declared — an upload stored as `text/plain` rendered as HTML, say — turning any
//! writer into a script author on the bucket's origin.
//!
//! It is installed only on the public read endpoint; the authenticated API and the
//! admin panel are left untouched.

use std::future::Future;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::header::{
    HeaderMap, HeaderValue, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ORIGIN, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use hyper::service::Service;
use hyper::{Method, Request, Response, StatusCode};
use s3s::{Body, HttpError, HttpResponse};

use crate::settings::{CorsDecision, SharedSettings};

/// Wraps an S3-serving service and applies the public endpoint's response-header
/// policy: CORS from the configured allowed-origins list, plus `nosniff`. The
/// wrapped service must produce an [`HttpResponse`] (which both `s3s::S3Service`
/// and this wrapper do).
#[derive(Clone)]
pub struct PublicHeaders<S> {
    inner: S,
    settings: SharedSettings,
}

impl<S> PublicHeaders<S> {
    #[must_use]
    pub fn new(inner: S, settings: SharedSettings) -> Self {
        Self { inner, settings }
    }
}

impl<S> Service<Request<Incoming>> for PublicHeaders<S>
where
    S: Service<Request<Incoming>, Response = HttpResponse, Error = HttpError> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = HttpResponse;
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + 'static>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        // Resolve the CORS handling from the request's `Origin` up front. A `None`
        // `allow_origin` means the origin is not on the allow-list (or nothing is
        // allowed at all); we then add no CORS headers and behave exactly as the
        // unwrapped service. Under a `*` allow-list it is always `Some`, including
        // for a request that carries no `Origin` at all -- see `cors_decision`.
        let origin = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok());
        let decision = self.settings.cors_decision(origin);

        // Preflight: answer `OPTIONS` directly. The public S3 backend only permits
        // GET/HEAD and would reject it, so it must be handled here.
        if req.method() == Method::OPTIONS {
            let requested_headers = req.headers().get(ACCESS_CONTROL_REQUEST_HEADERS).cloned();
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            let headers = resp.headers_mut();
            apply_headers(headers, &decision);
            if decision.allow_origin.is_some() {
                headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, HEAD, OPTIONS"));
                headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400"));
                if let Some(req_headers) = requested_headers {
                    headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, req_headers);
                }
            }
            return Box::pin(async move { Ok(resp) });
        }

        let fut = self.inner.call(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            apply_headers(resp.headers_mut(), &decision);
            Ok(resp)
        })
    }
}

/// Stamp the public endpoint's response headers: `nosniff` always,
/// `Access-Control-Allow-Origin` when the origin is allowed, and `Vary: Origin`
/// whenever the answer depends on the request's origin at all.
///
/// The `Vary` must be emitted even on the *deny* path. The public endpoint is meant
/// to sit behind a CDN, and without it a shared cache can store the header-less
/// response produced for a disallowed origin and later hand it to an allowed one
/// (or the reverse), so cross-origin reads fail intermittently for reasons that do
/// not reproduce.
fn apply_headers(headers: &mut HeaderMap, decision: &CorsDecision) {
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if decision.vary {
        headers.append(VARY, HeaderValue::from_static("Origin"));
    }
    let Some(value) = decision.allow_origin.as_deref() else { return };
    let Ok(header) = HeaderValue::from_str(value) else { return };
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, header);
}
