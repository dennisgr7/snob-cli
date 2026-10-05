//! What time it is, in the two units this project counts in.
//!
//! Two functions, two types and a rule: **seconds are the domain unit and
//! milliseconds are the rate-control unit**, and nothing converts between them
//! by hand. A capture's `taken_at`, a run's `started_at` and a rename's `at` are
//! [`Epoch`]; a cooldown's `until_ms` and the budget's theoretical arrival time
//! are [`EpochMs`], because a pace of one request every 3.83 seconds cannot be
//! expressed in whole seconds at all.
//!
//! Here rather than in storage because `snob-ig` reads a clock too, to say how
//! long is left of a cooldown, and must not depend on the persistence crate to
//! do it.

use std::time::Duration;

/// A moment, in seconds since the Unix epoch.
///
/// A type of its own rather than an alias for `i64`, for the reason
/// [`Pk`](crate::Pk) is one: a moment, a count and a length of time are all
/// sixty-four-bit numbers, and only one of them is a point on the calendar.
/// The second half of the same argument is [`EpochMs`]: a `_ms` suffix on a
/// name is a convention and not a rule, and milliseconds handed to something
/// expecting seconds are a moment in 57840 AD that no test would print.
///
/// What it deliberately does **not** have is as much of the point as what it
/// has. No `Deref` to `i64` and no `From<Epoch> for i64`: with either,
/// `taken_at + 1`, `taken_at == max_age_secs` and `until_ms - taken_at` would
/// all go on compiling. No `Add<i64>` either, for the same reason — the number
/// added to a moment is a length of time, so it is a [`Duration`] and says so.
/// And no conversion between it and [`EpochMs`] except [`EpochMs::to_epoch`],
/// which is a sentence a reader can see.
///
/// [`Display`](std::fmt::Display) writes the bare number, and
/// `#[serde(transparent)]` makes the JSON a bare number in both directions.
/// SQLite keeps its `INTEGER` columns and goes through [`Epoch::get`] and
/// [`Epoch::new`] at the row boundary, for the reason [`Pk`](crate::Pk) gives
/// for having no `ToSql` impl.
///
/// `Default` is the epoch itself: the zero `#[serde(default)]` gives a
/// `taken_at` Instagram left out, and `unwrap_or_default` a `taken_at` column
/// the view it is read through cannot return as NULL. It is not a sentinel
/// anything tests for.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Epoch(i64);

/// A moment, in milliseconds since the Unix epoch.
///
/// Everything [`Epoch`]'s own doc says applies here; what is worth saying twice
/// is that the two are different types and the compiler is what keeps them
/// apart. This is the rate-control unit and it stays inside the budget, the
/// pacer and the cooldown: [`EpochMs::to_epoch`] is where it becomes something
/// a person is shown.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct EpochMs(i64);

impl Epoch {
    /// The moment this number names. `const` so it can build a constant, which
    /// `From` cannot.
    pub const fn new(seconds: i64) -> Self {
        Self(seconds)
    }

    /// The number back out, for the places that need one: a SQLite parameter,
    /// a `chrono` constructor, an arithmetic mean of two moments.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl EpochMs {
    /// The moment this number names. `const` so it can build a constant, which
    /// `From` cannot.
    pub const fn new(milliseconds: i64) -> Self {
        Self(milliseconds)
    }

    /// The number back out, for a SQLite parameter and for a wait computed
    /// against another one.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// The same moment in the unit every timestamp this tool reports uses.
    ///
    /// `Pacer::cooldown` answers in milliseconds while `created_at` and
    /// `validated_at` next to it in `whoami`'s object are in seconds, so
    /// something has to convert — and the date printed at the person and that
    /// JSON field are the same cooldown, which is why they may not do their own
    /// arithmetic. This is the one place that does it.
    ///
    /// `div_euclid` rather than `/`, so a moment before the epoch floors instead
    /// of rounding towards zero into the wrong second.
    pub const fn to_epoch(self) -> Epoch {
        Epoch(self.0.div_euclid(1_000))
    }

    /// [`EpochMs::to_epoch`] rounded up, for a moment that means "not
    /// before": the end of a cooldown, when a bucket frees up. Rounded down,
    /// a script that waits until it would wake up to a second early and be
    /// refused.
    pub const fn to_epoch_not_before(self) -> Epoch {
        Epoch(self.0.div_euclid(1_000) + (self.0.rem_euclid(1_000) > 0) as i64)
    }
}

/// The bare number, which is what a JSON value, a label and a log line all want.
impl std::fmt::Display for Epoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::fmt::Display for EpochMs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// A moment plus a length of time is a moment.
///
/// Saturating, so an absurd length stops at the end of time instead of
/// panicking a debug build or wrapping into the past in release.
impl std::ops::Add<Duration> for Epoch {
    type Output = Self;

    fn add(self, length: Duration) -> Self {
        Self(self.0.saturating_add(whole_seconds(length)))
    }
}

/// A moment minus a length of time is a moment.
impl std::ops::Sub<Duration> for Epoch {
    type Output = Self;

    fn sub(self, length: Duration) -> Self {
        Self(self.0.saturating_sub(whole_seconds(length)))
    }
}

/// A moment minus a moment is a length of time — **signed**, in seconds.
///
/// Not a [`Duration`], and the difference is not a preference. A [`Duration`]
/// cannot be negative, so the conversion would have to saturate at zero, and
/// every one of these differences is somewhere a negative answer means
/// something: a stored capture dated in the future is a clock that went
/// backwards, and `gap_between` takes the larger of two subtractions in
/// opposite orders precisely so the sign decides which one wins.
impl std::ops::Sub for Epoch {
    type Output = i64;

    fn sub(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl std::ops::Add<Duration> for EpochMs {
    type Output = Self;

    fn add(self, length: Duration) -> Self {
        Self(self.0.saturating_add(whole_milliseconds(length)))
    }
}

impl std::ops::Sub<Duration> for EpochMs {
    type Output = Self;

    fn sub(self, length: Duration) -> Self {
        Self(self.0.saturating_sub(whole_milliseconds(length)))
    }
}

/// Signed milliseconds, for the same reason the seconds version is signed: the
/// budget compares a stored theoretical arrival time against now, and "the
/// clock went backwards" is a case it handles rather than one it rules out.
impl std::ops::Sub for EpochMs {
    type Output = i64;

    fn sub(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// A length of time as a number this arithmetic can use. Saturating at the top,
/// because a [`Duration`] counts to 584 billion years and a moment does not.
fn whole_seconds(length: Duration) -> i64 {
    i64::try_from(length.as_secs()).unwrap_or(i64::MAX)
}

fn whole_milliseconds(length: Duration) -> i64 {
    i64::try_from(length.as_millis()).unwrap_or(i64::MAX)
}

/// Current moment in seconds, the domain unit.
pub fn now() -> Epoch {
    Epoch(chrono::Utc::now().timestamp())
}

/// Current moment in milliseconds, the rate-control unit.
pub fn now_ms() -> EpochMs {
    EpochMs(chrono::Utc::now().timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape a moment appears in on the wire and on disk is the bare
    /// number, and that is a contract with whoever consumes the tool: `at`,
    /// `taken_at`, `last_reported_at` and the session file's `created_at` all
    /// go through this.
    #[test]
    fn a_moment_is_still_written_as_a_plain_number() {
        let at = Epoch::new(1_786_310_990);

        assert_eq!(at.to_string(), "1786310990");
        assert_eq!(serde_json::to_string(&at).unwrap(), "1786310990");
        assert_eq!(
            serde_json::from_str::<Epoch>("1786310990").unwrap(),
            at,
            "and reads back the same way"
        );
    }

    /// A "not before" moment never names a second before it.
    #[test]
    fn a_moment_not_before_rounds_up_to_the_second() {
        assert_eq!(EpochMs::new(5_000).to_epoch_not_before(), Epoch::new(5));
        assert_eq!(EpochMs::new(5_001).to_epoch_not_before(), Epoch::new(6));
        assert_eq!(EpochMs::new(5_999).to_epoch_not_before(), Epoch::new(6));
        assert_eq!(EpochMs::new(-1).to_epoch_not_before(), Epoch::new(0));
        assert_eq!(EpochMs::new(5_999).to_epoch(), Epoch::new(5));
    }

    /// The same contract for the rate-control unit. Nothing outside the budget
    /// reads one today, and the day something does it must read a number.
    #[test]
    fn a_moment_in_milliseconds_is_still_written_as_a_plain_number() {
        let until = EpochMs::new(1_786_310_990_123);

        assert_eq!(until.to_string(), "1786310990123");
        assert_eq!(serde_json::to_string(&until).unwrap(), "1786310990123");
        assert_eq!(
            serde_json::from_str::<EpochMs>("1786310990123").unwrap(),
            until,
            "and reads back the same way"
        );
    }

    /// A cooldown ending mid-second ends in that second and not in the next
    /// one, and a moment before the epoch floors rather than rounding towards
    /// zero — `/` would answer -1 for the second below, which is a second and a
    /// half in the wrong direction.
    #[test]
    fn a_cooldown_end_floors_to_the_second_it_is_in() {
        assert_eq!(
            EpochMs::new(1_786_310_990_123).to_epoch(),
            Epoch::new(1_786_310_990)
        );
        assert_eq!(EpochMs::new(-1_500).to_epoch(), Epoch::new(-2));
    }

    /// The arithmetic a moment is allowed, and the direction each of it goes
    /// in. What is not here is what the type is for: there is no `Add<i64>` to
    /// test, and no way to subtract a second from a millisecond.
    #[test]
    fn a_moment_takes_a_length_of_time_and_gives_one_back() {
        let at = Epoch::new(1_000);

        assert_eq!(at + Duration::from_secs(60), Epoch::new(1_060));
        assert_eq!(at - Duration::from_secs(60), Epoch::new(940));
        assert_eq!(Epoch::new(1_060) - at, 60);
        assert_eq!(at - Epoch::new(1_060), -60, "and the difference is signed");

        assert_eq!(
            EpochMs::new(1_000) + Duration::from_secs(1),
            EpochMs::new(2_000)
        );
        assert_eq!(EpochMs::new(2_000) - EpochMs::new(1_000), 1_000);
    }

    /// Saturating rather than panicking, in every direction. A hand-edited
    /// `watch.toml` can carry an interval of billions of years.
    #[test]
    fn an_absurd_length_of_time_stops_at_the_end_of_time() {
        assert_eq!(Epoch::new(1_000) + Duration::MAX, Epoch::new(i64::MAX));
        assert_eq!(
            Epoch::new(-1_000) - Duration::MAX,
            Epoch::new(i64::MIN),
            "a length of time longer than a moment can hold is i64::MAX of them"
        );
        assert_eq!(Epoch::new(i64::MIN) - Epoch::new(i64::MAX), i64::MIN);
    }
}
