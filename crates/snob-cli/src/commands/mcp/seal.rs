//! The tickets handed out with a question: **a yes counts once, for the
//! command it was given for.**
//!
//! On the stateless revision the client keeps the state between the question
//! and its answer, and may hand back anything; the protocol says to treat it
//! as the caller's. So the state is a ticket that says what it is good for, and
//! the server keeps the one thing a ticket cannot carry: whether it was used.
//!
//! - **What it is good for.** A ticket is `nonce "." until "." mac`: 16 random
//!   bytes (base64url), the second of this process's life past which it no
//!   longer redeems, and the first 128 bits of an HMAC-SHA256, under a key this
//!   process drew at start and never shows, over its kind (a form the client
//!   shows, or a question put in the chat), the nonce, `until` and the
//!   binding, which is the command line the question was about
//!   (`tools::Plan::binding`). A
//!   ticket from another server, another run, another command or another kind
//!   does not verify, and one that does not verify spends nothing.
//! - **Once.** A ticket that verifies and is in time is spent: its nonce goes
//!   into a set kept until the ticket would have expired anyway. The same
//!   ticket offered again is [`Spoiled::Used`]: a client that sends the
//!   answered call twice runs the command once.
//! - **On the monotonic clock.** `until` counts from when the sealer was made,
//!   on `Instant`, not the wall clock: a clock stepped back after a prune would
//!   otherwise make a pruned, spent ticket good again. The key and the set live
//!   exactly as long as the process, so a count from its start is all a ticket
//!   needs. (Where the monotonic clock stops in a suspend, as on Linux, a
//!   ticket outlives its ten minutes by the time the machine slept.)
//! - **Bounded, failing closed.** At most [`MOST_SPENT`] spent nonces are
//!   kept; when the set is full of tickets still in time, the next redeem is
//!   refused ([`Spoiled::Crowded`]) rather than one of them forgotten, which
//!   would let it be used twice.
//!
//! Signed with `snob_core::watch::sign`, the webhook's own HMAC, rather than
//! the SDK's sealer, which would bring a second copy of the RustCrypto crates
//! into the tree for the same arithmetic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use snob_core::secret::Secret;

/// How many spent tickets still in time are kept at most. Far past honest use:
/// one ticket is issued per call that asks, and each lives ten minutes.
const MOST_SPENT: usize = 4096;

/// How many hex digits of the HMAC a ticket carries: 128 bits, half of
/// SHA-256's, which RFC 2104 allows a truncated MAC.
const MAC_DIGITS: usize = 32;

/// Which way a ticket was handed out, so one cannot stand in for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// With a form the client shows the person (`input_required`).
    Form,
    /// With a question the agent puts to the person in the chat.
    Chat,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Form => "form",
            Kind::Chat => "chat",
        }
    }
}

/// Why a ticket does not redeem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spoiled {
    /// Not one this server handed out for this kind and this command, or not
    /// a ticket at all. Nothing was spent.
    NotThisCall,
    /// Handed out for this call, and past its time.
    Expired,
    /// Already redeemed once.
    Used,
    /// Too many tickets in time were spent to keep one more, so none is taken
    /// until some expire.
    Crowded,
}

/// A ticket, as handed out.
#[derive(Debug, Clone)]
pub struct Ticket {
    /// The text the client hands back.
    pub token: String,
    /// When it stops redeeming, on the wall clock: **for showing only**; the
    /// ticket itself is judged on the monotonic one.
    pub expires_at: SystemTime,
}

/// Hands out and redeems tickets, under one key.
pub struct Sealer {
    key: Secret,
    /// What `until` counts from.
    born: Instant,
    /// The nonces of the tickets redeemed, with the second each expires.
    spent: Mutex<HashMap<[u8; 16], u64>>,
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
            born: Instant::now(),
            spent: Mutex::new(HashMap::new()),
        }
    }

    /// A ticket of `kind` for the command `binding`, good for `life`.
    pub fn issue(&self, kind: Kind, binding: &str, life: Duration) -> anyhow::Result<Ticket> {
        let mut nonce = [0u8; 16];
        crate::relay::fill_random(&mut nonce)?;
        Ok(Ticket {
            token: self.token(
                kind,
                binding,
                &nonce,
                self.now().saturating_add(life.as_secs()),
            ),
            expires_at: SystemTime::now() + life,
        })
    }

    /// Spends `token` if it is a ticket of `kind` for `binding`, still in
    /// time and never redeemed.
    pub fn redeem(&self, kind: Kind, token: &str, binding: &str) -> Result<(), Spoiled> {
        self.redeem_at(kind, token, binding, self.now())
    }

    /// Seconds of this sealer's life so far.
    fn now(&self) -> u64 {
        self.born.elapsed().as_secs()
    }

    fn token(&self, kind: Kind, binding: &str, nonce: &[u8; 16], until: u64) -> String {
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(nonce);
        let until = until.to_string();
        let mac = self.mac(kind, &nonce, &until, binding);
        format!("{nonce}.{until}.{mac}")
    }

    /// Over the text of the nonce and of `until` as the ticket carries them,
    /// so a ticket spelled any other way does not verify. The binding is
    /// JSON, whose strings escape every control character, so the NULs
    /// between the fields cannot be forged inside one.
    fn mac(&self, kind: Kind, nonce: &str, until: &str, binding: &str) -> String {
        let body = format!("snob-mcp/{}\0{nonce}\0{until}\0{binding}", kind.name());
        let signed = snob_core::watch::sign::sign(&body, &self.key);
        let hex = signed.strip_prefix("sha256=").unwrap_or(&signed);
        hex.chars().take(MAC_DIGITS).collect()
    }

    /// [`Sealer::redeem`] at `now` seconds of this sealer's life.
    fn redeem_at(&self, kind: Kind, token: &str, binding: &str, now: u64) -> Result<(), Spoiled> {
        let mut parts = token.split('.');
        let (Some(nonce), Some(until), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Spoiled::NotThisCall);
        };
        // Checked before anything in the ticket is believed, and checking it
        // spends nothing: a ticket for another call stays good for its own.
        let expected = self.mac(kind, nonce, until, binding);
        if !snob_core::watch::sign::same(expected.as_bytes(), mac.as_bytes()) {
            return Err(Spoiled::NotThisCall);
        }
        let (Ok(until), Some(nonce)) = (until.parse::<u64>(), nonce_of(nonce)) else {
            // Signed by this key and still not a ticket this code makes.
            return Err(Spoiled::NotThisCall);
        };
        if now > until {
            return Err(Spoiled::Expired);
        }
        // Never held across an await (there is none here), so of two calls
        // redeeming one ticket at once exactly one gets it.
        let mut spent = self
            .spent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // What is past its time is refused as expired before it is looked up,
        // so forgetting it lets nothing through twice.
        spent.retain(|_, until| *until >= now);
        if spent.contains_key(&nonce) {
            return Err(Spoiled::Used);
        }
        if spent.len() >= MOST_SPENT {
            return Err(Spoiled::Crowded);
        }
        spent.insert(nonce, until);
        Ok(())
    }
}

fn nonce_of(text: &str) -> Option<[u8; 16]> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()?
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIFE: u64 = 600;

    fn sealer() -> Sealer {
        Sealer::with_key(&[7; 32])
    }

    /// A ticket for `binding` handed out at second `at`, good for [`LIFE`].
    fn ticket(sealer: &Sealer, kind: Kind, binding: &str, nonce: u8, at: u64) -> String {
        sealer.token(kind, binding, &[nonce; 16], at + LIFE)
    }

    #[test]
    fn a_ticket_redeems_once() {
        let sealer = sealer();
        let issued = sealer
            .issue(Kind::Form, "follow a", Duration::from_secs(LIFE))
            .unwrap();
        assert_eq!(sealer.redeem(Kind::Form, &issued.token, "follow a"), Ok(()));
        assert_eq!(
            sealer.redeem(Kind::Form, &issued.token, "follow a"),
            Err(Spoiled::Used)
        );
        let other = sealer
            .issue(Kind::Form, "follow a", Duration::from_secs(LIFE))
            .unwrap();
        assert_ne!(other.token, issued.token, "each ticket has its own nonce");
        assert_eq!(sealer.redeem(Kind::Form, &other.token, "follow a"), Ok(()));
    }

    #[test]
    fn a_ticket_redeems_only_for_its_own_kind_and_command() {
        let sealer = sealer();
        let form = ticket(&sealer, Kind::Form, "follow a", 1, 0);
        assert_eq!(
            sealer.redeem_at(Kind::Chat, &form, "follow a", 0),
            Err(Spoiled::NotThisCall)
        );
        assert_eq!(
            sealer.redeem_at(Kind::Form, &form, "follow b", 0),
            Err(Spoiled::NotThisCall)
        );
        assert_eq!(
            Sealer::with_key(&[8; 32]).redeem_at(Kind::Form, &form, "follow a", 0),
            Err(Spoiled::NotThisCall),
            "another key, another server"
        );
        assert_eq!(sealer.redeem_at(Kind::Form, &form, "follow a", 0), Ok(()));
    }

    #[test]
    fn a_ticket_expires() {
        let sealer = sealer();
        let late = ticket(&sealer, Kind::Form, "follow a", 1, 0);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &late, "follow a", LIFE + 1),
            Err(Spoiled::Expired)
        );
        let last = ticket(&sealer, Kind::Form, "follow a", 2, 0);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &last, "follow a", LIFE),
            Ok(()),
            "good to its last second"
        );
    }

    #[test]
    fn a_ticket_for_another_call_spends_nothing() {
        let sealer = sealer();
        let mine = ticket(&sealer, Kind::Form, "list b", 1, 0);
        for _ in 0..3 {
            assert_eq!(
                sealer.redeem_at(Kind::Form, &mine, "list c", 0),
                Err(Spoiled::NotThisCall)
            );
        }
        assert_eq!(sealer.redeem_at(Kind::Form, &mine, "list b", 0), Ok(()));
    }

    #[test]
    fn a_changed_nonce_time_or_mac_is_not_this_call() {
        let sealer = sealer();
        let good = ticket(&sealer, Kind::Form, "follow a", 1, 0);
        let parts: Vec<&str> = good.split('.').collect();
        let (nonce, until, mac) = (parts[0], parts[1], parts[2]);
        let other_nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([2u8; 16]);
        let flipped: String = mac
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 && c == '0' {
                    '1'
                } else if i == 0 {
                    '0'
                } else {
                    c
                }
            })
            .collect();
        for changed in [
            format!("{other_nonce}.{until}.{mac}"),
            format!("{nonce}.{}.{mac}", LIFE * 10),
            format!("{nonce}.+{until}.{mac}"),
            format!("{nonce}.{until}.{flipped}"),
            format!("{nonce}.{until}.{}", &mac[..MAC_DIGITS - 1]),
            format!("{nonce}.{until}"),
            format!("{good}.more"),
            String::new(),
            "not a ticket".into(),
        ] {
            assert_eq!(
                sealer.redeem_at(Kind::Form, &changed, "follow a", 0),
                Err(Spoiled::NotThisCall),
                "{changed}"
            );
        }
        assert_eq!(sealer.redeem_at(Kind::Form, &good, "follow a", 0), Ok(()));
    }

    #[test]
    fn pruning_forgets_only_expired_tickets() {
        let sealer = sealer();
        let early = ticket(&sealer, Kind::Form, "list a", 1, 0);
        let later = ticket(&sealer, Kind::Form, "list a", 2, 100);
        assert_eq!(sealer.redeem_at(Kind::Form, &early, "list a", 0), Ok(()));
        assert_eq!(sealer.redeem_at(Kind::Form, &later, "list a", 100), Ok(()));

        // Past the first one's time, a redeem prunes it and keeps the second.
        let next = ticket(&sealer, Kind::Form, "list a", 3, LIFE + 1);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &next, "list a", LIFE + 1),
            Ok(())
        );
        let spent = sealer.spent.lock().unwrap();
        assert!(!spent.contains_key(&[1; 16]), "expired, forgotten");
        assert!(spent.contains_key(&[2; 16]), "still in time, kept");
        assert!(spent.contains_key(&[3; 16]));
        drop(spent);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &later, "list a", LIFE + 1),
            Err(Spoiled::Used)
        );
        assert_eq!(
            sealer.redeem_at(Kind::Form, &early, "list a", LIFE + 1),
            Err(Spoiled::Expired),
            "forgotten, and still refused"
        );
    }

    #[test]
    fn a_full_set_refuses_rather_than_forgets_a_ticket_in_time() {
        let sealer = sealer();
        {
            let mut spent = sealer.spent.lock().unwrap();
            for n in 0..MOST_SPENT as u32 {
                let mut nonce = [0xff; 16];
                nonce[..4].copy_from_slice(&n.to_be_bytes());
                spent.insert(nonce, LIFE);
            }
        }
        let one_more = ticket(&sealer, Kind::Form, "list a", 1, 0);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &one_more, "list a", 0),
            Err(Spoiled::Crowded)
        );
        // Once those are past their time, there is room again.
        let later = ticket(&sealer, Kind::Form, "list a", 2, LIFE + 1);
        assert_eq!(
            sealer.redeem_at(Kind::Form, &later, "list a", LIFE + 1),
            Ok(())
        );
    }
}
