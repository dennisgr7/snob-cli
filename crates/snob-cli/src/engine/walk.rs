//! The walk itself: opening a snapshot, filling it page by page, closing it.
//!
//! Each page commits in its own transaction, which is what makes saving partial
//! progress not an action that has to run in time but simply a matter of
//! stopping — and that matters because neither `exit()` nor `panic = "abort"`
//! run destructors.

use anyhow::Result;
use snob_core::Epoch;
use snob_core::clock::now;
use snob_core::model::{ListKind, User};
use snob_ig::pace::Pace;
use snob_ig::pager::{ListRequest, ListWalker, OverBudget, WalkError};
use snob_store::store::{Store, snapshots};

use crate::app::App;
use crate::engine::target::Target;
use crate::engine::{ListOutcome, ListQuery, Provenance};

/// About how many accounts one request brings back: the most a page is
/// asked for, which the web app gets eight to twelve of.
///
/// It is here to turn a follower count into a number of requests for a
/// sentence a person agrees to, and nothing depends on it being exact: it is
/// an estimate, labeled as one.
const ACCOUNTS_PER_REQUEST: u64 = snob_ig::pace::ACCOUNTS_PER_PAGE as u64;

/// About how many pages a list of `accounts` is: at least one, since an
/// empty list is still asked for.
pub fn pages_to_walk(accounts: u64) -> u64 {
    accounts.div_ceil(ACCOUNTS_PER_REQUEST).max(1)
}

/// The most pages a list of `accounts` may take: the app gets as few as
/// [`snob_ig::pace::ACCOUNTS_PER_PAGE_FEWEST`] of the twelve it asks for.
pub fn pages_to_walk_at_most(accounts: u64) -> u64 {
    accounts
        .div_ceil(u64::from(snob_ig::pace::ACCOUNTS_PER_PAGE_FEWEST))
        .max(1)
}

/// About how many requests walking `pages` takes: from the browser, the
/// navigation that opens the list, and each page followed by its
/// `show_many`, as the app sends them; without it, one a page. No page is
/// no request: a walk capped at none stops before the navigation.
pub fn requests_for_pages(pages: u64, browser: bool) -> u64 {
    match pages {
        0 => 0,
        pages if browser => 1 + 2 * pages,
        pages => pages,
    }
}

/// About how many requests walking a list of `accounts` takes, from the
/// browser or not as `browser` says.
pub fn requests_to_walk(accounts: u64, browser: bool) -> u64 {
    requests_for_pages(pages_to_walk(accounts), browser)
}

/// Walks the list, resuming an interrupted one when there is a usable one.
pub async fn fetch(
    app: &mut App,
    args: &ListQuery,
    kind: ListKind,
    target: &Target,
    declared: Option<u64>,
) -> Result<(Vec<User>, ListOutcome)> {
    let opened = open_snapshot(app, args, kind, target, declared)?;
    let id = opened.id;

    let over_budget = match choose_over_budget(app, args, declared, opened.already_stored) {
        Ok(chosen) => chosen,
        Err(e) => {
            let_go(app.db(), id);
            return Err(e);
        }
    };

    // Somebody else's lists give up sooner on a network failure. This is the
    // one place that decides it, so the set commands and `scan` inherit it by
    // coming through here rather than each remembering to ask.
    let pace = if target.is_self {
        Pace::default()
    } else {
        Pace::third_party()
    };

    let request = ListRequest {
        pk: target.pk,
        // Empty when the name was never learned, which the pager documents
        // as allowed and simply leaves the referer generic. Never the numeric
        // id: `https://www.instagram.com/42/followers/` is a page no browser
        // is ever on.
        username: target.username.as_deref().unwrap_or_default(),
        direction: kind.into(),
        from: opened.cursor.as_deref(),
        estimated: declared,
        max_pages: args.max_pages,
        already_stored: opened.already_stored,
        over_budget,
    };

    let (client, db, progress) = app.parts();
    // One store for two callers that never overlap: the pages are saved
    // between requests, and the claim is kept alive only while the walk sleeps
    // on the day's accounts, so other processes see it asleep rather than dead.
    let db = std::cell::RefCell::new(db);
    let still_wanted = || Ok(snapshots::keep_claim(&db.borrow(), id)?);
    let walker = ListWalker::new(client)
        .with_pace(pace)
        .with_heartbeat(&still_wanted);
    // The machine is kept from sleeping on its own while the walk reads, and
    // let go while it waits for the day's accounts (`power::KeepAwake`). A
    // walk against a test server sleeps through nothing and asks for nothing.
    let mut awake =
        crate::power::KeepAwake::new("snob is reading an Instagram list", client.is_live());

    let summary = match walker
        .walk(
            request,
            |page, _| {
                let batch: Vec<User> = page.users.iter().map(User::from).collect();
                Ok(
                    snapshots::save_page(&mut db.borrow_mut(), id, &batch, page.next_cursor())?
                        .added,
                )
            },
            |event| {
                awake.follow(&event);
                progress.event(&event);
            },
        )
        .await
    {
        Ok(summary) => summary,
        Err(error) => {
            let_go(&db.borrow(), id);
            return Err(walk_failed(app, error, target, kind));
        }
    };

    snapshots::close(app.db().conn(), id, summary.reason)?;

    // Asked of the store rather than inferred from the stop reason, and asked
    // with the same predicate the next run will use — so "it can be continued"
    // means the next run really would, cursor and resume window included.
    //
    // `is_resumable` and not `resumable`: the latter takes the claim in the
    // statement that finds the row, and would hand this process the claim
    // `close` has just released, moments before it exits.
    let resumable = snapshots::is_resumable(app.db().conn(), target.pk, kind)?;

    // What Instagram said, said out loud: the walker keeps the error next to
    // the stop reason, and a checkpoint carries the address that clears it.
    // Through `report`, because two of the client's messages end in advice
    // about a `snob` subcommand and that half is `report`'s.
    let stopped_by = summary.error.map(|error| {
        app.warn(&crate::report::what_instagram_said(&error));
        crate::exit::from_ig_error(&error)
    });

    Ok((
        snapshots::members(app.db().conn(), id)?,
        ListOutcome {
            provenance: Provenance::Walked,
            reason: summary.reason,
            // Filled in by `engine::list` from the pacer.
            requests: 0,
            started_at: opened.started_at,
            taken_at: now(),
            account_pk: target.pk,
            snapshot_id: id,
            stopped_by,
            resumable,
        },
    ))
}

/// What to do if the day's accounts run out mid-walk, and saying so first
/// when it will matter.
///
/// **It pauses unless `--same-day` said otherwise, and it asks nothing.**
/// Pausing is the answer that cannot make things worse with Instagram, and a
/// question here would be a second one per command (a crossing walks two
/// lists), which `-y` could not answer. The warning names the flag that gives
/// the other answer; a caller passing `Some(Pause)` pauses without it.
///
/// The warning is only worth giving when the rest of the list is larger than
/// what the day has left, and only a walk against Instagram has a day to
/// spend — the pager ignores the budget against a test server. Without a
/// counter the size is unknown and nothing is said; the walk pauses if it has
/// to.
fn choose_over_budget(
    app: &App,
    args: &ListQuery,
    declared: Option<u64>,
    already_stored: usize,
) -> Result<OverBudget> {
    if let Some(chosen) = args.over_budget {
        return Ok(chosen);
    }
    let Some(declared) = declared else {
        return Ok(OverBudget::Pause);
    };
    if !app.client().is_live() {
        return Ok(OverBudget::Pause);
    }
    let needed = declared.saturating_sub(already_stored as u64);
    let left = u64::from(app.client().pacer().accounts_left()?);
    if needed > left {
        app.warn(&crate::report::over_the_day(needed, left));
        app.warn(crate::report::PAUSING_BY_DEFAULT);
    }
    Ok(OverBudget::Pause)
}

/// What a walk that stopped on an error is reported as.
fn walk_failed(app: &App, error: WalkError, target: &Target, kind: ListKind) -> anyhow::Error {
    match error {
        // Reachable only when the cooldown lands between the check in
        // `engine::list` and the walk: set by another process, or by a 429 on
        // the counter poll just before it (`freshness`). Another process is
        // also how the brake arrives, so it is worded as the gates word it.
        WalkError::Cooldown { until_ms, .. } => {
            let held = app.held().ok().flatten().unwrap_or(crate::app::Held {
                until_ms,
                braked: Vec::new(),
            });
            crate::report::refuse_cooldown_mid_walk(&held)
        }
        // A crossing walks two lists in the same run, so "the walk failed" would
        // not say which one stopped. The name goes through `printable` for the
        // same reason every other account name this tool prints does: it came
        // off Instagram, not out of anybody's keyboard.
        error => {
            let who = crate::app::target_label(target.username.as_deref());
            anyhow::Error::new(error).context(format!("could not read {who}'s {kind} list"))
        }
    }
}

/// Lets go of the capture after a failure, so the next run resumes it rather
/// than paying for its pages again; see `snapshots::release`. A release that
/// fails costs only that, and the failure that led here is the one worth
/// reporting.
fn let_go(db: &Store, id: i64) {
    if let Err(e) = snapshots::release(db.conn(), id) {
        tracing::debug!(error = %e, "could not let go of the capture");
    }
}

/// The snapshot this walk will fill, and what is already known about it.
///
/// A struct rather than a tuple: `(i64, Epoch, Option<String>, usize)` at a
/// call site says nothing about which is which.
struct Opened {
    id: i64,
    /// Read from the row rather than taken now, so a resumed walk keeps the
    /// moment its **first** page was asked for. That is the honest start of the
    /// interval this list covers: it does reflect everything from that page
    /// onward, and `snapshots::RESUME_WINDOW_SECS` has already decided a pause
    /// of that length is one capture.
    started_at: Epoch,
    cursor: Option<String>,
    already_stored: usize,
}

/// Continues an interrupted walk when one is still usable, and starts a fresh
/// snapshot otherwise. Either way, half-finished ones that no longer serve are
/// cleared out rather than piling up.
fn open_snapshot(
    app: &App,
    args: &ListQuery,
    kind: ListKind,
    target: &Target,
    declared: Option<u64>,
) -> Result<Opened> {
    let conn = app.db().conn();

    let pending = if args.no_resume {
        None
    } else {
        snapshots::resumable(conn, target.pk, kind)?
    };

    if let Some(snapshot) = pending {
        return Ok(Opened {
            id: snapshot.id,
            started_at: snapshot.started_at,
            cursor: snapshot.next_cursor,
            already_stored: snapshot.member_count as usize,
        });
    }

    // Beginning beside it would pay again for every page it has stored, and
    // store two captures of one moment.
    if snapshots::walked_elsewhere(conn, target.pk, kind)? {
        return Err(crate::report::walked_elsewhere(
            target.username.as_deref(),
            kind,
        ));
    }
    snapshots::delete_partials(conn, target.pk, kind)?;
    let fresh = snapshots::begin(conn, target.pk, kind, declared)?;
    Ok(Opened {
        id: fresh.id,
        started_at: fresh.started_at,
        cursor: None,
        already_stored: 0,
    })
}
