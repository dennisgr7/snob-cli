//! Paginated walk over a followers or following list.
//!
//! The loop the project we took inspiration from is missing: theirs does
//! `catch { continue }`, which skips every wait and repeats the request at
//! once, so throttling turns it into a burst against Instagram. Here every stop
//! condition is explicit.

use std::collections::HashSet;
use std::error::Error;
use std::time::Duration;

use snob_core::model::StopReason;
use snob_core::{EpochMs, Pk};

use crate::client::{Direction, IgClient};
use crate::error::{IgError, Reaction};
use crate::model::FriendshipsPage;
use crate::pace::{CancelToken, Pace};

/// Hard page ceiling. No failure should ever be able to produce an endless
/// stream of requests, whatever happens to the other stop conditions.
pub const HARD_PAGE_CAP: u32 = 2_000;

/// Past this **declared** size, falling short smells like Instagram truncating
/// the list rather than the counter lying.
///
/// It is a guess based on unverified reports. That is why it is a named
/// constant and why both numbers are logged: so real data can correct it.
///
/// It is compared against what Instagram declared, not against what was walked.
/// The other way round was a hole: a walk that stopped at six thousand of a
/// declared forty thousand fell under the threshold on the walked count and was
/// reported as a complete list, which is precisely the case the threshold
/// exists for.
pub(crate) const TRUNCATION_THRESHOLD: usize = 10_000;

/// How many pages in a row without a single new account are tolerated before
/// assuming the list is going round in circles.
const MAX_PAGES_WITHOUT_NEW: u32 = 3;

/// The share of a walk's rows, in percent, that may be accounts the same walk
/// was already served before the list is not believed complete.
///
/// Instagram's lists serve some accounts twice across pages: the web app asks
/// for twelve and gets eight to twelve, some of them seen on an earlier page. A
/// list where more than one row in twenty is a repeat has room for accounts
/// that were never served at all in the places the repeats took, and a list
/// missing accounts crossed against another names people who never left. So
/// such a walk is read as truncated, however cleanly it ended. The five is the
/// owner's decision, to be checked against what a real list serves; there is no
/// minimum sample, so one repeat in a short list counts.
pub(crate) const REPEAT_LIMIT_PERCENT: usize = 5;

/// The waits a walk pays. The request budget's own wait is not among them: it
/// is imposed inside the client, announced by the [`crate::pace::Pacer`], and
/// the walker never learns of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitKind {
    /// Before the walk's first page, as a person looks at a profile before
    /// opening its list: [`Pace::dwell`].
    Dwell,
    /// Between two pages of the walk: [`Pace::step`].
    Step,
    /// The sitting is full: the rest of [`Pace::sitting_pause`], unless the
    /// walk ends on that page.
    Sitting,
    /// The day's accounts ran out: the walk sleeps until the last 24 hours
    /// have room for `PAGES_AFTER_A_PAUSE` more pages, then carries on.
    Day,
}

/// How many pages the day must have room for before a walk paused on its
/// accounts wakes up. One would wake it for every page as the day frees up,
/// with a pause and a line on screen between each.
pub(crate) const PAGES_AFTER_A_PAUSE: u32 = 8;

/// How often a walk asleep on the day's accounts tells its caller it is still
/// there. Well inside the claim's lifetime in `snob-store`'s `snapshots`, so a
/// sleeping walk is never mistaken for a dead one.
const HEARTBEAT: Duration = Duration::from_secs(10 * 60);

/// What a walk asleep on the day's accounts calls every [`HEARTBEAT`]:
/// `false` means whoever the pages are saved for no longer wants them (another
/// process took the capture over), and the walk stops.
pub type Heartbeat<'a> = &'a dyn Fn() -> Result<bool, Box<dyn Error + Send + Sync>>;

/// What happens as the walk proceeds.
///
/// An enum and an `FnMut` rather than a trait: this crate depends on no
/// presentation library, and tests can spy on the exact sequence with a `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started {
        estimated: Option<u64>,
        resumed: bool,
    },
    Page {
        number: u32,
        received: usize,
        added: usize,
        running_total: usize,
    },
    Waiting {
        kind: WaitKind,
        duration: Duration,
    },
    Retrying {
        attempt: u32,
        after: Duration,
        error: String,
    },
    Warning(Warning),
    Finished {
        pages: u32,
        users: usize,
        reason: StopReason,
    },
}

/// Something the walk noticed and the caller ought to say out loud.
///
/// A variant per condition rather than a sentence, because the sentence is not
/// this crate's to write: `snob-ig` reports what it saw and `snob-cli`'s
/// `report::pager_warning` decides the words, so the wording of the walk's
/// most alarming lines does not live inside the HTTP client.
///
/// Six of the seven end the walk as [`StopReason::Truncated`]; they are separate
/// variants because what a reader has to understand differs by guard, and
/// because a caller may want to recognize one of them without reading a
/// sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warning {
    /// The cursor came back the same as the one just sent, so following it
    /// would ask for the page that has already been served.
    SameCursorTwice,
    /// Two pages in a row carried nobody.
    TwoEmptyPages,
    /// [`MAX_PAGES_WITHOUT_NEW`] pages in a row carried only accounts that had
    /// already been walked.
    GoingInCircles,
    /// One empty page, and no counter to tell an account with no followers
    /// from an account Instagram would not serve.
    EmptyAndNoCounter,
    /// The walk ended cleanly far short of what the profile declared, and
    /// [`truncated`] read that as Instagram having stopped serving pages.
    StoppedShort { walked: usize, declared: usize },
    /// The same shortfall, read the other way: a counter that includes
    /// accounts which no longer appear in the list.
    ShortOfDeclared { walked: usize, declared: usize },
    /// More than [`REPEAT_LIMIT_PERCENT`] of the rows the walk received were
    /// accounts it had already received.
    Repeated { repeated: usize, received: usize },
}

/// What a walk does when the day's accounts run out before the list does.
///
/// A choice rather than a rule because both answers cost something the person
/// running it is better placed to weigh: pausing spreads a large list over two
/// days and a list read over two days describes a longer stretch of time;
/// carrying on reads more accounts in one day than the ceiling in
/// `budget::accounts_per_day` was set at, which is what the account is judged
/// on. Pausing is the default because it is the one that cannot make things
/// worse with Instagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverBudget {
    /// Sleep before the page that would go over until the day has room, and
    /// carry on as the same walk: [`WaitKind::Day`].
    #[default]
    Pause,
    /// Read on. The pace still applies; the daily ceiling does not, so the
    /// day's reading is bounded only by the request budget's daily bucket,
    /// about two thousand pages.
    Continue,
}

/// What to walk.
#[derive(Debug, Clone)]
pub struct ListRequest<'a> {
    pub pk: Pk,
    /// Only used to name the page a browser would have called from. Empty is
    /// allowed and simply leaves the referer generic.
    pub username: &'a str,
    pub direction: Direction,
    /// Cursor to continue an interrupted walk from.
    pub from: Option<&'a str>,
    /// How many are expected, for progress. Never a stop condition.
    pub estimated: Option<u64>,
    /// Page cap requested by the caller.
    pub max_pages: Option<u32>,
    /// How many were already stored, when resuming.
    pub already_stored: usize,
    /// What to do when the day's accounts run out mid-walk.
    pub over_budget: OverBudget,
}

/// How the walk ended.
#[derive(Debug)]
pub struct WalkSummary {
    pub pages: u32,
    pub users: usize,
    pub reason: StopReason,
    /// Where it left off, if anything is left.
    pub pending_cursor: Option<String>,
    /// The error that cut the walk short, if any.
    pub error: Option<IgError>,
}

impl WalkSummary {
    pub fn is_complete(&self) -> bool {
        self.reason.yields_complete_list()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("the account is in cooldown for another {}", minutes(.remaining_ms))]
    Cooldown {
        until_ms: EpochMs,
        /// How long is left of it, in milliseconds. A length of time rather than
        /// a moment, which is why it stays a number.
        remaining_ms: i64,
    },
    #[error(transparent)]
    Budget(IgError),
    #[error("could not save the page: {0}")]
    Save(Box<dyn Error + Send + Sync>),
}

/// "1 minute" / "45 minutes"; a remainder under a minute still says 1.
fn minutes(remaining_ms: &i64) -> String {
    match (remaining_ms / 60_000).max(1) {
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

/// How many accounts the next page will carry, to ask the day's budget
/// whether it has room for them: at most what was asked for, on either list
/// (see [`crate::pace::ACCOUNTS_PER_PAGE`]).
fn expected_on_a_page(pace: &Pace) -> u32 {
    pace.per_page
}

pub struct ListWalker<'a> {
    client: &'a IgClient,
    pace: Pace,
    sleeps: bool,
    heartbeat: Option<Heartbeat<'a>>,
    /// The wall clock the day's wait is measured against: always
    /// [`snob_core::clock::now_ms`] outside the tests, which put a clock that
    /// jumps here to stand in for a machine that slept.
    wall: fn() -> EpochMs,
}

impl<'a> ListWalker<'a> {
    /// Rate control is not a parameter: it is inside the client, which cannot
    /// be built without it, so walking without it cannot be written.
    ///
    /// **The client also decides whether the waits are real**, by the server it
    /// is pointed at, not the caller: a walk against Instagram with no waits
    /// between pages is not something any caller can ask for. A mock server is
    /// not Instagram and Instagram is not a mock server.
    pub fn new(client: &'a IgClient) -> Self {
        Self {
            client,
            pace: Pace::default(),
            sleeps: client.is_live(),
            heartbeat: None,
            wall: snob_core::clock::now_ms,
        }
    }

    /// Sets who is told the walk is still there while it sleeps on the day's
    /// accounts. See [`Heartbeat`].
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Heartbeat<'a>) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    #[must_use]
    pub fn with_pace(mut self, pace: Pace) -> Self {
        self.pace = pace;
        self
    }

    /// The client's own token, so the walk stops on the same Ctrl+C that
    /// stops its requests.
    fn cancel(&self) -> &CancelToken {
        self.client.pacer().cancel_token()
    }

    /// Walks the whole list, page by page.
    ///
    /// `save` receives each page as soon as it arrives and **before any wait**,
    /// so the caller can persist it immediately; it returns how many users were
    /// genuinely new.
    pub async fn walk<S, O>(
        &self,
        request: ListRequest<'_>,
        mut save: S,
        mut observe: O,
    ) -> Result<WalkSummary, WalkError>
    where
        S: FnMut(&FriendshipsPage, u32) -> Result<usize, Box<dyn Error + Send + Sync>>,
        O: FnMut(Event),
    {
        self.check_cooldown().await?;
        // A walk is an action, and so is each sitting after its rest below.
        self.client.pacer().begin_action();

        observe(Event::Started {
            estimated: request.estimated,
            resumed: request.from.is_some(),
        });

        let mut state = WalkState::new(&request);
        // Whether the list has been opened: once a walk, before its first
        // page, whatever the loop starts over for before it.
        let mut opened = false;

        let reason = loop {
            if self.cancel().is_canceled() {
                break StopReason::Canceled;
            }

            // Asked again on every page, not only before the first.
            //
            // `check_cooldown` above answers for the moment the walk started,
            // and this loop is the longest unbroken run of requests the tool
            // produces. A cooldown another process writes while this walk is in
            // flight has to reach it — `snob whoami` in a second terminal
            // drawing a 429 stops *that* process and must not leave this one
            // paging into a door Instagram has just closed. The database is
            // shared per user, which makes that ordinary rather than a corner
            // case.
            //
            // It does not self-limit either, unless the push-back happens to
            // cover `/api/v1/friendships/` as well: an endpoint-specific
            // throttle leaves this walk answering normally to the end.
            //
            // **`Pacer::clear` reads the table too**, and this is still worth
            // its place: the two stop the walk by different routes.
            //
            // `break` rather than `Err`, so the partial keeps its cursor and
            // stays resumable — `verify_completion` passes a non-`Completed`
            // reason through untouched. The backstop answers `Err`, which lands
            // in `stop_reason_for` and also breaks with `RateLimit` carrying
            // `state.cursor`, so nothing is lost either way; what this buys is
            // catching the cooldown *between* pages, before the next request has
            // been reserved at all. One local SQLite read a page.
            if self
                .client
                .pacer()
                .cooldown_off_thread()
                .await
                .map_err(WalkError::Budget)?
                .is_some()
            {
                break StopReason::RateLimit;
            }

            if let Some(end) = state.cap_reached(&request) {
                break end;
            }

            // The day's accounts, asked before the page rather than found out
            // after it: a page that would go over is not asked for until the
            // day has room for a run of them. The sleep is inside the walk, so
            // every guard here keeps counting across it, and the loop starts
            // over afterwards to ask the cancel, the cooldown and the caps
            // again. Against a test server there is no day to spend, for the
            // same reason there are no waits.
            if self.sleeps && request.over_budget == OverBudget::Pause {
                let page = expected_on_a_page(&self.pace);
                let pacer = self.client.pacer();
                if !pacer
                    .accounts_wait(page)
                    .await
                    .map_err(WalkError::Budget)?
                    .is_zero()
                {
                    let wait = pacer
                        .accounts_wait(page * PAGES_AFTER_A_PAUSE)
                        .await
                        .map_err(WalkError::Budget)?;
                    if self.sleep_through_the_day(wait, &mut observe).await? {
                        break StopReason::Canceled;
                    }
                    continue;
                }
            }

            // **The list is opened as a person opens it**, before its first
            // page: the profile's navigation goes out as the app's router
            // sends it (`IgClient::open_list`), and then the walk waits as
            // long as a person looks at a profile before clicking its list
            // ([`Pace::dwell`]). Every walk does, so the second list of a
            // crossing is opened, and waited for, as the second click is.
            if !opened {
                opened = true;
                if let Err(e) = self.client.open_list(request.username, false).await {
                    break self.stop_reason_for(e, &mut state);
                }
                if self
                    .wait(WaitKind::Dwell, self.pace.dwell(), &mut observe)
                    .await
                {
                    break StopReason::Canceled;
                }
            }

            // A step before every page but the walk's first, which is the
            // action starting. The budget's own wait is not paid here. It
            // happens inside the client, after this one, so an exhausted
            // budget makes the walk slower than the step suggests — the safe
            // direction, and only once it is already rationing.
            if state.pages > 0
                && self
                    .wait(WaitKind::Step, self.pace.step(), &mut observe)
                    .await
            {
                break StopReason::Canceled;
            }

            let page = match self.fetch_page(&request, &state, &mut observe).await {
                Ok(p) => p,
                Err(e) => break self.stop_reason_for(e, &mut state),
            };
            let received = page.users.len();
            let added = save(&page, state.pages + 1).map_err(WalkError::Save)?;
            // Counted whether or not the walk goes on: the sitting is the
            // process's, and the next walk carries it on.
            let sitting_full = self.client.pacer().page_walked(&self.pace);

            state.pages += 1;
            state.users += added;
            observe(Event::Page {
                number: state.pages,
                received,
                added,
                running_total: state.users,
            });

            if let Some(end) = state.record_page(received, added, &page, &mut observe) {
                break end;
            }

            // The rest is skipped when the walk ends on this page, by its own
            // end or its cap: a rest with nothing after it is a quarter of an
            // hour of waiting for nobody. The sitting stays full, and the next
            // page anybody walks is rested after.
            if sitting_full && state.cap_reached(&request).is_none() {
                if self
                    .wait(WaitKind::Sitting, self.pace.sitting_pause(), &mut observe)
                    .await
                {
                    break StopReason::Canceled;
                }
                self.client.pacer().sitting_rested();
                self.client.pacer().begin_action();
            }
        };

        // The rate the repeat rule judges, written down whatever it found and
        // however the walk ended, so that a rate under the threshold is
        // measured too. Counts only.
        tracing::debug!(
            list = ?request.direction,
            pages = state.pages,
            received = state.received,
            repeated = state.repeated,
            "the rows a walk received, and how many it had received already"
        );
        let reason = state.verify_completion(reason, &request, &mut observe);

        observe(Event::Finished {
            pages: state.pages,
            users: state.users,
            reason,
        });

        Ok(WalkSummary {
            pages: state.pages,
            users: state.users,
            reason,
            pending_cursor: if reason.yields_complete_list() {
                None
            } else {
                state.cursor.clone()
            },
            error: state.error,
        })
    }

    /// Sleeps until the day has room, calling the heartbeat every
    /// [`HEARTBEAT`] so whoever saves the pages knows the walk is asleep
    /// rather than dead.
    ///
    /// `true` when the sleep was cut short: the person asked to stop, or the
    /// heartbeat said the walk is no longer wanted.
    ///
    /// **The wait ends at a moment on the wall clock, not after an amount of
    /// sleeping.** The day's budget is kept in wall-clock time, and the timers
    /// a sleep runs on are not: on Linux and macOS they stand still while the
    /// machine is suspended, and on Windows a relative wait does not count a
    /// low-power state either. Counted in sleeps, a laptop shut for the night
    /// halfway through a six-hour wait would wake to six more hours of it,
    /// for room the day made long ago. Each heartbeat looks at the wall clock
    /// again, so the wait is over at the first one after the moment passed.
    ///
    /// The sleeping is still counted, as the other bound: a wall clock set
    /// back by hand would otherwise move the moment away, and the wait is
    /// never longer than the budget asked for.
    async fn sleep_through_the_day<O: FnMut(Event)>(
        &self,
        wait: Duration,
        observe: &mut O,
    ) -> Result<bool, WalkError> {
        observe(Event::Waiting {
            kind: WaitKind::Day,
            duration: wait,
        });
        let until = (self.wall)() + wait;
        let mut slept = Duration::ZERO;
        while slept < wait {
            let on_the_wall = until - (self.wall)();
            let Ok(on_the_wall) = u64::try_from(on_the_wall) else {
                break;
            };
            if on_the_wall == 0 {
                break;
            }
            let step = Duration::from_millis(on_the_wall)
                .min(wait - slept)
                .min(HEARTBEAT);
            if self.cancel().sleep_or_cancel(step).await {
                return Ok(true);
            }
            if let Some(still_wanted) = self.heartbeat
                && !still_wanted().map_err(WalkError::Save)?
            {
                return Ok(true);
            }
            slept += step;
        }
        Ok(false)
    }

    async fn check_cooldown(&self) -> Result<(), WalkError> {
        let until = self
            .client
            .pacer()
            .cooldown_off_thread()
            .await
            .map_err(WalkError::Budget)?;
        if let Some(until_ms) = until {
            let remaining_ms = until_ms - snob_core::clock::now_ms();
            return Err(WalkError::Cooldown {
                until_ms,
                remaining_ms,
            });
        }
        Ok(())
    }

    /// Fetches a page, retrying only what is worth retrying.
    async fn fetch_page<O: FnMut(Event)>(
        &self,
        request: &ListRequest<'_>,
        state: &WalkState,
        observe: &mut O,
    ) -> Result<FriendshipsPage, IgError> {
        let mut attempt = 0;
        loop {
            let result = self
                .client
                .friendships_page(
                    request.pk,
                    request.username,
                    request.direction,
                    self.pace.per_page,
                    state.cursor.as_deref(),
                )
                .await;

            let error = match result {
                Ok(p) => return Ok(p),
                Err(e) => e,
            };

            // `reaction()` rather than the individual predicates: it is the
            // single authority here, and it is what guarantees a 429 is never
            // retried.
            if error.reaction() != Reaction::Retry || attempt >= self.pace.network_retries {
                return Err(error);
            }

            let after = self.pace.backoff(attempt);
            observe(Event::Retrying {
                attempt: attempt + 1,
                after,
                error: error.to_string(),
            });
            // `Canceled`, not the server's error: a 503 on page nine plus
            // Ctrl+C during the backoff is the user stopping, not the server
            // failing, so it exits 130 rather than 1 with
            // `stopped_by = 'network'`, the same as an interrupt a moment later.
            if self.sleeps && self.cancel().sleep_or_cancel(after).await {
                return Err(IgError::Canceled);
            }
            attempt += 1;
        }
    }

    fn stop_reason_for(&self, error: IgError, state: &mut WalkState) -> StopReason {
        let reason = match error.reaction() {
            Reaction::Cooldown => StopReason::RateLimit,
            Reaction::Retry => StopReason::Network,
            Reaction::Abort => match error {
                IgError::SessionExpired
                | IgError::UserAgentMismatch
                | IgError::Challenge { .. }
                | IgError::Checkpoint { .. } => StopReason::SessionInvalid,
                // Ctrl+C during the budget's wait arrives as an error from the
                // client rather than through the token, and it is still the
                // user stopping rather than anything going wrong.
                IgError::Canceled => StopReason::Canceled,
                // The backstop in `Pacer::clear` refusing, which is throttling
                // and not a network failure however it arrives. Its reaction is
                // `Abort` — retrying a wait measured in hours is the one thing
                // that must not happen — so without this arm it would fall to
                // `Network`. The mid-walk check in `walk` breaks with
                // `RateLimit` for the identical condition, and
                // `exit::from_stop_reason` and `from_ig_error` are
                // documented to agree.
                IgError::InCooldown { .. } => StopReason::RateLimit,
                _ => StopReason::Network,
            },
        };
        state.error = Some(error);
        reason
    }

    /// Returns `true` if it was canceled while waiting.
    async fn wait<O: FnMut(Event)>(
        &self,
        kind: WaitKind,
        duration: Duration,
        observe: &mut O,
    ) -> bool {
        if duration.is_zero() {
            return false;
        }
        observe(Event::Waiting { kind, duration });
        if !self.sleeps {
            return self.cancel().is_canceled();
        }
        self.cancel().sleep_or_cancel(duration).await
    }
}

/// Mutable state of the walk, kept apart so the stop conditions are functions
/// rather than branches scattered through the loop.
struct WalkState {
    cursor: Option<String>,
    /// The cursor of the previous page, to catch one that does not advance.
    last_cursor: Option<String>,
    pages: u32,
    users: usize,
    empty_in_a_row: u32,
    barren_in_a_row: u32,
    /// Every account this walk has received, to tell a repeat from a new
    /// row. A resumed walk starts it empty: rows an earlier run stored are
    /// not this walk's, and serving them again is not a repeat.
    seen: HashSet<Pk>,
    /// Every row this walk received, repeats included.
    received: usize,
    /// The rows that named an account this walk had already received.
    repeated: usize,
    error: Option<IgError>,
}

impl WalkState {
    fn new(request: &ListRequest<'_>) -> Self {
        Self {
            cursor: request.from.map(str::to_string),
            last_cursor: None,
            pages: 0,
            users: request.already_stored,
            empty_in_a_row: 0,
            barren_in_a_row: 0,
            seen: HashSet::new(),
            received: 0,
            repeated: 0,
            error: None,
        }
    }

    fn cap_reached(&self, request: &ListRequest<'_>) -> Option<StopReason> {
        if let Some(max) = request.max_pages
            && self.pages >= max
        {
            return Some(StopReason::PageLimit);
        }
        if self.pages >= HARD_PAGE_CAP {
            return Some(StopReason::Truncated);
        }
        None
    }

    fn record_page<O: FnMut(Event)>(
        &mut self,
        received: usize,
        added: usize,
        page: &FriendshipsPage,
        observe: &mut O,
    ) -> Option<StopReason> {
        for user in &page.users {
            self.received += 1;
            if !self.seen.insert(user.pk) {
                self.repeated += 1;
            }
        }

        let next = page.next_cursor().map(str::to_string);

        // No cursor means the list is done.
        let Some(next) = next else {
            return Some(StopReason::Completed);
        };

        // A cursor that does not advance is a loop. This is the guard the
        // original project lacks.
        //
        // Compared against the one just **sent**, not only against the previous
        // page's: on the first page of a resumed walk there is no previous one,
        // so a cursor that comes straight back unchanged would have gone
        // undetected for one more request — and a walk that ends this way keeps
        // that cursor and stays resumable for the resume window, so every run
        // in that window paid the same two requests to rediscover it.
        if self.last_cursor.as_deref() == Some(next.as_str())
            || self.cursor.as_deref() == Some(next.as_str())
        {
            observe(Event::Warning(Warning::SameCursorTwice));
            return Some(StopReason::Truncated);
        }

        if received == 0 {
            self.empty_in_a_row += 1;
            if self.empty_in_a_row >= 2 {
                observe(Event::Warning(Warning::TwoEmptyPages));
                return Some(StopReason::Truncated);
            }
        } else {
            self.empty_in_a_row = 0;
        }

        if added == 0 {
            self.barren_in_a_row += 1;
            if self.barren_in_a_row >= MAX_PAGES_WITHOUT_NEW {
                observe(Event::Warning(Warning::GoingInCircles));
                return Some(StopReason::Truncated);
            }
        } else {
            self.barren_in_a_row = 0;
        }

        self.last_cursor = Some(next.clone());
        self.cursor = Some(next);
        None
    }

    /// Last filter: a clean but very short ending, measured against what was
    /// declared, is probably not clean at all.
    fn verify_completion<O: FnMut(Event)>(
        &self,
        reason: StopReason,
        request: &ListRequest<'_>,
        observe: &mut O,
    ) -> StopReason {
        if reason != StopReason::Completed {
            return reason;
        }

        // Served twice too often to trust that everything was served once.
        if self.repeated * 100 > self.received * REPEAT_LIMIT_PERCENT {
            observe(Event::Warning(Warning::Repeated {
                repeated: self.repeated,
                received: self.received,
            }));
            return StopReason::Truncated;
        }

        // Nothing walked, and nothing to check that against.
        //
        // An account with no followers and an Instagram that served no
        // followers look identical from here: one page, no users, no cursor,
        // which `record_page` reads as the list being done. The counter is
        // what tells them apart, and this is the branch where the counter is
        // missing — the profile poll failed, which is the same bad afternoon
        // that produces the empty page.
        //
        // Getting it wrong here is not a partial result but a wrong one: an
        // empty followers list accepted as complete makes every account you
        // follow an unfollower. A real empty account loses nothing by being
        // asked again, so this refuses.
        if request.estimated.is_none() && self.users == 0 {
            observe(Event::Warning(Warning::EmptyAndNoCounter));
            return StopReason::Truncated;
        }

        let Some(estimated) = request.estimated else {
            return reason;
        };

        let estimated = estimated as usize;
        if self.users >= estimated {
            return reason;
        }

        // A shortfall of more than a tenth is more than deleted accounts
        // usually account for, and is what makes either explanation worth
        // weighing at all.
        let far_short = estimated > self.users * 11 / 10;
        if !far_short {
            return reason;
        }

        if truncated(self.users, estimated) {
            observe(Event::Warning(Warning::StoppedShort {
                walked: self.users,
                declared: estimated,
            }));
            return StopReason::Truncated;
        }

        observe(Event::Warning(Warning::ShortOfDeclared {
            walked: self.users,
            declared: estimated,
        }));
        reason
    }
}

/// Which of the two explanations for a short list to believe.
///
/// A walk that ends cleanly with fewer accounts than declared is either the
/// counter lying — it includes deleted and deactivated accounts that no longer
/// appear — or Instagram having stopped serving pages. Getting it wrong in one
/// direction disables comparison for every account whose counter overstates;
/// getting it wrong in the other reports departures that never happened, which
/// is the failure this whole tool is built to avoid.
///
/// Two independent signals, either of which is enough:
///
/// - **The declared size is past the threshold.** That is where Instagram is
///   reported to stop paginating, so any real shortfall there is suspect. It is
///   measured against what was *declared*, not against what was walked: reading
///   six thousand of a declared forty thousand is exactly the case this is for,
///   and testing the walked count would have called it complete.
/// - **The shortfall is enormous whatever the size.** Deleted accounts are not
///   half of anybody's followers. A hundred out of three thousand is not a
///   counter that overstates, it is a list that stopped.
fn truncated(walked: usize, declared: usize) -> bool {
    declared >= TRUNCATION_THRESHOLD || walked * 2 < declared
}

#[cfg(test)]
mod tests {
    use snob_core::session::{Session, SessionOrigin};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{SID, UA, client_with};
    use crate::pace::Pacer;

    #[test]
    fn the_cooldown_display_pluralizes_the_minutes() {
        let one = WalkError::Cooldown {
            until_ms: EpochMs::new(0),
            remaining_ms: 30_000,
        };
        assert_eq!(
            one.to_string(),
            "the account is in cooldown for another 1 minute"
        );

        let four = WalkError::Cooldown {
            until_ms: EpochMs::new(0),
            remaining_ms: 240_000,
        };
        assert_eq!(
            four.to_string(),
            "the account is in cooldown for another 4 minutes"
        );
    }

    fn client(server: &MockServer) -> IgClient {
        client_with(server, Pacer::unlimited())
    }

    /// A walk against Instagram pays every wait; one against a test server pays
    /// none, and neither is a choice a caller gets to make.
    ///
    /// The assertion behind AGENTS.md's "never walk a real account's lists
    /// without the limiter".
    #[tokio::test]
    async fn only_a_walk_against_a_test_server_skips_the_waits() {
        let server = MockServer::start().await;
        assert!(
            !ListWalker::new(&client(&server)).sleeps,
            "a mock server is not Instagram, so there is nothing to be polite to"
        );

        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let live = IgClient::new(session, Pacer::unlimited()).unwrap();
        assert!(
            ListWalker::new(&live).sleeps,
            "a walk against Instagram has to pay its waits"
        );
    }

    /// A body with `n` users and, optionally, a cursor to the next page.
    fn body(from: u64, n: u64, cursor: Option<&str>) -> String {
        let users: Vec<String> = (from..from + n)
            .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
            .collect();
        let cursor = match cursor {
            Some(c) => format!(r#","next_max_id":"{c}""#),
            None => String::new(),
        };
        format!(r#"{{"users":[{}]{cursor}}}"#, users.join(","))
    }

    fn request<'a>() -> ListRequest<'a> {
        ListRequest {
            pk: Pk::new(42),
            username: "someone",
            direction: Direction::Followers,
            from: None,
            estimated: None,
            max_pages: None,
            already_stored: 0,
            over_budget: OverBudget::Pause,
        }
    }

    /// Serves a scripted sequence of responses, one per request.
    async fn server(responses: Vec<ResponseTemplate>) -> MockServer {
        let server = MockServer::start().await;
        for (i, r) in responses.into_iter().enumerate() {
            Mock::given(method("GET"))
                .respond_with(r)
                .up_to_n_times(1)
                .with_priority(i as u8 + 1)
                .mount(&server)
                .await;
        }
        server
    }

    fn ok(body: String) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_string(body)
    }

    /// Walks collecting the events, without sleeping and with a free budget.
    async fn walk(
        server: &MockServer,
        request: ListRequest<'_>,
    ) -> (WalkSummary, Vec<Event>, Vec<Pk>) {
        let client = client(server);
        let walker = ListWalker::new(&client);

        let mut events = Vec::new();
        let mut seen: Vec<Pk> = Vec::new();

        let summary = walker
            .walk(
                request,
                |page, _| {
                    let before = seen.len();
                    for u in &page.users {
                        if !seen.contains(&u.pk) {
                            seen.push(u.pk);
                        }
                    }
                    Ok(seen.len() - before)
                },
                |e| events.push(e),
            )
            .await
            .unwrap();

        (summary, events, seen)
    }

    /// The one case where a clean ending is not a clean ending.
    ///
    /// One page, no users, no cursor: `record_page` reads that as the list
    /// being done, and it is also what Instagram serving nothing looks like.
    /// The declared counter is what tells the two apart, and the walk that
    /// hits this is the one whose profile poll already failed.
    ///
    /// Accepted as complete, the empty followers list makes every account you
    /// follow an unfollower — not a partial answer but a wrong one.
    #[tokio::test]
    async fn an_empty_list_with_no_counter_to_check_it_against_is_refused() {
        let server = server(vec![ok(body(0, 0, None))]).await;
        let (summary, events, _) = walk(&server, request()).await;

        assert_eq!(summary.users, 0);
        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(!summary.is_complete());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Warning(Warning::EmptyAndNoCounter))),
            "{events:?}"
        );
    }

    /// The same answer, with the counter agreeing that the account really has
    /// nobody. Nothing to be suspicious of, and refusing it would make an
    /// empty account permanently unusable.
    #[tokio::test]
    async fn an_empty_list_the_counter_confirms_is_complete() {
        let server = server(vec![ok(body(0, 0, None))]).await;
        let request = ListRequest {
            estimated: Some(0),
            ..request()
        };
        let (summary, _, _) = walk(&server, request).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(summary.is_complete());
    }

    #[tokio::test]
    async fn it_walks_until_the_cursor_runs_out() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 20, None)),
        ])
        .await;

        let (summary, _, seen) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(summary.is_complete());
        assert_eq!(summary.pages, 3);
        assert_eq!(summary.users, 120);
        assert_eq!(seen.len(), 120);
        assert_eq!(summary.pending_cursor, None);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    /// `n` pages of twelve with distinct accounts, the last without a
    /// cursor, numbered from `first`.
    fn pages(first: u64, n: u64) -> Vec<ResponseTemplate> {
        (first..first + n)
            .map(|i| {
                let cursor = (i + 1 < first + n).then(|| format!("c{i}"));
                ok(body(i * 12, 12, cursor.as_deref()))
            })
            .collect()
    }

    /// What a walk's waits and pages were, in order: `P` for a page, the
    /// wait's kind otherwise.
    fn cadence(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Page { number, .. } => Some(format!("P{number}")),
                Event::Waiting { kind, .. } => Some(format!("{kind:?}")),
                _ => None,
            })
            .collect()
    }

    /// Walks `request` with the production pace and no sleeps, collecting
    /// the events.
    async fn walk_at_pace(client: &IgClient, request: ListRequest<'_>) -> Vec<Event> {
        // PRODUCTION pace: the real policy is what is under test.
        let walker = ListWalker::new(client).with_pace(Pace::default());
        let mut events = Vec::new();
        walker
            .walk(request, |p, _| Ok(p.users.len()), |e| events.push(e))
            .await
            .unwrap();
        events
    }

    /// A step comes between two pages and nowhere else: not before the
    /// first, which is the action starting, and not after the last. Before
    /// the first comes the dwell, once, as a person looks at the profile
    /// before opening its list.
    #[tokio::test]
    async fn a_step_comes_between_pages() {
        let server = server(pages(0, 3)).await;
        let events = walk_at_pace(&client(&server), request()).await;

        assert_eq!(
            cadence(&events),
            ["Dwell", "P1", "Step", "P2", "Step", "P3"],
            "{events:?}"
        );
        for e in &events {
            if let Event::Waiting { kind, duration } = e {
                let ms = duration.as_millis() as u64;
                let range = match kind {
                    WaitKind::Dwell => 1_500..=4_000,
                    _ => 1_000..=3_000,
                };
                assert!(range.contains(&ms), "a {kind:?} of {ms} ms");
            }
        }
    }

    /// Every walk dwells before its first page, the second of two on one
    /// client too, as the second list of a crossing is opened by a second
    /// click; a zeroed dwell is no wait at all.
    #[tokio::test]
    async fn every_walk_dwells_before_its_first_page() {
        let server = server([pages(0, 2), pages(100, 2)].concat()).await;
        let client = client(&server);
        for _ in 0..2 {
            let cadence = cadence(&walk_at_pace(&client, request()).await);
            assert_eq!(cadence, ["Dwell", "P1", "Step", "P2"]);
        }

        let server = self::server(pages(0, 2)).await;
        let client = self::client(&server);
        let walker = ListWalker::new(&client).with_pace(zero_pace());
        let mut events = Vec::new();
        walker
            .walk(request(), |p, _| Ok(p.users.len()), |e| events.push(e))
            .await
            .unwrap();
        assert_eq!(cadence(&events), ["P1", "P2"]);
    }

    /// Pins the pacing policy down: the sitting's rest lands after the
    /// fortieth page and nowhere else, every wait is inside its documented
    /// range, and none is paid after the last page.
    #[tokio::test]
    async fn a_sitting_rests_after_its_fortieth_page() {
        let server = server(pages(0, 46)).await;
        let client = client(&server);
        let events = walk_at_pace(&client, request()).await;
        assert_eq!(
            client.pacer().actions(),
            2,
            "the walk begins an action, and so does the sitting after the rest"
        );
        let cadence = cadence(&events);

        let rests: Vec<usize> = cadence
            .iter()
            .enumerate()
            .filter(|(_, k)| *k == "Sitting")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rests.len(), 1, "one rest in 46 pages: {cadence:?}");
        assert_eq!(cadence[rests[0] - 1], "P40");
        assert_eq!(cadence[rests[0] + 1], "Step");
        assert_eq!(cadence[rests[0] + 2], "P41");

        for e in &events {
            let Event::Waiting { kind, duration } = e else {
                continue;
            };
            let ms = duration.as_millis() as u64;
            let range = match kind {
                WaitKind::Dwell => (1_500, 4_000),
                WaitKind::Step => (1_000, 3_000),
                WaitKind::Sitting => (300_000, 900_000),
                WaitKind::Day => panic!("a free budget never pauses for the day"),
            };
            assert!(
                (range.0..=range.1).contains(&ms),
                "{kind:?} of {ms} ms is outside {range:?}"
            );
        }
        assert_eq!(cadence.last().map(String::as_str), Some("P46"));
    }

    /// A sitting is the process's reading, not one walk's: two walks of
    /// twenty-five pages on one client meet one rest, fifteen pages into the
    /// second.
    #[tokio::test]
    async fn two_walks_on_one_client_share_a_sitting() {
        let server = server([pages(0, 25), pages(100, 25)].concat()).await;
        let client = client(&server);

        let first = cadence(&walk_at_pace(&client, request()).await);
        assert!(
            !first.contains(&"Sitting".to_string()),
            "twenty-five pages fill no sitting: {first:?}"
        );

        let second = cadence(&walk_at_pace(&client, request()).await);
        let rests: Vec<usize> = second
            .iter()
            .enumerate()
            .filter(|(_, k)| *k == "Sitting")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rests.len(), 1, "{second:?}");
        assert_eq!(second[rests[0] - 1], "P15");
        assert_eq!(server.received_requests().await.unwrap().len(), 50);
    }

    /// A walk that ends on a full sitting does not rest: there is nothing
    /// after the rest to wait for.
    #[tokio::test]
    async fn no_rest_follows_the_last_page() {
        let server = server(pages(0, 40)).await;
        let cadence = cadence(&walk_at_pace(&client(&server), request()).await);

        assert!(!cadence.contains(&"Sitting".to_string()), "{cadence:?}");
        assert_eq!(cadence.last().map(String::as_str), Some("P40"));
    }

    /// A walk that ends on a full sitting at its page cap does not rest
    /// either, though the list goes on: the cap ends the walk as surely as
    /// the list's last page does.
    #[tokio::test]
    async fn no_rest_follows_the_page_cap() {
        let server = server(pages(0, 46)).await;
        let client = client(&server);
        let request = ListRequest {
            max_pages: Some(40),
            ..request()
        };
        let walker = ListWalker::new(&client).with_pace(Pace::default());
        let mut events = Vec::new();
        let summary = walker
            .walk(request, |p, _| Ok(p.users.len()), |e| events.push(e))
            .await
            .unwrap();
        let cadence = cadence(&events);

        assert_eq!(summary.reason, StopReason::PageLimit);
        assert!(!cadence.contains(&"Sitting".to_string()), "{cadence:?}");
        assert_eq!(cadence.last().map(String::as_str), Some("P40"));
        assert_eq!(server.received_requests().await.unwrap().len(), 40);
    }

    /// The sitting a walk ended on stays full, and the next walk on the
    /// client rests after its first page.
    #[tokio::test]
    async fn a_full_sitting_carries_over_to_the_next_walk() {
        let server = server([pages(0, 40), pages(100, 3)].concat()).await;
        let client = client(&server);

        let first = cadence(&walk_at_pace(&client, request()).await);
        assert!(!first.contains(&"Sitting".to_string()), "{first:?}");
        assert_eq!(first.last().map(String::as_str), Some("P40"));

        let second = cadence(&walk_at_pace(&client, request()).await);
        assert_eq!(
            second,
            ["Dwell", "P1", "Sitting", "Step", "P2", "Step", "P3"],
            "the carried-over sitting is rested after the next page"
        );
    }

    #[tokio::test]
    async fn a_repeating_cursor_stops_the_loop() {
        // Instagram always returning the same cursor: without the guard this is
        // an endless burst of requests.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ok(body(0, 50, Some("always_the_same"))))
            .mount(&server)
            .await;

        let (summary, events, _) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert_eq!(summary.pages, 2, "it should stop as soon as it repeats");
        assert!(events.iter().any(|e| matches!(e, Event::Warning(_))));
    }

    #[tokio::test]
    async fn two_empty_pages_in_a_row_cut_it_short() {
        let server = server(vec![
            ok(body(0, 10, Some("c1"))),
            ok(body(0, 0, Some("c2"))),
            ok(body(0, 0, Some("c3"))),
        ])
        .await;

        let (summary, _, _) = walk(&server, request()).await;
        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(!summary.is_complete());
    }

    #[tokio::test]
    async fn throttling_stops_without_retrying() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ResponseTemplate::new(429).set_body_string(r#"{"message":"","spam":true}"#),
        ])
        .await;

        let (summary, _, _) = walk(&server, request()).await;

        assert_eq!(summary.reason, StopReason::RateLimit);
        assert!(!summary.is_complete());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "a 429 must not cause a fourth request"
        );
        assert_eq!(summary.pending_cursor.as_deref(), Some("c2"));
    }

    #[tokio::test]
    async fn an_expired_session_stops_and_says_so() {
        let server =
            server(vec![ResponseTemplate::new(403).set_body_string(
                r#"{"message":"login_required","status":"fail"}"#,
            )])
            .await;

        let (summary, _, _) = walk(&server, request()).await;
        assert_eq!(summary.reason, StopReason::SessionInvalid);
        assert!(matches!(summary.error, Some(IgError::SessionExpired)));
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_then_given_up_on() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("oops"))
            .mount(&server)
            .await;

        let (summary, events, _) = walk(&server, request()).await;

        let retries = events
            .iter()
            .filter(|e| matches!(e, Event::Retrying { .. }))
            .count();
        assert_eq!(retries, 3, "it should retry three times");
        assert_eq!(summary.reason, StopReason::Network);
    }

    /// Ctrl+C during a retry backoff is the user stopping, not the server
    /// failing.
    ///
    /// A 503 plus an interrupt during the backoff exits 130, not 1 with
    /// `stopped_by = 'network'`: the walk's error is `Canceled`.
    ///
    /// The waits have to be on for this: a walk against a test server does not
    /// wait at all, so there would be no backoff to race the token against.
    /// Everything else in the pace is set to zero so the only real wait is the
    /// one under test, and the cancellation is fired from the `Retrying` event,
    /// which is emitted immediately before it.
    ///
    /// The client reads the same token before the retry and refuses it with
    /// `Canceled` too, so the reason and the error alone cannot tell the race
    /// from the whole 30-second backoff slept through. The time can: the walk
    /// comes back well inside the backoff only when the backoff itself saw the
    /// token.
    #[tokio::test]
    async fn canceling_during_a_backoff_is_not_a_network_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("oops"))
            .mount(&server)
            .await;

        let client = client(&server);
        let cancel = client.pacer().cancel_token().clone();
        let pace = Pace {
            step_ms: (0, 0),
            dwell_ms: (0, 0),
            sitting_pause_ms: (0, 0),
            backoff_base_ms: 30_000,
            ..Pace::default()
        };
        let mut walker = ListWalker::new(&client).with_pace(pace);
        walker.sleeps = true;

        let started = std::time::Instant::now();
        let summary = walker
            .walk(
                request(),
                |p, _| Ok(p.users.len()),
                |event| {
                    if matches!(event, Event::Retrying { .. }) {
                        cancel.cancel();
                    }
                },
            )
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the backoff was slept through rather than cut short: {:?}",
            started.elapsed()
        );
        assert_eq!(summary.reason, StopReason::Canceled);
        assert!(
            matches!(summary.error, Some(IgError::Canceled)),
            "the server's error must not survive the user's interrupt: {:?}",
            summary.error
        );
    }

    #[tokio::test]
    async fn the_page_cap_stops_it_and_keeps_the_cursor() {
        // Distinct cursors per page: otherwise the repeating-cursor guard would
        // fire first.
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 50, Some("c3"))),
        ])
        .await;

        let mut r = request();
        r.max_pages = Some(2);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::PageLimit);
        assert_eq!(summary.pages, 2);
        assert_eq!(summary.pending_cursor.as_deref(), Some("c2"));
        assert!(
            !summary.is_complete(),
            "a walk cut short does not describe the whole list"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the cap has to save real requests"
        );
    }

    /// The walk's own check between pages is what stops it, with no error:
    /// the client's refusal of the next request would stop it too, but that
    /// arrives as `IgError::Canceled` in the summary.
    #[tokio::test]
    async fn canceling_stops_at_the_page_boundary() {
        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 50, Some("c3"))),
        ])
        .await;

        let client = client(&server);
        let cancel = client.pacer().cancel_token().clone();
        let walker = ListWalker::new(&client);

        let summary = walker
            .walk(
                request(),
                |p, number| {
                    if number == 2 {
                        cancel.cancel();
                    }
                    Ok(p.users.len())
                },
                |_| {},
            )
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Canceled);
        assert_eq!(summary.pages, 2);
        assert!(summary.pending_cursor.is_some());
        assert!(
            summary.error.is_none(),
            "the client stopped it, not the walk: {:?}",
            summary.error
        );
    }

    #[tokio::test]
    async fn resuming_starts_from_the_stored_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ok(body(0, 10, None)))
            .mount(&server)
            .await;

        let mut r = request();
        r.from = Some("from_here");
        r.already_stored = 100;
        let (summary, events, _) = walk(&server, r).await;

        assert!(matches!(
            events.first(),
            Some(Event::Started { resumed: true, .. })
        ));
        // What was already stored counts towards the total.
        assert_eq!(summary.users, 110);

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests[0]
                .url
                .query()
                .unwrap()
                .contains("max_id=from_here"),
            "the first request should continue from the cursor"
        );
    }

    #[tokio::test]
    async fn falling_short_on_a_small_list_is_not_truncation() {
        // Instagram's counter includes deleted accounts that no longer appear.
        let server = server(vec![ok(body(0, 80, None))]).await;

        let mut r = request();
        r.estimated = Some(100);
        let (summary, events, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Completed);
        assert!(
            summary.is_complete(),
            "80 out of 100 on a small list is normal"
        );
        assert!(events.iter().any(|e| matches!(e, Event::Warning(_))));
    }

    /// The rule that decides between "the counter overstates" and "Instagram
    /// stopped serving", pinned down without walking anything.
    #[test]
    fn the_two_explanations_for_a_short_list_are_told_apart() {
        // A tenth missing off a small list is the counter counting the dead.
        assert!(!truncated(80, 100));
        assert!(!truncated(270, 300));

        // Half missing is not, whatever the size. This is the case that used
        // to be reported as a complete list.
        assert!(truncated(100, 3_000));
        assert!(truncated(1_000, 2_001));

        // Past the declared threshold, any real shortfall is suspect — and it
        // is the declared count that is measured, not the walked one.
        assert!(truncated(6_000, 40_000));
        assert!(truncated(9_000, 10_000));

        // Just under the threshold with a believable difference stays clean.
        assert!(!truncated(8_000, 9_999));
    }

    /// The regression the rule above exists for: a walk that ends cleanly far
    /// short of a large declared count must not become a basis for comparison.
    #[tokio::test]
    async fn stopping_far_short_of_a_large_counter_is_truncation() {
        let server = server(vec![ok(body(0, 50, None))]).await;

        let mut r = request();
        r.estimated = Some(40_000);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(
            !summary.is_complete(),
            "50 accounts out of a declared 40,000 is a list that stopped, not a counter that lied"
        );
    }

    #[tokio::test]
    async fn falling_short_on_a_large_list_is_truncation() {
        let mut responses: Vec<ResponseTemplate> = (0..219)
            .map(|i| ok(body(i * 50, 50, Some(&format!("c{i}")))))
            .collect();
        responses.push(ok(body(10_950, 50, None)));
        let server = server(responses).await;

        let mut r = request();
        r.estimated = Some(30_000);
        let (summary, _, _) = walk(&server, r).await;

        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(
            !summary.is_complete(),
            "a large list that gets cut short cannot be compared against"
        );
    }

    #[tokio::test]
    async fn a_cooldown_prevents_starting() {
        use snob_core::budget::{RateBudget, RateBudgetError};

        struct InCooldown;
        impl RateBudget for InCooldown {
            fn reserve(&self) -> Result<Duration, RateBudgetError> {
                Ok(Duration::ZERO)
            }
            fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
                self.reserve()
            }
            fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
                Ok(Some(
                    snob_core::clock::now_ms() + Duration::from_millis(3_600_000),
                ))
            }
            fn start_cooldown(&self, _: &str, _: Duration) -> Result<EpochMs, RateBudgetError> {
                Ok(EpochMs::new(0))
            }
        }

        let server = MockServer::start().await;
        let client = client_with(&server, Pacer::new(std::sync::Arc::new(InCooldown)));
        let walker = ListWalker::new(&client);

        let error = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap_err();

        assert!(matches!(error, WalkError::Cooldown { .. }));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "in cooldown not a single request is made"
        );
    }

    /// A cooldown another process writes stops a walk already in flight.
    ///
    /// The database is shared per user, so this is the ordinary case rather
    /// than a contrived one: `snob watch` is eight minutes into a long walk,
    /// `snob whoami` in another terminal draws a 429, and the cooldown that
    /// stops *that* process was invisible to this one. The walk is the longest
    /// unbroken run of requests the tool produces and nothing on the way
    /// through re-read the table.
    ///
    /// The pages carry **distinct** users on purpose: with repeats,
    /// `MAX_PAGES_WITHOUT_NEW` would end the walk on its own and prove nothing.
    #[tokio::test]
    async fn a_cooldown_written_mid_walk_stops_the_walk() {
        use snob_core::budget::{RateBudget, RateBudgetError};

        /// Answers `None` until the walk is under way, then `Some` — the shape
        /// of another process writing the row while this one pages.
        ///
        /// **Three asks, not two, and the third one is the pacer's.** The walk
        /// consults the table at the entry check and once per iteration, and
        /// since the backstop went into `Pacer::clear` it is consulted again
        /// before every request. At two the cooldown became visible during the
        /// first page's reservation, so the walk stopped with nothing read and
        /// no cursor — which is the backstop working, and a different test from
        /// this one. `pace.rs` covers that. This one is about the check between
        /// pages, where the walk `break`s instead of erroring and the partial
        /// stays resumable, so the fixture has to let a page through first.
        #[derive(Default)]
        struct CooldownAfterThree(std::sync::atomic::AtomicUsize);
        impl RateBudget for CooldownAfterThree {
            fn reserve(&self) -> Result<Duration, RateBudgetError> {
                Ok(Duration::ZERO)
            }
            fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
                self.reserve()
            }
            fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
                let asked = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok((asked >= 3)
                    .then(|| snob_core::clock::now_ms() + Duration::from_millis(7_200_000)))
            }
            fn start_cooldown(&self, _: &str, _: Duration) -> Result<EpochMs, RateBudgetError> {
                Ok(EpochMs::new(0))
            }
        }

        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 50, Some("c3"))),
            ok(body(150, 50, Some("c4"))),
        ])
        .await;

        let client = client_with(
            &server,
            Pacer::new(std::sync::Arc::new(CooldownAfterThree::default())),
        );
        let walker = ListWalker::new(&client);
        let summary = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap();

        assert_eq!(
            summary.reason,
            StopReason::RateLimit,
            "the walk stopped for the reason it really stopped for"
        );
        assert!(
            server.received_requests().await.unwrap().len() < 4,
            "pages kept going out after the cooldown was written"
        );
        assert!(
            summary.pending_cursor.is_some(),
            "stopping is not the same as throwing the partial away"
        );
    }

    /// A budget that holds a number of accounts for the day and counts what it
    /// is charged. Asked for more than it holds, it answers a short wait and
    /// refills to `refill`: the day has passed by the time anybody asks again.
    struct Day {
        left: std::sync::atomic::AtomicU32,
        refill: u32,
    }

    impl Day {
        fn new(left: u32, refill: u32) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                left: std::sync::atomic::AtomicU32::new(left),
                refill,
            })
        }

        fn left(&self) -> u32 {
            self.left.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl snob_core::budget::RateBudget for Day {
        fn reserve(&self) -> Result<Duration, snob_core::budget::RateBudgetError> {
            Ok(Duration::ZERO)
        }
        fn reserve_write(&self) -> Result<Duration, snob_core::budget::RateBudgetError> {
            Ok(Duration::ZERO)
        }
        fn cooldown(&self) -> Result<Option<EpochMs>, snob_core::budget::RateBudgetError> {
            Ok(None)
        }
        fn start_cooldown(
            &self,
            _: &str,
            _: Duration,
        ) -> Result<EpochMs, snob_core::budget::RateBudgetError> {
            Ok(EpochMs::new(0))
        }
        fn accounts_wait(&self, n: u32) -> Result<Duration, snob_core::budget::RateBudgetError> {
            if n <= self.left() {
                return Ok(Duration::ZERO);
            }
            self.left
                .store(self.refill, std::sync::atomic::Ordering::Relaxed);
            Ok(Duration::from_millis(20))
        }
        fn spend_accounts(&self, n: u32) -> Result<(), snob_core::budget::RateBudgetError> {
            self.left.store(
                self.left().saturating_sub(n),
                std::sync::atomic::Ordering::Relaxed,
            );
            Ok(())
        }
    }

    /// A wall clock that reads its starting moment once, then two hours
    /// later: the machine slept through the day's wait.
    fn slept_through() -> EpochMs {
        static READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let start = 1_700_000_000_000;
        if READ.swap(true, std::sync::atomic::Ordering::SeqCst) {
            EpochMs::new(start + 2 * 3_600_000)
        } else {
            EpochMs::new(start)
        }
    }

    /// A wall clock somebody set an hour back once the wait had started.
    fn set_back() -> EpochMs {
        static READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let start = 1_700_000_000_000;
        if READ.swap(true, std::sync::atomic::Ordering::SeqCst) {
            EpochMs::new(start - 3_600_000)
        } else {
            EpochMs::new(start)
        }
    }

    /// A day's wait the machine slept through is over when it wakes: the
    /// budget's day is on the wall clock, and so is the end of the wait.
    #[tokio::test]
    async fn a_day_wait_slept_through_is_over_on_waking() {
        let server = MockServer::start().await;
        let client = client_with(&server, Pacer::new(Day::new(0, 0)));
        let mut walker = ListWalker::new(&client);
        walker.wall = slept_through;

        let started = std::time::Instant::now();
        let mut events = Vec::new();
        let cut_short = walker
            .sleep_through_the_day(Duration::from_secs(3_600), &mut |e| events.push(e))
            .await
            .unwrap();

        assert!(!cut_short);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(days_waited(&events), 1, "the wait is still announced");
    }

    /// A wall clock set back does not stretch the wait past what the budget
    /// asked for.
    #[tokio::test]
    async fn a_clock_set_back_does_not_stretch_a_day_wait() {
        let server = MockServer::start().await;
        let client = client_with(&server, Pacer::new(Day::new(0, 0)));
        let mut walker = ListWalker::new(&client);
        walker.wall = set_back;

        let started = std::time::Instant::now();
        let cut_short = walker
            .sleep_through_the_day(Duration::from_millis(50), &mut |_| {})
            .await
            .unwrap();

        assert!(!cut_short);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn zero_pace() -> Pace {
        Pace {
            step_ms: (0, 0),
            dwell_ms: (0, 0),
            sitting_pause_ms: (0, 0),
            ..Pace::default()
        }
    }

    fn days_waited(events: &[Event]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Waiting {
                        kind: WaitKind::Day,
                        ..
                    }
                )
            })
            .count()
    }

    /// The page that would go past the day is not asked for until the day
    /// has room, and then the same walk carries on to the end, telling its
    /// caller it is still there while it sleeps.
    #[tokio::test]
    async fn the_page_that_would_go_past_the_day_waits_for_it() {
        let server = server(vec![
            ok(body(0, 25, Some("c1"))),
            ok(body(25, 25, Some("c2"))),
            ok(body(50, 25, None)),
        ])
        .await;
        let budget = Day::new(60, 25);
        let client = client_with(&server, Pacer::new(budget.clone()));
        let beats = std::sync::atomic::AtomicU32::new(0);
        let heartbeat = || {
            beats.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(true)
        };
        let mut walker = ListWalker::new(&client)
            .with_pace(zero_pace())
            .with_heartbeat(&heartbeat);
        walker.sleeps = true;

        let mut events = Vec::new();
        let summary = walker
            .walk(request(), |p, _| Ok(p.users.len()), |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Completed);
        assert_eq!(summary.pages, 3);
        assert_eq!(days_waited(&events), 1, "sixty accounts fit two pages");
        assert_eq!(beats.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert_eq!(budget.left(), 0, "what the pages carried was charged");
    }

    /// A list going round in circles is caught however many pauses it is
    /// read across: the guards carry over a pause, or a day that allows one
    /// page at a time would never let a guard count to two.
    #[tokio::test]
    async fn a_list_going_in_circles_is_caught_across_pauses() {
        let a = || ok(body(0, 25, Some("a")));
        let b = || ok(body(25, 25, Some("b")));
        let server = server(vec![a(), b(), a(), b(), a(), b(), a(), b()]).await;
        let client = client_with(&server, Pacer::new(Day::new(25, 25)));
        let mut walker = ListWalker::new(&client).with_pace(zero_pace());
        walker.sleeps = true;

        let mut seen = std::collections::HashSet::new();
        let mut events = Vec::new();
        let summary = walker
            .walk(
                request(),
                |p, _| Ok(p.users.iter().filter(|u| seen.insert(u.pk)).count()),
                |e| events.push(e),
            )
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Truncated);
        assert!(events.contains(&Event::Warning(Warning::GoingInCircles)));
        assert_eq!(
            days_waited(&events),
            4,
            "a pause before every page but the first"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }

    /// `--max-pages` caps the walk, not each stretch between two pauses.
    #[tokio::test]
    async fn the_page_cap_counts_across_a_pause() {
        let server = server(vec![
            ok(body(0, 25, Some("c1"))),
            ok(body(25, 25, Some("c2"))),
            ok(body(50, 25, None)),
        ])
        .await;
        let client = client_with(&server, Pacer::new(Day::new(25, 25)));
        let mut walker = ListWalker::new(&client).with_pace(zero_pace());
        walker.sleeps = true;

        let request = ListRequest {
            max_pages: Some(2),
            ..request()
        };
        let summary = walker
            .walk(request, |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::PageLimit);
        assert_eq!(summary.pages, 2);
        assert_eq!(summary.pending_cursor.as_deref(), Some("c2"));
    }

    /// A walk whose capture another process took over while it slept stops
    /// there, with its cursor, and sends nothing more.
    #[tokio::test]
    async fn a_walk_no_longer_wanted_stops_while_it_sleeps() {
        let server = server(vec![ok(body(0, 25, Some("c1"))), ok(body(25, 25, None))]).await;
        let client = client_with(&server, Pacer::new(Day::new(25, 25)));
        let heartbeat = || Ok(false);
        let mut walker = ListWalker::new(&client)
            .with_pace(zero_pace())
            .with_heartbeat(&heartbeat);
        walker.sleeps = true;

        let summary = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Canceled);
        assert_eq!(summary.pending_cursor.as_deref(), Some("c1"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// Choosing to finish today reads past the day, and still charges it.
    #[tokio::test]
    async fn finishing_today_reads_past_the_day() {
        let server = server(vec![
            ok(body(0, 25, Some("c1"))),
            ok(body(25, 25, Some("c2"))),
            ok(body(50, 25, None)),
        ])
        .await;
        let budget = Day::new(30, 0);
        let client = client_with(&server, Pacer::new(budget.clone()));
        let mut walker = ListWalker::new(&client).with_pace(zero_pace());
        walker.sleeps = true;

        let request = ListRequest {
            over_budget: OverBudget::Continue,
            ..request()
        };
        let mut events = Vec::new();
        let summary = walker
            .walk(request, |p, _| Ok(p.users.len()), |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(summary.reason, StopReason::Completed);
        assert_eq!(days_waited(&events), 0);
        assert_eq!(budget.left(), 0);
    }

    /// The point of moving the budget into the client: a walk pays for every
    /// page without the walker having to remember to.
    #[tokio::test]
    async fn every_page_is_charged_to_the_budget() {
        use snob_core::budget::{RateBudget, RateBudgetError};

        #[derive(Default)]
        struct Counting(std::sync::atomic::AtomicUsize);
        impl RateBudget for Counting {
            fn reserve(&self) -> Result<Duration, RateBudgetError> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Duration::ZERO)
            }
            fn reserve_write(&self) -> Result<Duration, RateBudgetError> {
                self.reserve()
            }
            fn cooldown(&self) -> Result<Option<EpochMs>, RateBudgetError> {
                Ok(None)
            }
            fn start_cooldown(&self, _: &str, _: Duration) -> Result<EpochMs, RateBudgetError> {
                Ok(EpochMs::new(0))
            }
        }

        let server = server(vec![
            ok(body(0, 50, Some("c1"))),
            ok(body(50, 50, Some("c2"))),
            ok(body(100, 20, None)),
        ])
        .await;

        let budget = std::sync::Arc::new(Counting::default());
        let client = client_with(&server, Pacer::new(budget.clone()));
        let walker = ListWalker::new(&client);
        let summary = walker
            .walk(request(), |p, _| Ok(p.users.len()), |_| {})
            .await
            .unwrap();

        assert_eq!(summary.pages, 3);
        assert_eq!(
            budget.0.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "one reservation per request, taken by the client itself"
        );
    }

    #[tokio::test]
    async fn repeats_do_not_inflate_the_count() {
        // The same account on two pages: the running total follows what the
        // saver reports as new, not what arrived.
        let server =
            server(vec![
            ok(r#"{"users":[{"pk":1,"username":"a"},{"pk":2,"username":"b"}],"next_max_id":"c1"}"#
                .to_string()),
            ok(r#"{"users":[{"pk":2,"username":"b"},{"pk":3,"username":"c"}]}"#.to_string()),
        ])
            .await;

        let (summary, _, seen) = walk(&server, request()).await;
        assert_eq!(summary.users, 3);
        assert_eq!(seen, vec![Pk::new(1), Pk::new(2), Pk::new(3)]);
    }

    /// A page of the accounts `pks`, in that order.
    fn rows(pks: impl IntoIterator<Item = u64>, cursor: Option<&str>) -> String {
        let users: Vec<String> = pks
            .into_iter()
            .map(|i| format!(r#"{{"pk":{i},"username":"u{i}"}}"#))
            .collect();
        let cursor = match cursor {
            Some(c) => format!(r#","next_max_id":"{c}""#),
            None => String::new(),
        };
        format!(r#"{{"users":[{}]{cursor}}}"#, users.join(","))
    }

    /// Walks two pages, `first` then `second`, and says how it ended.
    async fn two_pages(first: String, second: String) -> (StopReason, Vec<Event>) {
        let server = server(vec![ok(first), ok(second)]).await;
        let (summary, events, _) = walk(&server, request()).await;
        (summary.reason, events)
    }

    /// More than one row in twenty served twice and the list is not believed
    /// complete, however short it is; at or under it, it is.
    #[tokio::test]
    async fn a_list_that_repeats_more_than_one_row_in_twenty_is_truncated() {
        // One repeat in thirteen rows: no sample is too small to count.
        let (reason, events) = two_pages(rows(0..11, Some("c1")), rows([0, 11], None)).await;
        assert_eq!(reason, StopReason::Truncated);
        assert!(
            events.contains(&Event::Warning(Warning::Repeated {
                repeated: 1,
                received: 13,
            })),
            "{events:?}"
        );

        // One in thirty.
        let (reason, _) = two_pages(rows(0..15, Some("c1")), rows((15..29).chain([0]), None)).await;
        assert_eq!(reason, StopReason::Completed);

        // Nine in a hundred and fifty, six percent.
        let (reason, events) =
            two_pages(rows(0..75, Some("c1")), rows((75..141).chain(0..9), None)).await;
        assert_eq!(reason, StopReason::Truncated);
        assert!(
            events.contains(&Event::Warning(Warning::Repeated {
                repeated: 9,
                received: 150,
            })),
            "{events:?}"
        );

        // Six in a hundred and fifty, four percent.
        let (reason, _) =
            two_pages(rows(0..75, Some("c1")), rows((75..144).chain(0..6), None)).await;
        assert_eq!(reason, StopReason::Completed);
    }

    /// Every walk writes down the rows it received and how many were
    /// repeats, under the threshold too, so that the live check measures the
    /// rate whatever it is.
    #[tokio::test]
    async fn a_walk_logs_its_repeat_rate_under_the_threshold_too() {
        #[derive(Clone, Default)]
        struct Lines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        // Every task of this test runs on its one thread. With a single
        // dispatcher registered, a test walking on another thread meanwhile
        // asks only its own (none) whether the walk's line is wanted and
        // caches "never" for everyone; a second one, alive for the test,
        // makes every thread ask them all.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let _logged = tracing::subscriber::set_default(subscriber);
        // One in thirty: under the threshold, so no warning says it.
        let (reason, events) =
            two_pages(rows(0..15, Some("c1")), rows((15..29).chain([0]), None)).await;
        assert_eq!(reason, StopReason::Completed);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Warning(Warning::Repeated { .. }))),
            "{events:?}"
        );

        let said = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        let rate: Vec<&str> = said
            .lines()
            .filter(|line| line.contains("the rows a walk received"))
            .collect();
        assert_eq!(rate.len(), 1, "{said}");
        for field in ["list=Followers", "pages=2", "received=30", "repeated=1"] {
            assert!(rate[0].contains(field), "{field} in {}", rate[0]);
        }
    }

    /// A resumed walk served the rows an earlier run of it stored has
    /// received each of them once: none is a repeat.
    #[tokio::test]
    async fn a_resumed_walk_re_served_its_stored_rows_has_no_repeats() {
        let server = server(vec![ok(rows(0..12, Some("c6"))), ok(rows(12..20, None))]).await;
        let client = client(&server);
        let walker = ListWalker::new(&client);
        let request = ListRequest {
            from: Some("c5"),
            already_stored: 20,
            ..request()
        };
        let mut events = Vec::new();
        let summary = walker
            .walk(request, |_, _| Ok(0), |e| events.push(e))
            .await
            .unwrap();
        assert_eq!(summary.reason, StopReason::Completed, "{events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::Warning(Warning::Repeated { .. }))),
            "{events:?}"
        );
    }
}
