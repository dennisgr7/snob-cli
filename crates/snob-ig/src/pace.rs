//! Request pacing and cancellation.
//!
//! **The numbers in [`Pace`] are not changed without a documented reason.**
//! Each one carries below what it is for and where it comes from, and that is
//! what stops a number being quietly tuned down until the tool is asking far
//! more of Instagram's service than answering the question needs. Slower is
//! always an acceptable answer here, and faster has to be argued for. The
//! page size is the one number chosen knowing it makes more requests: it is
//! the web app's own twelve ([`ACCOUNTS_PER_PAGE`]), so a list takes more
//! pages, each of them the page instagram.com asks for.
//!
//! Where they come from, since none of them is a published rate:
//!
//! - **The cadence** is the web app's, paced per user action, on the owner's
//!   decision that snob's calls keep the rhythm of the app whose calls they
//!   are. An action, to the pace, is opening a profile, a sitting of a list or
//!   a download; the narrower count [`Pacer::begin_action`] keeps is explained
//!   there. Within one, the pages follow each other at about the speed
//!   instagram.com asks for them: a recorded session pages a list at a median
//!   of 1.2 seconds a page, twenty-six pages in 37.5 seconds with no pause, and
//!   opens profiles 0.6 to 2.1 seconds apart. snob waits one to three seconds
//!   between the pages of an action ([`STEP_MS`]), slower than that, waits as
//!   long as a person looks at a profile before the first page of each list
//!   ([`DWELL_MS`]), and after [`PAGES_PER_SITTING`] pages, about 480
//!   accounts, rests five to fifteen minutes ([`SITTING_PAUSE_MS`]),
//!   as somebody scrolling a list stops somewhere. A page every minute or two
//!   is a rhythm no person scrolling keeps, and it bounds nothing the daily
//!   ceiling below does not bound already.
//! - **The volume** is what the account is judged on. Meta's paper on the
//!   system that issues these warnings (arXiv 2502.17693, February 2025)
//!   weighs each request by the accounts it returns, and a list page is
//!   nothing but accounts. The daily ceiling on those is its own budget,
//!   `budget::accounts_per_day`, not a pace, and it is the one that binds: two
//!   thousand accounts are about 167 pages a day, however fast they come.
//!   Except under `--same-day` (`pager::OverBudget::Continue`), which reads
//!   past it on purpose: the day's reading is then bounded only by this pace
//!   and the request budget's daily bucket, about two thousand pages (some
//!   24,000 accounts) a day.
//! - **The error handling** is this project's own: the first push-back is a
//!   hard stop and a cooldown. InstagramUnfollowers' failure path was
//!   `catch { continue; }`, an unbounded retry, and nobody should "restore
//!   fidelity" by copying it.
//!
//! The request budget's pace bucket (`PACE_EMISSION_MS` in `snob-store`'s
//! `rate_budget.rs`) sits under all of it and keeps its numbers: it lets about
//! forty pages a sitting through at two seconds a step before it starts
//! pacing them, and a sitting's rest refills it.
//!
//! **The requests that go with the cadence** are the app's, and each is paid
//! for as a read: a profile is opened with four (its query and the three the
//! app sends beside it), a list with the app's navigation, and each list page
//! is followed by the app's `show_many` once the page has shown the token it
//! needs, as the recording of 2026-10-01 shows the app sending them. None of
//! them has a wait of its own: the app sends them within a second of what they
//! go with. With `show_many` a page is two requests, and the pace bucket starts
//! pacing a sitting after about fourteen pages rather than forty, which is
//! slower and is left so.
//!
//! Not evidence, because both get quoted at this problem: the "200 calls per
//! user per hour" is Meta's Graph API limit for `graph.facebook.com`, which
//! has nothing to do with these endpoints; and a recorded session in which
//! nothing was refused says what the app does, not how far past it one may
//! go. Instagram's `x-ig-capacity-level` and `x-ig-peak-time` are written down
//! when it pushes back (`IgClient::note_push_back`) and deliberately not acted
//! on: they describe a datacenter's headroom, not a checkpoint on one account.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use snob_core::EpochMs;
use snob_core::budget::RateBudget;

/// Accounts asked for per list page: what the web app asks for, `count=12`
/// on both lists, of which it gets eight to twelve back.
///
/// **It makes more requests than a larger page would**: a list endpoint
/// serves up to fifty a page when asked for them, so a list takes two to
/// four times the pages it would at fifty. It is twelve on the owner's
/// decision that a list is read in the pages the app reads it in, on both
/// paths, rather than in pages no browser asks for.
pub const ACCOUNTS_PER_PAGE: u32 = 12;

/// The wait between the pages of one action, in milliseconds, drawn anew
/// each time: one to three seconds.
///
/// The recorded web app pages a list at a median of 1.2 seconds a page and
/// opens profiles 0.6 to 2.1 seconds apart; a median step of two seconds is
/// slower than both, and the jitter keeps the pages from landing on a beat no
/// person scrolls to.
pub const STEP_MS: (u64, u64) = (1_000, 3_000);

/// The wait before the first page of a list, in milliseconds, drawn anew for
/// each list: one and a half to four seconds.
///
/// A person opens the profile, looks at it, and only then opens its list. In
/// the recording of 2026-10-01 the gap between a profile opening and the
/// click on its followers or following was 2.55 seconds at the median,
/// between 1.1 and 4.5; snob used to ask the first page the moment the
/// profile answered, which no person does. The range sits inside what was
/// measured and its middle near the median, so the first page lands where a
/// person's click would. It is paid before every list walked, so the second
/// list of a crossing waits it too, as the second click does.
pub const DWELL_MS: (u64, u64) = (1_500, 4_000);

/// How many pages make a sitting: forty, about 480 accounts at
/// [`ACCOUNTS_PER_PAGE`].
///
/// The recorded session pages twenty-six in a row with no pause; forty is a
/// long scroll, and it is about a quarter of the day's accounts
/// (`budget::accounts_per_day`), so a day's reading is spread over four
/// sittings or more rather than read in one.
pub const PAGES_PER_SITTING: u32 = 40;

/// The rest after a full sitting, in milliseconds: five to fifteen minutes.
///
/// Long enough to refill the request budget's pace bucket, which forty
/// pages at two seconds nearly empty, and to be the break a person reading
/// a long list takes; short enough that the longest quiet stretch between
/// two saved pages stays well inside a walk's claim (`CLAIM_TTL_SECS` in
/// `snob-store`'s `snapshots.rs`).
pub const SITTING_PAUSE_MS: (u64, u64) = (300_000, 900_000);

/// Request cadence: within an action, and between sittings.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// Accounts asked for per page: [`ACCOUNTS_PER_PAGE`], the web app's.
    pub per_page: u32,
    /// The wait before each page of an action after its first: [`STEP_MS`].
    pub step_ms: (u64, u64),
    /// The wait before the first page of a list: [`DWELL_MS`].
    pub dwell_ms: (u64, u64),
    /// The rest once a sitting is full: [`SITTING_PAUSE_MS`].
    pub sitting_pause_ms: (u64, u64),
    /// How many pages make a sitting: [`PAGES_PER_SITTING`].
    pub pages_per_sitting: u32,
    /// How many times a network failure is retried before giving up.
    pub network_retries: u32,
    /// Base of the exponential delay between retries.
    pub backoff_base_ms: u64,
}

impl Default for Pace {
    /// **A page every one to three seconds, and a rest of five to fifteen
    /// minutes about every 480 accounts.** The reasons are on the constants
    /// and at the top of this module.
    ///
    /// A list of a thousand takes a quarter of an hour to half an hour,
    /// most of it two rests, and the daily ceiling on accounts
    /// (`budget::accounts_per_day`) still decides how much is read in a day,
    /// except under `--same-day`, where only the request budget's two
    /// thousand requests a day do.
    fn default() -> Self {
        Self {
            per_page: ACCOUNTS_PER_PAGE,
            step_ms: STEP_MS,
            dwell_ms: DWELL_MS,
            sitting_pause_ms: SITTING_PAUSE_MS,
            pages_per_sitting: PAGES_PER_SITTING,
            network_retries: 3,
            backoff_base_ms: 2_000,
        }
    }
}

impl Pace {
    /// The pace for somebody else's lists: the same cadence, and less
    /// insistence on a network failure.
    ///
    /// One cadence for every list, on the owner's decision: the app pages
    /// somebody else's followers as it pages the viewer's own, and a
    /// different rhythm would be one more thing that is not the app's. What
    /// differs is the error handling. On somebody else's account a network
    /// failure is a reason to stop rather than to insist, so it is retried
    /// one time fewer and after a longer wait.
    pub fn third_party() -> Self {
        Self {
            network_retries: 2,
            backoff_base_ms: 3_000,
            ..Self::default()
        }
    }

    /// One wait between the pages of an action, inside [`Self::step_ms`].
    pub fn step(&self) -> Duration {
        jitter(self.step_ms)
    }

    /// The wait before the first page of a list, inside [`Self::dwell_ms`].
    pub fn dwell(&self) -> Duration {
        jitter(self.dwell_ms)
    }

    /// One rest after a full sitting, inside [`Self::sitting_pause_ms`].
    pub fn sitting_pause(&self) -> Duration {
        jitter(self.sitting_pause_ms)
    }

    pub(crate) fn backoff(&self, attempt: u32) -> Duration {
        let base = self.backoff_base_ms.saturating_mul(1u64 << attempt.min(6));
        Duration::from_millis(base)
    }
}

/// Random wait inside the range, both ends included.
fn jitter((min, max): (u64, u64)) -> Duration {
    if max <= min {
        return Duration::from_millis(min);
    }
    Duration::from_millis(fastrand::u64(min..=max))
}

/// Who pays for a request, and who gets told when paying means waiting.
///
/// It exists so that making a request and paying for it cannot come apart: the
/// only way to reach Instagram is through [`crate::client::IgClient`], and the
/// only way through it is past here.
///
/// The wait it imposes **adds to** the walker's own step rather than
/// replacing it, so a walk running on an exhausted budget goes slightly slower
/// than the arithmetic suggests. That is the safe direction to be wrong in, and
/// it only happens once the budget is already rationing.
pub struct Pacer {
    budget: Arc<dyn RateBudget>,
    cancel: CancelToken,
    /// Told before a wait, so whoever is driving can say why nothing is
    /// happening. `None` stays silent, which is what tests and machine-readable
    /// runs want.
    announce: Option<Arc<dyn Fn(Duration) + Send + Sync>>,
    /// How many requests have been paid for. See [`Pacer::spent`].
    spent: AtomicU32,
    /// How many pages have been walked since the last rest. See
    /// [`Pacer::page_walked`].
    sitting_pages: AtomicU32,
    /// How many actions have begun. See [`Pacer::begin_action`].
    actions: AtomicU32,
}

impl Pacer {
    pub fn new(budget: Arc<dyn RateBudget>) -> Self {
        Self {
            budget,
            cancel: CancelToken::default(),
            announce: None,
            spent: AtomicU32::new(0),
            sitting_pages: AtomicU32::new(0),
            actions: AtomicU32::new(0),
        }
    }

    /// Marks the start of an action long enough for the account to move in:
    /// a walk, each sitting after its rest, a monitor tick.
    ///
    /// Requests are paced per action (see the module doc), and an action is
    /// also the unit of time something read can be trusted over: the
    /// requests that open a profile go out within a second of each other,
    /// while a walk takes minutes in which the account can move. So what is
    /// remembered across requests is stamped with [`Self::actions`] rather
    /// than with [`Self::spent`], and goes stale when a new action begins.
    ///
    /// The count is advanced only where such a stretch of time begins.
    /// Opening a profile and a download are actions to the pace, stepped as
    /// any other, but do not advance it: their requests are seconds apart,
    /// and nothing read before one is asked again after it on the same
    /// client.
    pub fn begin_action(&self) {
        self.actions.fetch_add(1, Ordering::Relaxed);
    }

    /// How many actions have begun since the client was built. See
    /// [`Self::begin_action`].
    pub fn actions(&self) -> u32 {
        self.actions.load(Ordering::Relaxed)
    }

    /// Counts a page walked, and answers whether the sitting is full.
    ///
    /// Counted here rather than by each walk because a sitting is the
    /// reading this process does, not one list's: `scan` walks two lists in
    /// a row, and a profile's mutual followers are pages too, so a counter
    /// per walk would let two walks of thirty pages go by as two short
    /// sittings. A full sitting stays full until [`Self::sitting_rested`],
    /// so one whose walk ended on it is rested after the next page anybody
    /// walks.
    pub fn page_walked(&self, pace: &Pace) -> bool {
        let walked = self.sitting_pages.fetch_add(1, Ordering::Relaxed) + 1;
        walked >= pace.pages_per_sitting.max(1)
    }

    /// Starts a new sitting, after its rest.
    pub fn sitting_rested(&self) {
        self.sitting_pages.store(0, Ordering::Relaxed);
    }

    /// How many requests have been paid for since the client was built.
    ///
    /// This is the honest number, and the only one: it counts what the budget
    /// was charged, so retries, the profile lookup and the counter poll are all
    /// in it. Counting successful pages instead would understate a walk that
    /// hit a 503 and recovered, telling the user three requests while the
    /// budget had been charged five.
    pub fn spent(&self) -> u32 {
        self.spent.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets who gets told about a wait the budget imposed.
    #[must_use]
    pub fn announcing(mut self, announce: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.announce = Some(announce);
        self
    }

    /// Grants everything and counts nothing. **Tests only**: using it against
    /// Instagram skips rate control entirely.
    #[doc(hidden)]
    pub fn unlimited() -> Self {
        Self::new(Arc::new(snob_core::budget::UnlimitedRateBudget))
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }

    /// Until when the account is in cooldown, if it is.
    pub fn cooldown(&self) -> Result<Option<EpochMs>, crate::error::IgError> {
        self.budget
            .cooldown()
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// The brake on every account, when one stands. [`Self::cooldown`]
    /// already counts it; this names the accounts behind it.
    pub fn brake(&self) -> Result<Option<snob_core::budget::Brake>, crate::error::IgError> {
        self.budget
            .brake()
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Records that Instagram pushed back, so the next run does not walk
    /// straight into it again.
    pub fn start_cooldown(
        &self,
        reason: &str,
        minimum: Duration,
    ) -> Result<EpochMs, crate::error::IgError> {
        self.budget
            .start_cooldown(reason, minimum)
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Reads the cooldown off the async worker, through the hop
    /// [`Self::off_thread`] explains.
    ///
    /// The same database, the same five-second busy timeout, and the same
    /// position: before every single request. [`Pacer::cooldown`] stays
    /// synchronous because its callers are gates in `snob-cli` that run once,
    /// with nothing else on the runtime waiting — this one runs in the hot path,
    /// where a blocked worker is the Ctrl+C that does nothing.
    ///
    /// The pager asks it too, before the walk and once per page, for the same
    /// reason: a second `snob` holding the write lock would otherwise stall a
    /// worker for up to the busy timeout, with an interrupt going nowhere.
    pub(crate) async fn cooldown_off_thread(
        &self,
    ) -> Result<Option<EpochMs>, crate::error::IgError> {
        self.off_thread(|budget| budget.cooldown()).await
    }

    /// How long until `accounts` more may be read today. See
    /// [`RateBudget::accounts_wait`]; off the async worker for the reason
    /// `Self::cooldown_off_thread` gives.
    pub async fn accounts_wait(&self, accounts: u32) -> Result<Duration, crate::error::IgError> {
        self.off_thread(move |budget| budget.accounts_wait(accounts))
            .await
    }

    /// Records the accounts a list page carried.
    pub async fn spend_accounts(&self, accounts: u32) -> Result<(), crate::error::IgError> {
        self.off_thread(move |budget| budget.spend_accounts(accounts))
            .await
    }

    /// How many accounts may still be read today without waiting.
    pub fn accounts_left(&self) -> Result<u32, crate::error::IgError> {
        self.budget
            .accounts_left()
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    async fn charge(&self, write: bool) -> Result<Duration, crate::error::IgError> {
        self.off_thread(move |budget| {
            if write {
                budget.reserve_write()
            } else {
                budget.reserve()
            }
        })
        .await
    }

    /// Asks the budget something off the async worker: the one copy of the
    /// hop every budget call on the hot path makes.
    ///
    /// `spawn_blocking` rather than `block_in_place`, which would be simpler
    /// and needs no clone: `block_in_place` panics on a current-thread
    /// runtime, and that is what `#[tokio::test]` builds by default. A tool
    /// whose tests cannot run it is not a tool this code can use.
    async fn off_thread<T: Send + 'static>(
        &self,
        ask: impl FnOnce(&dyn RateBudget) -> Result<T, snob_core::budget::RateBudgetError>
        + Send
        + 'static,
    ) -> Result<T, crate::error::IgError> {
        let budget = Arc::clone(&self.budget);
        tokio::task::spawn_blocking(move || ask(budget.as_ref()))
            .await
            .map_err(|e| crate::error::IgError::Budget(format!("the budget task failed: {e}")))?
            .map_err(|e| crate::error::IgError::Budget(e.to_string()))
    }

    /// Takes a slot and waits for it. Every request goes through here.
    ///
    /// Which is why the cancellation is read here rather than left to each
    /// caller: read only *inside* the wait, a canceled run with nothing owed
    /// would keep sending, and every loop that had to remember to check
    /// between requests would be a loop that could forget. "No request is sent
    /// after cancellation" lives in the same place as "every request is paid
    /// for", and is as hard to get around.
    ///
    /// Before the reservation, not after: refusing to send and charging for it
    /// anyway is the one combination that helps nobody.
    pub(crate) async fn clear_to_send(&self) -> Result<(), crate::error::IgError> {
        self.clear(false).await
    }

    /// The same for a write, which pays the read budgets and the write bucket.
    ///
    /// A separate method rather than a boolean on the one above, so that the
    /// choice is made by which of `IgClient::get` and `IgClient::post` is
    /// calling and not by an argument somebody could pass wrongly. There is no
    /// way to send a follow or an unfollow that goes through the cheaper one.
    pub(crate) async fn clear_to_send_write(&self) -> Result<(), crate::error::IgError> {
        self.clear(true).await
    }

    /// Waits until a write would be sent without waiting, and spends
    /// nothing: a write from the browser pays that wait here, before the
    /// profile it is made from is loaded, rather than between the two
    /// (`IgClient::write_from_the_page`). Its own [`Self::clear_to_send_write`]
    /// still charges it, and waits for anything another process took in
    /// the meantime.
    ///
    /// Canceled and refused in a cooldown as a reservation is, and for the
    /// same reasons.
    pub(crate) async fn rested_for_a_write(&self) -> Result<(), crate::error::IgError> {
        if self.cancel.is_canceled() {
            return Err(crate::error::IgError::Canceled);
        }
        if let Some(until_ms) = self.cooldown_off_thread().await? {
            return Err(crate::error::IgError::InCooldown { until_ms });
        }
        let owed = self.off_thread(|budget| budget.write_wait()).await?;
        if owed.is_zero() {
            return Ok(());
        }
        if let Some(announce) = &self.announce {
            announce(owed);
        }
        if self.cancel.sleep_or_cancel(owed).await {
            return Err(crate::error::IgError::Canceled);
        }
        Ok(())
    }

    async fn clear(&self, write: bool) -> Result<(), crate::error::IgError> {
        if self.cancel.is_canceled() {
            return Err(crate::error::IgError::Canceled);
        }

        // **Nothing is spent during a cooldown.** The budget charges its
        // buckets without ever reading the `cooldowns` table, so without this
        // the rule would hold only as long as every caller asked first — and a
        // poller such as `engine::check` would knock on a door Instagram has
        // just closed, once per account, at whatever interval it runs.
        //
        // Here for the same reason the cancellation above is here: this is the
        // one place every request passes through, so a caller that forgets
        // still cannot spend. The explicit gates stay. They do two things a
        // backstop cannot — refuse before asking the user for consent, and serve
        // a stored list instead of failing — and this is the net underneath
        // them, not their replacement.
        //
        // `SNOB_IGNORE_COOLDOWN` is read inside `SqliteRateBudget::cooldown`,
        // so it answers `None` here exactly as it does at every other gate.
        //
        // Before the reservation, like the cancel: a refusal that charges for
        // itself helps nobody.
        if let Some(until_ms) = self.cooldown_off_thread().await? {
            return Err(crate::error::IgError::InCooldown { until_ms });
        }

        // `charge` opens an immediate transaction against a database other
        // snob processes share, so under contention it sits on the
        // five-second busy timeout, off the async worker, before every single
        // request. The public pair `clear_to_send`/`clear_to_send_write` stays
        // two methods on purpose: the write budget is a different promise.
        let owed = self.charge(write).await?;
        // Counted at the reservation rather than at the answer: the budget has
        // been charged by now whatever the server goes on to say.
        self.spent.fetch_add(1, Ordering::Relaxed);

        if owed.is_zero() {
            return Ok(());
        }
        if let Some(announce) = &self.announce {
            announce(owed);
        }
        if self.cancel.sleep_or_cancel(owed).await {
            return Err(crate::error::IgError::Canceled);
        }
        Ok(())
    }
}

/// Shared cancellation token.
///
/// Used instead of awaiting the signal directly on every iteration because a
/// `select!` inside a loop rebuilds its branches each time round, which would
/// throw away the signal future over and over.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancelToken {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub fn is_canceled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Resolves when the run is canceled, and never otherwise.
    ///
    /// Public because [`crate::client::IgClient`] races it against a request in
    /// flight. `sleep_or_cancel` covers a wait this program chose to take; this
    /// covers the one it did not — a server holding the connection, where the
    /// exit would otherwise track the server's patience rather than the user's.
    pub async fn canceled(&self) {
        loop {
            // Register BEFORE checking the flag. The other way round loses a
            // notification arriving between the check and the registration.
            let pending = self.notify.notified();
            if self.is_canceled() {
                return;
            }
            pending.await;
        }
    }

    /// Sleeps, or returns early on cancellation. Returns `true` if canceled.
    pub async fn sleep_or_cancel(&self, duration: Duration) -> bool {
        if self.is_canceled() {
            return true;
        }
        tokio::select! {
            biased;
            () = self.canceled() => true,
            _ = tokio::time::sleep(duration) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_lands_inside_the_range() {
        for _ in 0..200 {
            let d = jitter((500, 2_000)).as_millis() as u64;
            assert!((500..=2_000).contains(&d), "{d} is out of range");
        }
    }

    #[test]
    fn a_degenerate_range_does_not_panic() {
        // fastrand panics on an empty range, and the test pace has zeroes.
        assert_eq!(jitter((0, 0)), Duration::ZERO);
        assert_eq!(jitter((100, 100)), Duration::from_millis(100));
        assert_eq!(jitter((100, 50)), Duration::from_millis(100));
    }

    #[test]
    fn the_default_pace_is_the_documented_one() {
        assert_eq!(STEP_MS, (1_000, 3_000), "one to three seconds a page");
        assert_eq!(
            DWELL_MS,
            (1_500, 4_000),
            "a second and a half to four before a list"
        );
        assert_eq!(PAGES_PER_SITTING, 40, "about 480 accounts a sitting");
        assert_eq!(
            SITTING_PAUSE_MS,
            (300_000, 900_000),
            "five to fifteen minutes of rest"
        );

        let p = Pace::default();
        assert_eq!(p.per_page, 12);
        assert_eq!(p.step_ms, STEP_MS);
        assert_eq!(p.dwell_ms, DWELL_MS);
        assert_eq!(p.sitting_pause_ms, SITTING_PAUSE_MS);
        assert_eq!(p.pages_per_sitting, PAGES_PER_SITTING);
        assert_eq!(p.network_retries, 3);
        assert_eq!(p.backoff_base_ms, 2_000);
    }

    /// Somebody else's lists are read at the same cadence, and a network
    /// failure on them is insisted on less.
    #[test]
    fn the_third_party_pace_differs_only_in_its_retries() {
        let own = Pace::default();
        let other = Pace::third_party();

        assert_eq!(other.per_page, own.per_page);
        assert_eq!(other.step_ms, own.step_ms);
        assert_eq!(other.dwell_ms, own.dwell_ms);
        assert_eq!(other.sitting_pause_ms, own.sitting_pause_ms);
        assert_eq!(other.pages_per_sitting, own.pages_per_sitting);

        assert_eq!(other.network_retries, 2);
        assert_eq!(other.backoff_base_ms, 3_000);
    }

    #[test]
    fn the_waits_land_inside_their_ranges() {
        let p = Pace::default();
        for _ in 0..200 {
            let step = p.step().as_millis() as u64;
            assert!((1_000..=3_000).contains(&step), "{step}");
            let dwell = p.dwell().as_millis() as u64;
            assert!((1_500..=4_000).contains(&dwell), "{dwell}");
            let rest = p.sitting_pause().as_millis() as u64;
            assert!((300_000..=900_000).contains(&rest), "{rest}");
        }
    }

    /// A sitting is counted across walks, fills at its fortieth page, stays
    /// full until it is rested, and starts over after.
    #[test]
    fn a_sitting_fills_at_its_fortieth_page_and_starts_over_after_its_rest() {
        let pacer = Pacer::unlimited();
        let pace = Pace::default();
        for page in 1..PAGES_PER_SITTING {
            assert!(!pacer.page_walked(&pace), "page {page} is mid-sitting");
        }
        assert!(pacer.page_walked(&pace), "the fortieth page fills it");
        assert!(pacer.page_walked(&pace), "and it stays full until rested");

        pacer.sitting_rested();
        assert!(!pacer.page_walked(&pace), "a rested sitting starts over");
    }

    /// An action is counted only when one begins: a request spent is not one.
    #[tokio::test]
    async fn actions_are_counted_apart_from_requests() {
        let pacer = Pacer::unlimited();
        assert_eq!(pacer.actions(), 0);
        pacer.clear_to_send().await.unwrap();
        pacer.clear_to_send().await.unwrap();
        assert_eq!(pacer.actions(), 0, "requests begin no action");

        pacer.begin_action();
        assert_eq!(pacer.actions(), 1);
        assert_eq!(pacer.spent(), 2, "and an action spends nothing");
    }

    #[test]
    fn the_backoff_grows_and_does_not_overflow() {
        let p = Pace::default();
        assert_eq!(p.backoff(0), Duration::from_millis(2_000));
        assert_eq!(p.backoff(1), Duration::from_millis(4_000));
        assert_eq!(p.backoff(2), Duration::from_millis(8_000));
        // Even given an absurd number.
        assert!(p.backoff(99) <= Duration::from_millis(2_000 * 64));
    }

    #[tokio::test]
    async fn canceling_interrupts_a_long_wait() {
        let c = CancelToken::default();
        let copy = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            copy.cancel();
        });

        let start = std::time::Instant::now();
        let canceled = c.sleep_or_cancel(Duration::from_secs(30)).await;
        assert!(canceled);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the wait was not interrupted"
        );
    }

    #[tokio::test]
    async fn without_cancellation_the_wait_finishes_on_its_own() {
        let c = CancelToken::default();
        assert!(!c.sleep_or_cancel(Duration::from_millis(1)).await);
    }

    #[tokio::test]
    async fn canceling_before_waiting_is_still_noticed() {
        let c = CancelToken::default();
        c.cancel();
        assert!(c.sleep_or_cancel(Duration::from_secs(30)).await);
    }

    /// A canceled run is never cleared to send, even with nothing owed.
    ///
    /// The ordinary case is a budget that owes nothing, so a token read only
    /// inside the wait would let the request go out.
    #[tokio::test]
    async fn a_canceled_run_is_not_cleared_to_send() {
        let pacer = Pacer::unlimited();
        pacer
            .clear_to_send()
            .await
            .expect("nothing is canceled yet");
        assert_eq!(pacer.spent(), 1);

        pacer.cancel_token().cancel();

        let error = pacer.clear_to_send().await.unwrap_err();
        assert!(matches!(error, crate::error::IgError::Canceled));
        assert_eq!(
            pacer.spent(),
            1,
            "a request that is refused is not charged for"
        );
    }

    use snob_core::budget::RateBudgetError;

    /// A budget that is in cooldown and counts every reservation it is asked
    /// for, so a test can assert that it was asked for none.
    struct Cooling {
        until_ms: EpochMs,
        reserved: AtomicU32,
    }

    impl RateBudget for Cooling {
        fn reserve(&self) -> Result<Duration, RateBudgetError> {
            self.reserved.fetch_add(1, Ordering::Relaxed);
            Ok(Duration::ZERO)
        }

        fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
            self.reserved.fetch_add(1, Ordering::Relaxed);
            Ok(Duration::ZERO)
        }

        fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
            Ok(Some(self.until_ms))
        }

        fn start_cooldown(
            &self,
            _reason: &str,
            _minimum: Duration,
        ) -> Result<EpochMs, RateBudgetError> {
            Ok(self.until_ms)
        }
    }

    /// Nothing is spent during a cooldown, whoever asks and whether or not they
    /// remembered to check first.
    ///
    /// This is the backstop rather than the gates: every caller in `snob-cli`
    /// asks `Pacer::cooldown` before it gets here, and one of them — the command
    /// written to be polled by a monitoring system — did not, and spent a
    /// request per configured account per poll against a door that was shut. The
    /// budget charges its buckets without ever reading the `cooldowns` table, so
    /// until `clear` read it the rule lived in eight places and held in seven.
    #[tokio::test]
    async fn nothing_is_cleared_to_send_during_a_cooldown() {
        let until_ms = EpochMs::new(1_722_700_000_000);
        let budget = Arc::new(Cooling {
            until_ms,
            reserved: AtomicU32::new(0),
        });
        let pacer = Pacer::new(Arc::clone(&budget) as Arc<dyn RateBudget>);

        let error = pacer.clear_to_send().await.unwrap_err();
        assert!(
            matches!(error, crate::error::IgError::InCooldown { until_ms: u } if u == until_ms),
            "a read during a cooldown answers InCooldown, carrying when it lifts: {error:?}"
        );

        // A write pays out of a second bucket, so it gets its own arm here: a
        // backstop that covered only reads would leave the one request class
        // that earns the twelve-hour cooldown uncovered.
        let error = pacer.clear_to_send_write().await.unwrap_err();
        assert!(matches!(
            error,
            crate::error::IgError::InCooldown { until_ms: u } if u == until_ms
        ));

        assert_eq!(
            budget.reserved.load(Ordering::Relaxed),
            0,
            "the refusal comes before the reservation, so neither bucket was charged"
        );
        assert_eq!(pacer.spent(), 0, "and nothing refused is reported as spent");
    }
}
