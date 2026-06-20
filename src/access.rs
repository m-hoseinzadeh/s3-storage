//! Per-bucket public/private access control.
//!
//! `s3s` verifies the SigV4 signature before this runs (when auth is configured),
//! so a present `credentials()` means a valid, authenticated request. The policy:
//!
//!   * Authenticated requests are always allowed.
//!   * Anonymous requests are allowed only for read-only operations (`GET`/`HEAD`)
//!     against a bucket listed as public; everything else is denied.

use std::collections::HashSet;

use hyper::Method;
use s3s::S3Result;
use s3s::access::{S3Access, S3AccessContext};
use s3s::s3_error;

use crate::settings::SharedSettings;

/// Access policy for the authenticated S3 API port. The `public_buckets` set is
/// always empty here (installed via `AccessControl::new(HashSet::new())`): the API
/// port authenticates every request and never serves anonymous reads — those are
/// served on the dedicated public port by [`PublicReadAccess`]. It is intentionally
/// *not* settings-backed, so the API port's security posture cannot change at
/// runtime.
#[derive(Debug)]
pub struct AccessControl {
    public_buckets: HashSet<String>,
}

impl AccessControl {
    #[must_use]
    pub fn new(public_buckets: HashSet<String>) -> Self {
        Self { public_buckets }
    }
}

#[async_trait::async_trait]
impl S3Access for AccessControl {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        // A valid signature was already verified by the auth layer.
        if cx.credentials().is_some() {
            return Ok(());
        }

        // Anonymous: only read-only access to public buckets is permitted.
        let method = cx.method();
        let is_read = *method == Method::GET || *method == Method::HEAD;
        if is_read
            && let Some(bucket) = cx.s3_path().get_bucket_name()
            && self.public_buckets.contains(bucket)
        {
            return Ok(());
        }

        Err(s3_error!(AccessDenied, "Anonymous access is not allowed for this request"))
    }
}

/// Access policy for the dedicated public endpoint: regardless of credentials, the
/// only thing permitted is `GET`/`HEAD` of an individual object in a configured
/// public bucket. Bucket-level listing (`ListObjectsV2`) and bucket enumeration
/// (`ListBuckets`) are denied, so the public port cannot be used to discover keys —
/// callers must know the object key. This keeps the port strictly read-only and
/// public-scoped even if a request carries a valid signature.
///
/// `s3s` runs access checks only when authentication is also configured, so the
/// public service still installs an auth provider purely to enable this stage.
#[derive(Debug)]
pub struct PublicReadAccess {
    settings: SharedSettings,
}

impl PublicReadAccess {
    #[must_use]
    pub fn new(settings: SharedSettings) -> Self {
        Self { settings }
    }
}

#[async_trait::async_trait]
impl S3Access for PublicReadAccess {
    async fn check(&self, cx: &mut S3AccessContext<'_>) -> S3Result<()> {
        let method = cx.method();
        let is_read = *method == Method::GET || *method == Method::HEAD;
        // `get_object_key()` is `Some` only for an object path (bucket + key); it is
        // `None` for bucket-level requests, which excludes listing/enumeration.
        if is_read
            && cx.s3_path().get_object_key().is_some()
            && let Some(bucket) = cx.s3_path().get_bucket_name()
            && self.settings.is_public(bucket)
        {
            return Ok(());
        }

        Err(s3_error!(AccessDenied, "Only public-bucket object reads are served on this endpoint"))
    }
}
