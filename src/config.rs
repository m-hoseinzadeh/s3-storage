//! Runtime configuration, sourced from CLI flags overlaid on environment variables.
//!
//! This holds only the startup/bootstrap settings that cannot be self-served:
//! bind address, ports, credentials, and whether the admin panel is enabled.
//! The mutable, deployment-facing settings (public buckets, virtual-host domains,
//! custom-domain mappings, the public API URL and the admin session TTL) are
//! persisted in the settings store ([`crate::settings`]) and managed exclusively
//! through the admin panel.

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
    /// marked public in the admin panel. Intended to sit behind a CDN/asset domain.
    #[arg(long, env = "S3_PUBLIC_PORT", default_value_t = 8082)]
    pub public_port: u16,

    /// Access key for SigV4 authentication. Must be set together with `--secret-key`.
    #[arg(long, env = "S3_ACCESS_KEY")]
    pub access_key: Option<String>,

    /// Secret key for SigV4 authentication. Must be set together with `--access-key`.
    #[arg(long, env = "S3_SECRET_KEY")]
    pub secret_key: Option<String>,

    /// Enable the embedded web admin panel. Requires credentials
    /// (`--access-key`/`--secret-key`) to be configured; otherwise it stays disabled.
    #[arg(long, env = "S3_ADMIN_ENABLED", default_value_t = false)]
    pub admin_enabled: bool,

    /// Port for the admin panel. The panel (SPA + its JSON API) is served at the
    /// root of this dedicated port, so it can sit behind its own admin domain.
    #[arg(long, env = "S3_ADMIN_PORT", default_value_t = 8081)]
    pub admin_port: u16,
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

    /// Whether the admin panel should actually be installed: explicitly enabled
    /// *and* credentials are configured (there is nothing to authenticate against
    /// otherwise).
    #[must_use]
    pub fn admin_active(&self) -> bool {
        self.admin_enabled && self.credentials().is_some()
    }
}
