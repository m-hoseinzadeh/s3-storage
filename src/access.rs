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
