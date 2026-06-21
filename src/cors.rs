//! CORS response layer for the public endpoint.
//!
//! Browsers fetch `@font-face` fonts (and other CORS-gated subresources) in CORS
//! mode even for a plain `GET`, and discard the response unless it carries an
//! `Access-Control-Allow-Origin` header matching the page's origin. The S3 layer
//! never emits one, so this thin wrapper stamps it — and answers `OPTIONS`
//! preflights — whenever the request's `Origin` is permitted by the admin-configured
//! allow-list ([`SettingsStore::cors_allow_origin`](crate::settings::SettingsStore::cors_allow_origin)).
//!
//! It is installed only on the public read endpoint; the authenticated API and the
//! admin panel are left untouched.

use std::future::Future;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::header::{
    HeaderMap, HeaderValue, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ORIGIN, VARY,
};
use hyper::service::Service;
use hyper::{Method, Request, Response, StatusCode};
use s3s::{Body, HttpError, HttpResponse};

use crate::settings::SharedSettings;

/// Wraps an S3-serving service and applies CORS headers from the configured
/// allowed-origins list. The wrapped service must produce an [`HttpResponse`]
/// (which both `s3s::S3Service` and this wrapper do).
#[derive(Clone)]
pub struct CorsService<S> {
    inner: S,
    settings: SharedSettings,
}

impl<S> CorsService<S> {
    #[must_use]
    pub fn new(inner: S, settings: SharedSettings) -> Self {
        Self { inner, settings }
    }
}

impl<S> Service<Request<Incoming>> for CorsService<S>
where
    S: Service<Request<Incoming>, Response = HttpResponse, Error = HttpError> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = HttpResponse;
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + 'static>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        // Resolve the allow-origin decision from the request's `Origin` up front.
        // `None` here means either no `Origin` (a non-CORS request — e.g. the AWS CLI
        // or curl) or an origin not on the allow-list; in both cases we add no CORS
        // headers and behave exactly as the unwrapped service.
        let allow_origin = req
            .headers()
            .get(ORIGIN)
            .and_then(|v| v.to_str().ok())
            .and_then(|origin| self.settings.cors_allow_origin(origin));

        // Preflight: answer `OPTIONS` directly. The public S3 backend only permits
        // GET/HEAD and would reject it, so it must be handled here.
        if req.method() == Method::OPTIONS {
            let requested_headers = req.headers().get(ACCESS_CONTROL_REQUEST_HEADERS).cloned();
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            let headers = resp.headers_mut();
            apply_allow_origin(headers, allow_origin.as_deref());
            if allow_origin.is_some() {
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
            apply_allow_origin(resp.headers_mut(), allow_origin.as_deref());
            Ok(resp)
        })
    }
}

/// Stamp `Access-Control-Allow-Origin` (plus `Vary: Origin` when the value is
/// origin-specific, so caches don't serve one origin's header to another). A `None`
/// decision leaves the headers untouched.
fn apply_allow_origin(headers: &mut HeaderMap, allow_origin: Option<&str>) {
    let Some(value) = allow_origin else { return };
    let Ok(header) = HeaderValue::from_str(value) else { return };
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, header);
    if value != "*" {
        headers.append(VARY, HeaderValue::from_static("Origin"));
    }
}
