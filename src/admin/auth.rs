//! Admin session authentication.
//!
//! Login compares the submitted access/secret key against the configured
//! credentials in constant time. On success a stateless, HMAC-SHA256-signed token
//! is issued and stored in an `HttpOnly` cookie; no server-side session state is
//! kept. The signing key is derived from the configured secret key, so tokens
//! survive restarts but are invalidated if the secret key changes.

use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::settings::SharedSettings;

type HmacSha256 = Hmac<Sha256>;

/// Failed logins that cost nothing, so an ordinary typo is not punished.
const FREE_ATTEMPTS: u32 = 3;
/// Cooldown after the first failure past [`FREE_ATTEMPTS`]; it doubles from there.
const BASE_COOLDOWN: Duration = Duration::from_millis(250);
/// Ceiling on the cooldown, so a legitimate operator is never locked out for long.
const MAX_COOLDOWN: Duration = Duration::from_secs(5);

/// Rate limit on the login endpoint.
///
/// Nothing limited login before, so the admin port offered unlimited guesses at the
/// secret key, as fast as connections could be opened. Each failure past
/// [`FREE_ATTEMPTS`] now opens a cooldown -- 250ms, doubling to [`MAX_COOLDOWN`] --
/// during which further attempts are refused outright, capping guessing at roughly
/// one attempt per cooldown however many requests arrive in parallel.
///
/// Refused, not delayed: an earlier version held a lock and slept out the penalty,
/// which throttled guessing but pinned a connection and a task per waiting attempt,
/// so a flood of attempts tied up the server for as long as it took the queue to
/// drain. Rejecting immediately with `429` and a `Retry-After` costs the server
/// nothing per attempt.
///
/// Deliberately a cooldown rather than an account lockout: with a single credential
/// pair there is nobody to fall back to, so a long lockout would let anyone who can
/// reach the port deny the operator access. A successful login clears the streak.
#[derive(Debug, Default)]
pub struct LoginThrottle {
    state: std::sync::Mutex<ThrottleState>,
}

#[derive(Debug, Default)]
struct ThrottleState {
    /// Consecutive failures.
    failures: u32,
    /// When the next attempt may be made; `None` means "right now".
    next_allowed: Option<Instant>,
}

impl LoginThrottle {
    /// Run one login attempt under the rate limit.
    ///
    /// `verify` is the credential check. It runs while the lock is held -- it is
    /// synchronous and constant-time, with nothing to await -- so checking the
    /// cooldown, verifying, and recording the outcome are one atomic step and
    /// parallel attempts cannot slip between them.
    ///
    /// Returns `Err(retry_after)` when the caller is in a cooldown, otherwise
    /// `Ok(true)` / `Ok(false)` for the verification result.
    pub fn attempt(&self, verify: impl FnOnce() -> bool) -> Result<bool, Duration> {
        // A poisoned lock would mean a panic inside `verify`; recover rather than
        // wedging the only way in to the panel.
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if let Some(next) = state.next_allowed
            && now < next
        {
            return Err(next.duration_since(now));
        }

        let ok = verify();
        if ok {
            state.failures = 0;
            state.next_allowed = None;
        } else {
            state.failures = state.failures.saturating_add(1);
            let cooldown = Self::cooldown(state.failures);
            state.next_allowed = (!cooldown.is_zero()).then(|| now + cooldown);
        }
        Ok(ok)
    }

    fn cooldown(failures: u32) -> Duration {
        let Some(over) = failures.checked_sub(FREE_ATTEMPTS) else { return Duration::ZERO };
        if over == 0 {
            return Duration::ZERO;
        }
        BASE_COOLDOWN
            .checked_mul(1u32.checked_shl(over - 1).unwrap_or(u32::MAX))
            .unwrap_or(MAX_COOLDOWN)
            .min(MAX_COOLDOWN)
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
    fn cooldown_is_free_then_doubles_up_to_the_cap() {
        for failures in 0..=FREE_ATTEMPTS {
            assert_eq!(LoginThrottle::cooldown(failures), Duration::ZERO, "{failures} should be free");
        }
        assert_eq!(LoginThrottle::cooldown(FREE_ATTEMPTS + 1), BASE_COOLDOWN);
        assert_eq!(LoginThrottle::cooldown(FREE_ATTEMPTS + 2), BASE_COOLDOWN * 2);
        assert_eq!(LoginThrottle::cooldown(FREE_ATTEMPTS + 3), BASE_COOLDOWN * 4);
        // Never past the ceiling, and no overflow however long the streak runs.
        assert_eq!(LoginThrottle::cooldown(FREE_ATTEMPTS + 40), MAX_COOLDOWN);
        assert_eq!(LoginThrottle::cooldown(u32::MAX), MAX_COOLDOWN);
    }

    /// The whole point of refusing rather than delaying: an attempt inside the
    /// cooldown must return immediately, not block the caller.
    #[test]
    fn attempts_inside_the_cooldown_are_refused_without_waiting() {
        let throttle = LoginThrottle::default();
        for _ in 0..=FREE_ATTEMPTS {
            assert_eq!(throttle.attempt(|| false), Ok(false));
        }
        let start = Instant::now();
        let retry = throttle.attempt(|| false).expect_err("must be throttled");
        assert!(retry <= BASE_COOLDOWN);
        assert!(start.elapsed() < Duration::from_millis(50), "refusal must not block");

        // A refused attempt never runs `verify`, so it cannot extend its own cooldown.
        let mut ran = false;
        let _ = throttle.attempt(|| {
            ran = true;
            true
        });
        assert!(!ran, "a throttled attempt must not check credentials");
    }
}
