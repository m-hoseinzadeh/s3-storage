//! Admin session authentication.
//!
//! Login compares the submitted access/secret key against the configured
//! credentials in constant time. On success a stateless, HMAC-SHA256-signed token
//! is issued and stored in an `HttpOnly` cookie; no server-side session state is
//! kept. The signing key is derived from the configured secret key, so tokens
//! survive restarts but are invalidated if the secret key changes.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::settings::SharedSettings;

type HmacSha256 = Hmac<Sha256>;

/// Failed logins that cost nothing, so an ordinary typo is not punished.
const FREE_ATTEMPTS: u32 = 3;
/// Penalty for the first failure past [`FREE_ATTEMPTS`]; it doubles from there.
const BASE_PENALTY: Duration = Duration::from_millis(250);
/// Ceiling on the penalty, so a locked-out operator is never stuck for long.
const MAX_PENALTY: Duration = Duration::from_secs(5);

/// Serializing, exponentially backing-off gate on the login endpoint.
///
/// Nothing rate-limited login before, so the admin port offered unlimited
/// guesses at the secret key. Attempts now queue behind a single lock and each
/// one waits out the penalty earned by the current failure streak, which caps
/// guessing at roughly one attempt per [`MAX_PENALTY`] no matter how many
/// requests are made in parallel.
///
/// Deliberately a delay rather than a lockout: with a single credential pair a
/// lockout would let anyone who can reach the port deny the operator access,
/// trading a brute-force risk for a denial-of-service one. A successful login
/// clears the streak, so a legitimate operator pays the penalty at most once.
#[derive(Debug, Default)]
pub struct LoginThrottle {
    /// Consecutive failures. Held across the delay so attempts cannot run in
    /// parallel to escape it.
    failures: tokio::sync::Mutex<u32>,
}

impl LoginThrottle {
    /// Take the login gate, waiting out any penalty owed. The caller must report
    /// the outcome on the returned guard.
    pub async fn acquire(&self) -> LoginGate<'_> {
        let failures = self.failures.lock().await;
        let penalty = Self::penalty(*failures);
        if !penalty.is_zero() {
            tokio::time::sleep(penalty).await;
        }
        LoginGate { failures }
    }

    fn penalty(failures: u32) -> Duration {
        let Some(over) = failures.checked_sub(FREE_ATTEMPTS) else { return Duration::ZERO };
        if over == 0 {
            return Duration::ZERO;
        }
        BASE_PENALTY
            .checked_mul(1u32.checked_shl(over - 1).unwrap_or(u32::MAX))
            .unwrap_or(MAX_PENALTY)
            .min(MAX_PENALTY)
    }
}

/// Holds the login gate for the duration of one attempt.
pub struct LoginGate<'a> {
    failures: tokio::sync::MutexGuard<'a, u32>,
}

impl LoginGate<'_> {
    pub fn succeeded(mut self) {
        *self.failures = 0;
    }

    pub fn failed(mut self) {
        *self.failures = self.failures.saturating_add(1);
    }
}

/// Name of the session cookie.
pub const COOKIE_NAME: &str = "s3admin_session";

/// Session configuration shared by the admin route. The lifetime (`ttl_secs`) is
/// read live from the settings store, so changing it in the panel affects sessions
/// issued thereafter without a restart.
#[derive(Debug, Clone)]
pub struct Sessions {
    access_key: String,
    secret_key: String,
    settings: SharedSettings,
    cookie_path: String,
}

impl Sessions {
    #[must_use]
    pub fn new(access_key: String, secret_key: String, settings: SharedSettings, cookie_path: String) -> Self {
        Self { access_key, secret_key, settings, cookie_path }
    }

    /// Current session lifetime in seconds (from the settings store).
    fn ttl_secs(&self) -> u64 {
        self.settings.session_ttl_secs()
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
        let exp = now_unix().saturating_add(i64::try_from(self.ttl_secs()).unwrap_or(i64::MAX));
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
    /// `Secure` is added only when the request reached us over HTTPS (`secure`).
    /// A `Secure` cookie is silently dropped by browsers on a non-secure context
    /// (plain HTTP on anything but `localhost`), which would leave the user unable
    /// to log in; emitting it conditionally keeps TLS deployments protected while
    /// still working when the panel is served over plain HTTP.
    #[must_use]
    pub fn set_cookie(&self, token: &str, secure: bool) -> String {
        let secure_attr = if secure { "; Secure" } else { "" };
        format!(
            "{COOKIE_NAME}={token}; HttpOnly{secure_attr}; SameSite=Strict; Path={}; Max-Age={}",
            self.cookie_path,
            self.ttl_secs()
        )
    }

    /// `Set-Cookie` value that clears the session token. `secure` must mirror the
    /// value used by [`Self::set_cookie`] so the clearing cookie matches.
    #[must_use]
    pub fn clear_cookie(&self, secure: bool) -> String {
        let secure_attr = if secure { "; Secure" } else { "" };
        format!("{COOKIE_NAME}=; HttpOnly{secure_attr}; SameSite=Strict; Path={}; Max-Age=0", self.cookie_path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn penalty_is_free_then_doubles_up_to_the_cap() {
        for failures in 0..=FREE_ATTEMPTS {
            assert_eq!(LoginThrottle::penalty(failures), Duration::ZERO, "{failures} should be free");
        }
        assert_eq!(LoginThrottle::penalty(FREE_ATTEMPTS + 1), BASE_PENALTY);
        assert_eq!(LoginThrottle::penalty(FREE_ATTEMPTS + 2), BASE_PENALTY * 2);
        assert_eq!(LoginThrottle::penalty(FREE_ATTEMPTS + 3), BASE_PENALTY * 4);
        // Never past the ceiling, and no overflow however long the streak runs.
        assert_eq!(LoginThrottle::penalty(FREE_ATTEMPTS + 40), MAX_PENALTY);
        assert_eq!(LoginThrottle::penalty(u32::MAX), MAX_PENALTY);
    }
}
