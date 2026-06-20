//! Admin session authentication.
//!
//! Login compares the submitted access/secret key against the configured
//! credentials in constant time. On success a stateless, HMAC-SHA256-signed token
//! is issued and stored in an `HttpOnly` cookie; no server-side session state is
//! kept. The signing key is derived from the configured secret key, so tokens
//! survive restarts but are invalidated if the secret key changes.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Name of the session cookie.
pub const COOKIE_NAME: &str = "s3admin_session";

/// Immutable session configuration shared by the admin route.
#[derive(Debug, Clone)]
pub struct Sessions {
    access_key: String,
    secret_key: String,
    ttl_secs: u64,
    cookie_path: String,
}

impl Sessions {
    #[must_use]
    pub fn new(access_key: String, secret_key: String, ttl_secs: u64, cookie_path: String) -> Self {
        Self { access_key, secret_key, ttl_secs, cookie_path }
    }

    /// Constant-time check of submitted credentials against the configured pair.
    #[must_use]
    pub fn verify_credentials(&self, access_key: &str, secret_key: &str) -> bool {
        let ak = access_key.as_bytes().ct_eq(self.access_key.as_bytes());
        let sk = secret_key.as_bytes().ct_eq(self.secret_key.as_bytes());
        (ak & sk).into()
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(self.secret_key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(msg);
        mac.finalize().into_bytes().to_vec()
    }

    fn b64(data: &[u8]) -> String {
        base64_simd::URL_SAFE_NO_PAD.encode_to_string(data)
    }

    fn unb64(s: &str) -> Option<Vec<u8>> {
        base64_simd::URL_SAFE_NO_PAD.decode_to_vec(s).ok()
    }

    /// Issue a signed token valid for `ttl_secs` from now.
    #[must_use]
    pub fn issue(&self) -> String {
        let exp = now_unix().saturating_add(i64::try_from(self.ttl_secs).unwrap_or(i64::MAX));
        let payload = serde_json::json!({ "sub": self.access_key, "exp": exp });
        let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
        let payload_b64 = Self::b64(&payload_bytes);
        let sig = self.sign(payload_b64.as_bytes());
        format!("{payload_b64}.{}", Self::b64(&sig))
    }

    /// Verify a token. Returns the bound access key when the signature is valid
    /// and the token has not expired.
    #[must_use]
    pub fn verify(&self, token: &str) -> Option<String> {
        let (payload_b64, sig_b64) = token.split_once('.')?;
        let expected = self.sign(payload_b64.as_bytes());
        let actual = Self::unb64(sig_b64)?;
        if !bool::from(expected.ct_eq(&actual)) {
            return None;
        }
        let payload: serde_json::Value = serde_json::from_slice(&Self::unb64(payload_b64)?).ok()?;
        let exp = payload.get("exp")?.as_i64()?;
        if exp <= now_unix() {
            return None;
        }
        let sub = payload.get("sub")?.as_str()?.to_owned();
        // Defence in depth: the token must be bound to the active access key.
        if !bool::from(sub.as_bytes().ct_eq(self.access_key.as_bytes())) {
            return None;
        }
        Some(sub)
    }

    /// `Set-Cookie` value that installs a fresh session token.
    ///
    /// `Secure` is set so the session cookie is only ever sent over HTTPS (the
    /// admin panel is intended to live on its own TLS-terminated domain).
    /// Browsers treat `localhost` as a secure context, so plain-HTTP local
    /// development still works.
    #[must_use]
    pub fn set_cookie(&self, token: &str) -> String {
        format!(
            "{COOKIE_NAME}={token}; HttpOnly; Secure; SameSite=Strict; Path={}; Max-Age={}",
            self.cookie_path, self.ttl_secs
        )
    }

    /// `Set-Cookie` value that clears the session token.
    #[must_use]
    pub fn clear_cookie(&self) -> String {
        format!("{COOKIE_NAME}=; HttpOnly; Secure; SameSite=Strict; Path={}; Max-Age=0", self.cookie_path)
    }
}

/// Extract the session token from a `Cookie` header value.
#[must_use]
pub fn token_from_cookies(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE_NAME).then(|| value.trim())
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
