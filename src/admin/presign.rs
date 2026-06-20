//! Minimal AWS SigV4 query-string signer for generating presigned URLs.
//!
//! `s3s` verifies presigned URLs but does not expose a generator, so we sign them
//! ourselves. The signature is self-consistent (we choose the credential scope and
//! sign with it), and because the generated link targets the same host the admin
//! panel runs on, `s3s`'s own verification accepts it. Region/service are fixed to
//! `us-east-1`/`s3`; `SimpleAuth` only resolves the secret by access key and
//! recomputes the signature from the scope declared in the URL.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";

/// Build a presigned URL for `method` on `/{bucket}/{key}` valid for `expires_secs`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn presign(
    access_key: &str,
    secret_key: &str,
    scheme: &str,
    host: &str,
    bucket: &str,
    key: &str,
    method: &str,
    expires_secs: u64,
) -> String {
    let now = OffsetDateTime::now_utc();
    let amz_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    );
    let datestamp = format!("{:04}{:02}{:02}", now.year(), now.month() as u8, now.day());

    let canonical_uri = format!("/{}/{}", uri_encode(bucket, false), uri_encode(key, true));

    let credential_scope = format!("{datestamp}/{REGION}/{SERVICE}/aws4_request");
    let credential = format!("{access_key}/{credential_scope}");

    // Query params that participate in the signature, sorted by key.
    let mut params: Vec<(&str, String)> = vec![
        ("X-Amz-Algorithm", ALGORITHM.to_owned()),
        ("X-Amz-Credential", uri_encode(&credential, false)),
        ("X-Amz-Date", amz_date.clone()),
        ("X-Amz-Expires", expires_secs.to_string()),
        ("X-Amz-SignedHeaders", "host".to_owned()),
    ];
    params.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_qs = params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let canonical_headers = format!("host:{host}\n");
    let signed_headers = "host";
    let payload_hash = "UNSIGNED-PAYLOAD";

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_qs}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let signing_key = signing_key(secret_key, &datestamp);
    let signature = hex_lower(&hmac(&signing_key, string_to_sign.as_bytes()));

    format!("{scheme}://{host}{canonical_uri}?{canonical_qs}&X-Amz-Signature={signature}")
}

fn signing_key(secret_key: &str, datestamp: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), datestamp.as_bytes());
    let k_region = hmac(&k_date, REGION.as_bytes());
    let k_service = hmac(&k_region, SERVICE.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex_lower(Sha256::digest(data).as_slice())
}

fn hex_lower(data: &[u8]) -> String {
    hex_simd::encode_to_string(data, hex_simd::AsciiCase::Lower)
}

/// AWS-style percent-encoding. Unreserved chars pass through; `/` is preserved
/// only when `keep_slash` is set (object key path segments).
fn uri_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
