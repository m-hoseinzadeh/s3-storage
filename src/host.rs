//! Host-header resolution for both default (path-style) and custom-domain access.
//!
//! Resolution order for an incoming `Host`:
//!   1. Exact match against a configured custom domain -> that bucket (virtual-host).
//!   2. `<bucket>.<base-domain>` against a configured base domain -> that bucket.
//!   3. Anything else (the base domain itself, `localhost`, an IP, an unknown host)
//!      -> path-style, i.e. the bucket is taken from the first path segment.
//!
//! Path-style is the default so the out-of-the-box URL (`http://host/bucket/key`)
//! always works without any DNS configuration.

use std::collections::HashMap;

use s3s::S3Result;
use s3s::host::{S3Host, VirtualHost};

#[derive(Debug)]
pub struct CustomHost {
    base_domains: Vec<String>,
    domain_map: HashMap<String, String>,
}

impl CustomHost {
    #[must_use]
    pub fn new(base_domains: Vec<String>, domain_map: HashMap<String, String>) -> Self {
        Self {
            base_domains: base_domains.into_iter().map(|d| d.to_ascii_lowercase()).collect(),
            domain_map,
        }
    }
}

impl S3Host for CustomHost {
    fn parse_host_header<'a>(&'a self, host: &'a str) -> S3Result<VirtualHost<'a>> {
        let host_only = host.split(':').next().unwrap_or(host).to_ascii_lowercase();

        // 1. Explicit custom-domain mapping.
        if let Some(bucket) = self.domain_map.get(&host_only) {
            return Ok(VirtualHost::new(host).with_bucket(bucket.clone()));
        }

        // 2. `<bucket>.<base>` virtual-hosted style.
        for base in &self.base_domains {
            if host_only == *base {
                return Ok(VirtualHost::new(host));
            }
            if let Some(prefix) = host_only.strip_suffix(base.as_str()).and_then(|h| h.strip_suffix('.'))
                && !prefix.is_empty()
            {
                return Ok(VirtualHost::new(host).with_bucket(prefix.to_owned()));
            }
        }

        // 3. Default: path-style addressing.
        Ok(VirtualHost::new(host))
    }
}
