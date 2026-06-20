//! Host-header resolution for both default (path-style) and custom-domain access.
//!
//! Resolution order for an incoming `Host`:
//!   1. Exact match against a configured custom domain -> that bucket (virtual-host).
//!   2. `<bucket>.<base-domain>` against a configured base domain -> that bucket.
//!   3. Anything else (the base domain itself, `localhost`, an IP, an unknown host)
//!      -> path-style, i.e. the bucket is taken from the first path segment.
//!
//! Path-style is the default so the out-of-the-box URL (`http://host/bucket/key`)
//! always works without any DNS configuration. The domain configuration is read
//! live from the [settings store](crate::settings) on every request.

use s3s::S3Result;
use s3s::host::{S3Host, VirtualHost};

use crate::settings::SharedSettings;

#[derive(Debug)]
pub struct CustomHost {
    settings: SharedSettings,
}

impl CustomHost {
    #[must_use]
    pub fn new(settings: SharedSettings) -> Self {
        Self { settings }
    }
}

impl S3Host for CustomHost {
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>> {
        let host_only = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        match self.settings.resolve_host(&host_only) {
            Some(bucket) => Ok(VirtualHost::new(host).with_bucket(bucket)),
            None => Ok(VirtualHost::new(host)),
        }
    }
}
