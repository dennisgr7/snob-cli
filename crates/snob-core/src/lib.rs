//! snob's domain: what the things are, and what follows from them.
//!
//! Models, set arithmetic, the diff between two captures, the schedule a
//! monitor runs on, the signature a report carries, and the interface the
//! request budget is asked through. **No I/O.** Nothing here opens a database,
//! makes a request, reads a file or names a directory — [`clock`] is the single
//! exception, and it is here because both of the crates above need one.
//!
//! That line is what the split is for. `snob_store` is the other side of it:
//! SQLite, the platform's directories, the keyring, and the monitor's
//! configuration file. Keeping them apart is what lets `snob-ig`, an HTTP
//! client, build without SQLite, three keyring backends and a TOML parser it
//! never calls, and without recompiling when the database schema changes.
//!
//! Identity rule: an account is always identified by its numeric `pk`, never by
//! its username. Usernames change, and detecting that change is itself an event
//! the tool reports.

pub mod budget;
pub mod clock;
pub mod duration;
pub mod filters;
pub mod model;
pub mod secret;
pub mod session;
pub mod sets;
pub mod watch;

/// The two moment types, at the crate root beside [`Pk`] because they are read
/// as widely as it is and for the same reason: `snob_core::Epoch` next to
/// `snob_core::Pk` in a signature says what the two arguments are, where
/// `clock::Epoch` would say where the clock lives.
pub use clock::{Epoch, EpochMs};

/// Stable numeric identifier of an Instagram account.
///
/// A type of its own rather than an alias for `u64`, because an id, a count
/// and a moment are all sixty-four-bit numbers and only one of them is an
/// account. `ListOutcome` holds `account_pk` next to `started_at` and
/// `taken_at`, and the compiler is what keeps them from being transposed.
///
/// What it deliberately does **not** have is as much of the point as what it
/// has. There is no `Deref` to `u64` and no `From<Pk> for u64`: with either,
/// `pk + 1`, `pk == followers` and `sum(pks)` would all go on compiling, and
/// an id that can be added to a count is an alias with extra syntax. The one
/// way out is [`Pk::get`], which is a sentence a reader can see.
///
/// [`Display`](std::fmt::Display) writes the bare digits, for URLs,
/// `account {pk}` labels and log lines, and `#[serde(transparent)]` makes the
/// JSON a bare number in both directions. Instagram's habit of sending the same
/// id as `123` and as `"123"` is handled by `flexible_pk` in `snob_ig::model`,
/// which builds a `Pk` from whichever arrived.
///
/// SQLite is the one place the conversion is not free, and it stays behind
/// `snob_store::store::pk_to_sql` / `pk_from_sql` — the reasoning is there.
/// Implementing `rusqlite`'s `ToSql` and `FromSql` here would put the bit-cast
/// in one place instead of two functions, which is better, and is not
/// available: this crate does **no I/O** and must not compile SQLite, while
/// the orphan rule stops `snob-store` writing the impls for a type it does not
/// own. `params![pk]` does not compile, for want of any `ToSql` at all.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Pk(u64);

impl Pk {
    /// The id this number names. `const` so it can build a constant, which
    /// `From` cannot.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The number back out, for the two places that need one: the SQLite
    /// bit-cast and a formatted address.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Pk {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// The bare number, which is what a URL, a label and a JSON value all want.
impl std::fmt::Display for Pk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Ids arrive as text often enough to be worth a standard trait: out of the
/// session cookie, out of a command line, out of the string half of an
/// Instagram response.
impl std::str::FromStr for Pk {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::Pk;

    /// Every shape this appears in on the wire and on screen is the bare
    /// number, and that is a contract with whoever consumes the tool.
    #[test]
    fn an_id_is_still_written_as_a_plain_number() {
        let pk = Pk::new(4_340_136_074);

        assert_eq!(pk.to_string(), "4340136074");
        assert_eq!(serde_json::to_string(&pk).unwrap(), "4340136074");
        assert_eq!(
            serde_json::from_str::<Pk>("4340136074").unwrap(),
            pk,
            "and reads back the same way"
        );
        assert_eq!("4340136074".parse::<Pk>().unwrap(), pk);
    }
}
