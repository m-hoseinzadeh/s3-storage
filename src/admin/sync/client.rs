//! Builds the S3 client used to read from a remote source (MinIO or any
//! S3-compatible endpoint).
//!
//! The connector is assembled by hand rather than taken from the SDK's defaults.
//! `aws-sdk-s3`'s `default-https-client` feature resolves to a rustls backed by
//! aws-lc-rs, whose build script needs cmake -- absent from the `rust:1-slim`
//! build stage -- and its `rustls` feature is the legacy stack, which would link
//! a second hyper (0.14) beside the hyper 1 the server already uses. Selecting
//! `ring` explicitly avoids both.

use std::time::Duration;

use aws_sdk_s3::config::{
    BehaviorVersion, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
    retry::RetryConfig, timeout::TimeoutConfig,
};
use aws_smithy_http_client::tls;

/// Everything needed to reach one remote S3/MinIO endpoint.
///
/// Credentials live here for the lifetime of a single run and are never
/// persisted; see the module docs on [`super`] for why.
pub(crate) struct SourceConfig {
    /// Base URL, e.g. `https://minio.example.com:9000`.
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    /// Address buckets as `host/bucket` rather than `bucket.host`.
    pub path_style: bool,
    /// PEM CA bundle to trust *in addition to* the platform roots, for a MinIO
    /// deployment behind a private CA or a self-signed certificate.
    pub ca_pem: Option<String>,
}

/// The default region. MinIO ignores the region unless `MINIO_REGION` is set,
/// in which case a mismatch surfaces as `SignatureDoesNotMatch`.
pub(crate) const DEFAULT_REGION: &str = "us-east-1";

/// How long to wait for the TCP+TLS handshake. Deliberately short: an
/// unreachable endpoint is the most common misconfiguration, and it should be
/// reported in seconds rather than after a retry storm.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle connection reuse across the many small GETs a sync issues.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Reject anything that is not a plain HTTP(S) URL with a host.
///
/// The endpoint is operator-supplied and the *server* is what connects to it, so
/// this is the one place to shut the door on `file:`, `gopher:` and friends. It
/// is not a full SSRF defence -- an authenticated admin can already read and
/// write all storage -- but it keeps the fetch to the protocol we mean.
pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| "endpoint must start with http:// or https://".to_owned())?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(format!("unsupported endpoint scheme `{scheme}`; use http or https"));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() {
        return Err("endpoint has no host".to_owned());
    }
    Ok(())
}

/// Build a client for `src`, or explain why the configuration is unusable.
pub(crate) fn build_client(src: &SourceConfig) -> Result<aws_sdk_s3::Client, String> {
    validate_endpoint(&src.endpoint)?;

    let mut trust_store = tls::TrustStore::empty().with_native_roots(true);
    if let Some(pem) = &src.ca_pem {
        trust_store = trust_store.with_pem_certificate(pem.as_bytes().to_vec());
    }
    let tls_context = tls::TlsContext::builder()
        .with_trust_store(trust_store)
        .build()
        .map_err(|e| format!("invalid CA certificate: {e}"))?;

    let http_client = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
        .tls_context(tls_context)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build_https();

    let credentials = Credentials::new(
        src.access_key.clone(),
        src.secret_key.clone(),
        src.session_token.clone(),
        None,
        "s3-storage-sync",
    );

    let region = if src.region.trim().is_empty() { DEFAULT_REGION } else { src.region.trim() };

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .http_client(http_client)
        .endpoint_url(src.endpoint.trim_end_matches('/').to_owned())
        .force_path_style(src.path_style)
        .region(Region::new(region.to_owned()))
        .credentials_provider(credentials)
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                // No operation timeout on purpose: a multi-gigabyte object
                // legitimately takes minutes to stream. A connection that stops
                // producing bytes is caught by stalled-stream protection, which
                // `BehaviorVersion::latest()` turns on.
                .build(),
        )
        .retry_config(RetryConfig::standard().with_max_attempts(3))
        // MinIO's flexible-checksum support varies by release, and the backend
        // recomputes MD5 on write regardless, so asking for either direction
        // only adds ways for a copy to fail.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .build();

    Ok(aws_sdk_s3::Client::from_conf(config))
}

#[cfg(test)]
mod tests {
    use super::validate_endpoint;

    #[test]
    fn http_and_https_endpoints_are_accepted() {
        assert!(validate_endpoint("http://127.0.0.1:9000").is_ok());
        assert!(validate_endpoint("https://minio.example.com").is_ok());
        assert!(validate_endpoint("HTTPS://minio.example.com/").is_ok());
    }

    #[test]
    fn non_http_schemes_are_refused() {
        for bad in ["file:///etc/passwd", "gopher://host", "ftp://host/x"] {
            assert!(validate_endpoint(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn a_missing_scheme_or_host_is_refused() {
        assert!(validate_endpoint("minio.example.com:9000").is_err());
        assert!(validate_endpoint("http://").is_err());
        assert!(validate_endpoint("http:///bucket").is_err());
    }
}
