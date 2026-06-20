//! Runtime configuration, sourced from CLI flags overlaid on environment variables.
//!
//! Per-bucket deployment settings (public/private access mode and custom-domain
//! mapping) are configuration-driven rather than persisted per bucket: this keeps
//! the data root pure object storage and mirrors how the service is deployed in
//! practice (behind DNS / a reverse proxy).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "s3-storage", version, about = "Minimal S3-compatible file server")]
pub struct Config {
    /// Root directory where buckets and objects are stored.
    #[arg(long, env = "S3_ROOT", default_value = "/data")]
    pub root: PathBuf,

    /// Address to bind the HTTP listeners to (shared by all three ports).
    #[arg(long, env = "S3_HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// Port for the authenticated S3 API (SDK clients). Anonymous access is
    /// rejected here; anonymous public-bucket reads are served on `--public-port`.
    #[arg(long, env = "S3_PORT", default_value_t = 8080)]
    pub port: u16,

    /// Port for the public read-only endpoint: anonymous `GET`/`HEAD` of buckets
    /// listed in `--public-bucket`. Intended to sit behind a CDN/asset domain.
    #[arg(long, env = "S3_PUBLIC_PORT", default_value_t = 8082)]
    pub public_port: u16,

    /// Access key for SigV4 authentication. Must be set together with `--secret-key`.
    #[arg(long, env = "S3_ACCESS_KEY")]
    pub access_key: Option<String>,

    /// Secret key for SigV4 authentication. Must be set together with `--access-key`.
    #[arg(long, env = "S3_SECRET_KEY")]
    pub secret_key: Option<String>,

    /// Base domain(s) enabling virtual-hosted-style access (`<bucket>.<domain>`).
    /// Repeat the flag or use a comma-separated `S3_DOMAINS`.
    #[arg(long = "domain", env = "S3_DOMAINS", value_delimiter = ',')]
    pub domains: Vec<String>,

    /// Buckets that allow anonymous read access. Repeat the flag or use a
    /// comma-separated `S3_PUBLIC_BUCKETS`.
    #[arg(long = "public-bucket", env = "S3_PUBLIC_BUCKETS", value_delimiter = ',')]
    pub public_buckets: Vec<String>,

    /// Custom-domain to bucket mappings as `host=bucket`. Repeat the flag or use a
    /// comma-separated `S3_DOMAIN_MAP` (e.g. `files.example.com=assets,cdn.foo=img`).
    #[arg(long = "domain-map", env = "S3_DOMAIN_MAP", value_delimiter = ',')]
    pub domain_map: Vec<String>,

    /// Enable the embedded web admin panel. Requires credentials
    /// (`--access-key`/`--secret-key`) to be configured; otherwise it stays disabled.
    #[arg(long, env = "S3_ADMIN_ENABLED", default_value_t = false)]
    pub admin_enabled: bool,

    /// Port for the admin panel. The panel (SPA + its JSON API) is served at the
    /// root of this dedicated port, so it can sit behind its own admin domain.
    #[arg(long, env = "S3_ADMIN_PORT", default_value_t = 8081)]
    pub admin_port: u16,

    /// Admin session lifetime in seconds (how long a login stays valid).
    #[arg(long, env = "S3_ADMIN_SESSION_TTL", default_value_t = 3600)]
    pub admin_session_ttl_secs: u64,

    /// Public base URL of the S3 API (e.g. `https://api.example.com`), used by the
    /// admin panel when minting presigned links. Since a SigV4 presigned URL is
    /// signed over its host, this must be the host SDK clients actually reach.
    /// Required for presigning: when unset the admin panel refuses to mint
    /// presigned links (it cannot infer the API host from the admin request).
    #[arg(long, env = "S3_API_PUBLIC_URL")]
    pub api_public_url: Option<String>,
}

impl Config {
    /// Resolved credential pair, if both halves are present.
    #[must_use]
    pub fn credentials(&self) -> Option<(String, String)> {
        match (&self.access_key, &self.secret_key) {
            (Some(ak), Some(sk)) => Some((ak.clone(), sk.clone())),
            _ => None,
        }
    }

    /// Set of buckets that permit anonymous reads.
    #[must_use]
    pub fn public_bucket_set(&self) -> HashSet<String> {
        self.public_buckets.iter().cloned().collect()
    }

    /// Whether the admin panel should actually be installed: explicitly enabled
    /// *and* credentials are configured (there is nothing to authenticate against
    /// otherwise).
    #[must_use]
    pub fn admin_active(&self) -> bool {
        self.admin_enabled && self.credentials().is_some()
    }

    /// Parsed `host -> bucket` custom-domain map. Invalid entries are skipped.
    #[must_use]
    pub fn parsed_domain_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for entry in &self.domain_map {
            if let Some((host, bucket)) = entry.split_once('=') {
                let host = host.trim();
                let bucket = bucket.trim();
                if !host.is_empty() && !bucket.is_empty() {
                    map.insert(host.to_ascii_lowercase(), bucket.to_owned());
                }
            }
        }
        map
    }
}
