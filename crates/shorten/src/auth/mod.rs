//! Authentication primitives: the administrator API key, signed browser
//! sessions and the bounded per-IP rate limiter.

mod limiter;

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub use limiter::Limiter;

/// Session cookie name, unchanged from the previous implementation so an
/// existing browser session survives a cutover.
pub const COOKIE_NAME: &str = "shorten_session";

/// The administrator credential, stored as a digest so a comparison never
/// reads the live key and always runs in constant time.
#[derive(Clone, Debug)]
pub struct ApiKey {
    expected: [u8; 32],
}

impl ApiKey {
    #[must_use]
    pub fn new(value: &str) -> Self {
        Self {
            expected: Sha256::digest(value.as_bytes()).into(),
        }
    }

    /// Compare a candidate key against the stored digest in constant time.
    #[must_use]
    pub fn valid(&self, value: &str) -> bool {
        let actual = Sha256::digest(value.as_bytes());
        actual.as_slice().ct_eq(&self.expected).into()
    }
}

/// The signed session payload. Field names match the previous implementation
/// so a token issued by one service parses in the other.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub id: String,
    pub csrf: String,
    #[serde(rename = "iat")]
    pub issued_at: i64,
    #[serde(rename = "exp")]
    pub expires_at: i64,
}

/// Signed, expiring session tokens with support for secret rotation.
#[derive(Clone, Debug)]
pub struct Sessions {
    current: Vec<u8>,
    previous: Vec<u8>,
    ttl: Duration,
    secure: bool,
}

/// A session failure, reported with the same copy the console produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    Malformed,
    Signature,
    Payload,
    Claims,
    Expired,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Malformed => "invalid session",
            Self::Signature => "invalid session signature",
            Self::Payload => "invalid session payload",
            Self::Claims => "invalid session claims",
            Self::Expired => "expired or invalid session",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SessionError {}

impl Sessions {
    #[must_use]
    pub fn new(
        current: impl Into<String>,
        previous: impl Into<String>,
        ttl: Duration,
        secure: bool,
    ) -> Self {
        Self {
            current: current.into().into_bytes(),
            previous: previous.into().into_bytes(),
            ttl,
            secure,
        }
    }

    #[must_use]
    pub fn secure(&self) -> bool {
        self.secure
    }

    /// Issue a fresh session token and its claims.
    ///
    /// # Errors
    ///
    /// Propagates a failure of the system random source.
    pub fn issue(&self, now: DateTime<Utc>) -> Result<(String, Claims), SessionError> {
        let id = random_token(18).map_err(|_| SessionError::Payload)?;
        let csrf = random_token(24).map_err(|_| SessionError::Payload)?;
        let ttl_ms = i64::try_from(self.ttl.as_millis()).unwrap_or(i64::MAX);
        let claims = Claims {
            id,
            csrf,
            issued_at: now.timestamp(),
            expires_at: (now + chrono::Duration::milliseconds(ttl_ms)).timestamp(),
        };
        let payload = serde_json::to_vec(&claims).map_err(|_| SessionError::Payload)?;
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        let signature = sign(&encoded, &self.current);
        Ok((format!("{encoded}.{signature}"), claims))
    }

    /// Verify a token, accepting the previous secret while a rotation is live.
    ///
    /// # Errors
    ///
    /// Returns the session failure that applies, matching the previous copy.
    pub fn parse(&self, token: &str, now: DateTime<Utc>) -> Result<Claims, SessionError> {
        let separator = token.rfind('.').ok_or(SessionError::Malformed)?;
        if separator < 1 {
            return Err(SessionError::Malformed);
        }
        let (payload, signature) = (&token[..separator], &token[separator + 1..]);
        let valid = verify(payload, signature, &self.current)
            || (!self.previous.is_empty() && verify(payload, signature, &self.previous));
        if !valid {
            return Err(SessionError::Signature);
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| SessionError::Payload)?;
        let claims: Claims = serde_json::from_slice(&decoded).map_err(|_| SessionError::Claims)?;
        let now_second = now.timestamp();
        if claims.id.is_empty()
            || claims.csrf.is_empty()
            || claims.expires_at <= now_second
            || claims.issued_at > now_second + 60
        {
            return Err(SessionError::Expired);
        }
        Ok(claims)
    }

    /// Attributes for a session cookie that expires at `expires`.
    #[must_use]
    pub fn cookie_attributes(
        &self,
        token: &str,
        expires: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> CookieSpec {
        CookieSpec {
            name: COOKIE_NAME.to_string(),
            value: token.to_string(),
            path: Some("/".to_string()),
            http_only: true,
            secure: self.secure,
            same_site: SameSite::Strict,
            expires: Some(expires),
            max_age: Some((expires - now).num_seconds()),
            remove: false,
        }
    }

    /// Attributes that clear the session cookie.
    ///
    /// The previous implementation sent `Max-Age=-1`; RFC 6265 treats a
    /// negative `Max-Age` and an expired `Expires` the same way, so the
    /// removal marker plus an epoch expiry is equivalent for every client.
    #[must_use]
    pub fn clear_cookie_attributes(&self) -> CookieSpec {
        CookieSpec {
            name: COOKIE_NAME.to_string(),
            value: String::new(),
            path: Some("/".to_string()),
            http_only: true,
            secure: self.secure,
            same_site: SameSite::Strict,
            expires: Some(
                Utc.timestamp_opt(1, 0)
                    .single()
                    .expect("second 1 is in range"),
            ),
            max_age: Some(-1),
            remove: true,
        }
    }
}

/// The cookie attributes the HTTP layer turns into a `Set-Cookie` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookieSpec {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: SameSite,
    pub expires: Option<DateTime<Utc>>,
    pub max_age: Option<i64>,
    pub remove: bool,
}

/// `SameSite` values, mirroring the strict policy the service has always used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SameSite {
    Strict,
    #[default]
    Lax,
    None,
}

fn sign(payload: &str, secret: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify(payload: &str, signature: &str, secret: &[u8]) -> bool {
    let expected = sign(payload, secret);
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

fn random_token(size: usize) -> Result<String, std::io::Error> {
    let mut bytes = vec![0_u8; size];
    rand::rng().fill_bytes(&mut bytes);
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn compares_api_keys_in_constant_time() {
        let key = ApiKey::new("correct-secret-key");
        assert!(key.valid("correct-secret-key"));
        assert!(!key.valid("wrong"));
        assert!(!key.valid(""));
    }

    #[test]
    fn issues_and_parses_sessions() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let sessions = Sessions::new(
            "12345678901234567890123456789012",
            "",
            Duration::from_secs(3600),
            true,
        );
        let (token, claims) = sessions.issue(now).expect("issue");
        let parsed = sessions.parse(&token, now).expect("parse");
        assert_eq!(parsed.csrf, claims.csrf);
        assert_eq!(parsed.id, claims.id);
        assert_eq!(parsed.expires_at, claims.expires_at);
    }

    #[test]
    fn accepts_previous_secret_during_rotation() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let original = Sessions::new(
            "12345678901234567890123456789012",
            "",
            Duration::from_secs(3600),
            true,
        );
        let (token, _) = original.issue(now).expect("issue");
        let rotated = Sessions::new(
            "abcdefghijklmnopqrstuvwxyz123456",
            "12345678901234567890123456789012",
            Duration::from_secs(3600),
            true,
        );
        rotated.parse(&token, now).expect("rotated secret accepted");
        let without_rotation = Sessions::new(
            "abcdefghijklmnopqrstuvwxyz123456",
            "",
            Duration::from_secs(3600),
            true,
        );
        assert_eq!(
            without_rotation.parse(&token, now),
            Err(SessionError::Signature)
        );
    }

    #[test]
    fn rejects_expired_sessions() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let sessions = Sessions::new(
            "12345678901234567890123456789012",
            "",
            Duration::from_secs(3600),
            true,
        );
        let (token, _) = sessions.issue(now).expect("issue");
        let later = now + chrono::Duration::hours(2);
        assert_eq!(sessions.parse(&token, later), Err(SessionError::Expired));
    }

    #[test]
    fn rejects_tampered_tokens() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let sessions = Sessions::new(
            "12345678901234567890123456789012",
            "",
            Duration::from_secs(3600),
            true,
        );
        let (token, _) = sessions.issue(now).expect("issue");
        let tampered = format!("{token}.extra-signature-bytes");
        assert!(sessions.parse(&tampered, now).is_err());
        assert!(sessions.parse("no-separator", now).is_err());
        assert!(sessions.parse(".signature", now).is_err());
    }

    #[test]
    fn cookie_attributes_match_previous_policy() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 10, 0, 0).unwrap();
        let expires = now + chrono::Duration::hours(12);
        let sessions = Sessions::new(
            "12345678901234567890123456789012",
            "",
            Duration::from_secs(3600),
            true,
        );
        let cookie = sessions.cookie_attributes("token", expires, now);
        assert_eq!(cookie.name, "shorten_session");
        assert_eq!(cookie.path.as_deref(), Some("/"));
        assert!(cookie.http_only);
        assert!(cookie.secure);
        assert_eq!(cookie.same_site, SameSite::Strict);
        assert_eq!(cookie.max_age, Some(12 * 3600));
        let cleared = sessions.clear_cookie_attributes();
        assert_eq!(cleared.max_age, Some(-1));
        assert!(cleared.remove);
    }

    #[test]
    fn limiter_enforces_limit_and_capacity() {
        let limiter = Limiter::new(2, Duration::from_secs(60), 2);
        let now = Utc::now();
        assert!(limiter.allow("a", now));
        assert!(limiter.allow("a", now));
        assert!(!limiter.allow("a", now));
        let _ = limiter.allow("b", now);
        let _ = limiter.allow("c", now);
        assert!(limiter.len() <= 2, "entries={}", limiter.len());
    }

    #[test]
    fn limiter_window_resets() {
        let limiter = Limiter::new(1, Duration::from_secs(60), 8);
        let now = Utc::now();
        assert!(limiter.allow("a", now));
        assert!(!limiter.allow("a", now));
        let after = now + chrono::Duration::seconds(61);
        assert!(limiter.allow("a", after));
    }
}
