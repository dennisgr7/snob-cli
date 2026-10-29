//! The state handed out with a question on the stateless revision, signed so
//! that the answer that comes back with it counts for that question only.
//!
//! The client keeps the state between the question and its answer, and may
//! hand back anything; the protocol says to treat it as the caller's. So it
//! carries what was asked about and until when, and an HMAC-SHA256 over both
//! with a key this process drew at start and never shows: a state from
//! another server, another run, another call or a later hour does not open.
//!
//! Signed with `snob_core::watch::sign`, the webhook's own HMAC, rather than
//! the SDK's sealer, which would bring a second copy of the RustCrypto crates
//! into the tree for the same arithmetic.

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snob_core::secret::Secret;

/// Signs and opens states, under one key.
pub struct Sealer {
    key: Secret,
}

#[derive(Serialize, Deserialize)]
struct Sealed<T> {
    what: T,
    /// Seconds since the epoch past which it no longer opens.
    until: i64,
}

impl Sealer {
    /// A sealer under 256 random bits from the operating system.
    pub fn new() -> anyhow::Result<Self> {
        let mut key = [0u8; 32];
        crate::relay::fill_random(&mut key)?;
        Ok(Self::with_key(&key))
    }

    fn with_key(key: &[u8]) -> Self {
        Self {
            key: Secret::new(base64::engine::general_purpose::STANDARD.encode(key)),
        }
    }

    /// `what`, good until `until` (seconds since the epoch), as text.
    pub fn seal<T: Serialize>(&self, what: &T, until: i64) -> anyhow::Result<String> {
        let body = serde_json::to_string(&Sealed { what, until })?;
        let signature = snob_core::watch::sign::sign(&body, &self.key);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body);
        Ok(format!("{body}.{signature}"))
    }

    /// What `state` was sealed over, if this sealer sealed it and it is still
    /// good at `now`.
    pub fn open<T: DeserializeOwned>(&self, state: &str, now: i64) -> Option<T> {
        let (body, signature) = state.split_once('.')?;
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .ok()?;
        let body = String::from_utf8(body).ok()?;
        let expected = snob_core::watch::sign::sign(&body, &self.key);
        if !same(expected.as_bytes(), signature.as_bytes()) {
            return None;
        }
        let sealed: Sealed<T> = serde_json::from_str(&body).ok()?;
        (now <= sealed.until).then_some(sealed.what)
    }
}

/// Whether two signatures are equal, in a time that does not depend on where
/// they first differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_opens_under_its_own_key_until_it_expires() {
        let sealer = Sealer::with_key(&[7; 32]);
        let state = sealer.seal(&"follow a", 100).unwrap();
        assert_eq!(
            sealer.open::<String>(&state, 100).as_deref(),
            Some("follow a")
        );
        assert_eq!(sealer.open::<String>(&state, 101), None, "expired");
        assert_eq!(Sealer::with_key(&[8; 32]).open::<String>(&state, 0), None);
    }

    #[test]
    fn a_state_changed_in_any_way_does_not_open() {
        let sealer = Sealer::with_key(&[7; 32]);
        let state = sealer.seal(&"follow a", 100).unwrap();
        let (body, signature) = state.split_once('.').unwrap();
        let other = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"what":"follow b","until":100}"#);
        assert_eq!(
            sealer.open::<String>(&format!("{other}.{signature}"), 0),
            None
        );
        assert_eq!(sealer.open::<String>(&format!("{body}.sha256=00"), 0), None);
        assert_eq!(sealer.open::<String>(body, 0), None);
    }
}
