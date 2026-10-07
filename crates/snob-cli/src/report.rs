//! The sentences the commands share.
//!
//! Small, but worth one home: these phrases are the ones a user compares
//! between commands, and two copies of "run it again" that drifted apart would
//! read as two different pieces of advice about the same situation.

use snob_core::model::{ListKind, StopReason, User, printable};
use snob_core::{Epoch, EpochMs};
use snob_ig::pager::Warning;

use crate::app::{ConsentInAdvance, Held, Viewer};
use crate::engine::Provenance;

use crate::exit::{ExitCode, ExitError};

/// What a machine with no `watch.toml` is told, by both things that look:
/// `snob watch check`, through `commands::watch::say::problem_line`, and
/// `watch::status::health`. One copy, because it is the advice a newly
/// installed tool gives and so the sentence somebody edits, and two probes a
/// person runs one after the other must not say different things about the
/// same machine.
pub const NOTHING_CONFIGURED: &str =
    "nothing is configured, so a bare \"snob watch\" has no schedule to run on";

/// What a missing consent means for a run with nobody at the keyboard.
///
/// Both lines `commands::watch::say::problem_line` renders for
/// [`crate::engine::check::Problem::NoRecordedConsent`] say it: the account
/// that was polled, and the account nothing was asked about because a cooldown
/// was standing, which gets a parenthetical after it. It is the reason `snob
/// watch` refuses to start at all, so it is worth exactly one wording.
///
/// What it deliberately does not do is say what to do about it.
/// `commands::watch::scheduled::refuse_unattended` is the sentence that names
/// `snob watch setup`, and that is the refusal itself rather than a report about
/// one.
pub const NO_RECORDED_CONSENT: &str =
    "no recorded consent, so an unattended run will refuse to read it";

/// Prints a failed run's error, as one message rather than as several.
///
/// The refusals this tool produces are paragraphs — two or three sentences with
/// deliberate newlines between them — so the continuations are indented under
/// the `error:` label, where a bare `error: {e}` would leave them at column
/// zero and read as unattributed text, and the advice comes back on its own as
/// a `hint:` rather than as more of the complaint.
///
/// A run the user stopped is not a failure to report: it gets no label and no
/// cause chain, because "error:" over "@someone was not confirmed" reads as a
/// reprimand for doing something wrong.
///
/// `.for_stderr()` on every styled label is not optional. Without it `console`
/// decides on stdout's color state, so the labels lose their color when only
/// stdout is redirected, and write escape codes into the file when only stderr
/// is.
pub fn print_error(error: &anyhow::Error, wording: Wording) {
    print_error_as(error, wording, acting_as().as_ref());
}

/// [`print_error`] for a failure met while acting as `viewer`, which a run
/// over several accounts knows better than the account acting last.
pub(crate) fn print_error_as(error: &anyhow::Error, wording: Wording, viewer: Option<&Viewer>) {
    match wording {
        Wording::Prose => eprint!("{}", rendered(error)),
        Wording::Json => eprintln!("{}", error_json_for(error, viewer)),
    }
}

/// The account this run acts as, for a failure told in JSON to name.
///
/// A static rather than an argument: a failure reaches `main` through `?`
/// from anywhere, and the account was set long before, by `main` once the
/// account was resolved and by `App::open` once its session was read.
static ACTING_AS: std::sync::Mutex<Option<Viewer>> = std::sync::Mutex::new(None);

/// Records the account this run acts as.
pub fn act_as(viewer: Viewer) {
    set_acting_as(Some(viewer));
}

/// Records the account this run acts as, or that it acts as none.
pub(crate) fn set_acting_as(viewer: Option<Viewer>) {
    *ACTING_AS.lock().unwrap_or_else(|e| e.into_inner()) = viewer;
}

/// The account this run acts as, once one has been resolved.
pub(crate) fn acting_as() -> Option<Viewer> {
    ACTING_AS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// How a failure is told: to a person, or to a program.
///
/// A run whose result was going to be JSON tells its failure in JSON too, so
/// a script reading `snob followers --format json` can parse the hint and the
/// challenge address a code 4 carries, not only the exit code. The decision is
/// `main`'s, from the format the command was going to answer in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wording {
    Prose,
    Json,
}

/// The failure as one JSON object, for a caller that asked for JSON.
///
/// One shape for every command: `code` is the same token the exit status
/// names, so a reader that only has the stream and a reader that only has
/// `$?` are told the same thing; `message` and `causes` are the chain the
/// prose prints, in the same order; `hint` is the advice the prose sets
/// apart; `url` is the address a challenge has to be cleared at;
/// `cooldown_until` is when a cooldown that refused the run lifts, and only
/// that: a push-back met in this run starts one this does not look up.
/// Printed as one line, the last on standard error. Filtered line by line
/// like the prose, and for the same reason -- a name is filtered before
/// anything draws it, and a JSON consumer may well print it. `viewer`, beside
/// `error`, is the account the run acted as, or null before there was one.
fn error_json_for(error: &anyhow::Error, viewer: Option<&Viewer>) -> serde_json::Value {
    let code = crate::exit::exit_code_for(error);
    let mut chain = error
        .chain()
        .map(|cause| filtered(&cause.to_string(), "\n"));
    let message = chain.next().unwrap_or_default();
    let causes: Vec<String> = chain.collect();
    let instagram = instagram_in(error);
    let hint = advice_in(error).map(|hint| filtered(hint, "\n"));
    let url = instagram.and_then(|e| e.challenge_url());
    let lifts = instagram.and_then(|e| match e {
        snob_ig::error::IgError::InCooldown { until_ms } => Some(until_ms.to_epoch()),
        _ => None,
    });

    serde_json::json!({
        "viewer": viewer.map(Viewer::json),
        "error": {
            "code": code.as_str(),
            "exit": crate::exit::code(code),
            "message": message,
            "causes": causes,
            "hint": hint,
            "url": url,
            "cooldown_until": lifts,
        }
    })
}

/// The block `print_error` writes, built rather than printed, so a test can
/// assert on every branch of it, the filter included, from inside the process.
fn rendered(error: &anyhow::Error) -> String {
    let code = crate::exit::from_chain(error);
    if code == Some(ExitCode::Interrupted) {
        // No label, so nothing to indent under — but the filter is not the
        // label's business. It applies here for the same reason it applies
        // below: the sentence names an account, and the name came from
        // `watch.toml` or from a terminal, not from this program.
        return format!("{}\n", filtered(&error.to_string(), "\n"));
    }

    let label = console::style("error:").red().bold().for_stderr();
    let mut out = format!("{label} {}\n", indented(&error.to_string()));

    for cause in error.chain().skip(1) {
        let caused = console::style("caused by:").dim().for_stderr();
        out.push_str(&format!("  {caused} {}\n", indented(&cause.to_string())));
    }

    if let Some(hint) = advice_in(error) {
        let label = console::style("hint:").cyan().bold().for_stderr();
        out.push_str(&format!("{label}  {}\n", indented(hint)));
    }

    // The backstop in `Pacer::clear` answers with the epoch and no wording,
    // because when a cooldown lifts is a date and `snob-ig` has no business
    // formatting one. Said here so the last resort tells a person the same thing
    // the explicit gates in front of it tell them; every path anybody really
    // reaches names the date itself, so this fires only when one of them was
    // forgotten — which is exactly when the person reading needs it most.
    if let Some(snob_ig::error::IgError::InCooldown { until_ms }) = instagram_in(error) {
        let label = console::style("hint:").cyan().bold().for_stderr();
        out.push_str(&format!(
            "{label}  {}\n",
            indented(&format!("it lifts {}", cooldown_ends_at(*until_ms)))
        ));
    }

    out
}

/// The first thing Instagram's client said, anywhere in the chain.
fn instagram_in(error: &anyhow::Error) -> Option<&snob_ig::error::IgError> {
    error.chain().find_map(|c| c.downcast_ref())
}

/// The one piece of advice a failure carries, read the same way by the prose
/// and by the JSON so the two shapes cannot disagree.
///
/// The command's own advice first, and Instagram's client's only when the
/// command had none: a command that has written advice for this exact
/// situation knows more than a variant does. The two cannot in fact meet —
/// `ExitError` carries no source, so nothing of Instagram's is ever underneath
/// one — but the order is written down rather than left to that.
fn advice_in(error: &anyhow::Error) -> Option<&str> {
    error
        .chain()
        .find_map(|c| c.downcast_ref::<ExitError>())
        .and_then(ExitError::hint)
        .or_else(|| instagram_in(error).and_then(advice_for))
}

/// Lines after the first start under the label rather than at column zero.
///
/// Deliberately not re-wrapped to the terminal width: the only hard breaks in
/// these strings are the ones somebody put between sentences, and re-wrapping
/// would eventually split a URL or `snob login --paste` across a line. The
/// terminal already soft-wraps at the width it really has.
fn indented(text: &str) -> String {
    filtered(text, "\n       ")
}

/// The filter every string this module prints goes through, which makes it the
/// boundary the rule asks for: a name is filtered before anything draws it,
/// whoever it came from. Each of these messages is built by interpolating
/// something into a sentence, so filtering the result spares every
/// interpolation from having to remember.
///
/// **Line by line, not over the whole string.** `printable` turns any
/// whitespace into a space, newlines included, so filtering the text whole
/// would collapse the deliberate paragraph breaks [`indented`] exists to lay
/// out. Split first and the real breaks survive while an escape sequence
/// injected into a name does not.
///
/// `join` is how the caller puts the lines back together: under the label for
/// a reported failure, and with a bare newline where there is no label to
/// indent under.
fn filtered(text: &str, join: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join(join)
}

/// The requests half of a summary line: what was spent, or the promise that
/// nothing was. Shared by the single lists and the crossings, so "without
/// touching the network" is said one way.
pub fn spent(n: u32) -> String {
    if n > 0 {
        format!(" - {}", requests(n))
    } else {
        " - without touching the network".to_string()
    }
}

/// The date a crossing's summary names: the older of the two captures,
/// because a crossing is only as recent as its staler half.
///
/// `check_same_moment` is what stops the two being far apart at all, so this
/// is completeness rather than a correction. `sets` and `scan` both use it.
pub fn stored_on_the_older_of(a: snob_core::Epoch, b: snob_core::Epoch) -> String {
    stored_on(a.min(b))
}

/// "Aug 3 at 14:12", in the local zone like every other moment the tool prints.
pub fn stored_on(taken_at: Epoch) -> String {
    format_epoch(taken_at, "earlier")
}

/// "Aug 3, 2024", for a moment that may be years old.
///
/// [`stored_on`] carries no year because everything it dates — a capture, a
/// story, a cooldown — is at most days away and the hour is the informative
/// part. A highlight is the opposite: kept for years on purpose, so "Aug 3"
/// alone would read as this year and be wrong most of the time, and the
/// hour of a moment years back says nothing worth a column.
pub fn dated(at: Epoch) -> String {
    chrono::DateTime::from_timestamp(at.get(), 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%b %-d, %Y")
                .to_string()
        })
        .unwrap_or_else(|| "sometime".to_string())
}

/// "Aug 4 at 16:30", from a cooldown end.
///
/// The conversion to seconds is [`EpochMs::to_epoch`]: the printed date and
/// `whoami`'s JSON field are the same cooldown, so the arithmetic belongs with
/// the type.
fn cooldown_ends_at(until_ms: EpochMs) -> String {
    format_epoch(until_ms.to_epoch(), "later")
}

/// Until when nothing may be sent, as a clause: the account's own cooldown,
/// or the brake on every account.
pub fn held_until(held: &Held) -> String {
    if held.braked.is_empty() {
        format!(
            "the account is in cooldown until {}",
            cooldown_ends_at(held.until_ms)
        )
    } else {
        brake_sentence(held.until_ms, &held.braked)
    }
}

/// The brake on every account, naming the accounts whose push-backs set it.
fn brake_sentence(until_ms: EpochMs, accounts: &[String]) -> String {
    let named = and_list(accounts).unwrap_or_else(|| "more than one account".to_string());
    format!(
        "every account is paused until {} because Instagram pushed back on {named} \
         within an hour",
        cooldown_ends_at(until_ms)
    )
}

/// A moment, for a person to read.
///
/// **The month is named rather than numbered**, because `03/08` is the eighth
/// of March to one reader and the third of August to another; naming it
/// removes the ambiguity for everybody. `%-d` rather than `%d` because "Aug 3"
/// is how the date is said.
///
/// **In the local zone**, because everything else the person reads is. The
/// schedule is evaluated in `chrono::Local`, the README says "times are
/// your local ones", and a cooldown "until 15:46" is read against the clock
/// on the wall. The zone is taken at the call, so the test can pin the
/// arithmetic with a fixed one.
fn format_epoch(at: Epoch, unknown: &str) -> String {
    format_epoch_in(at, unknown, &chrono::Local)
}

fn format_epoch_in<Z: chrono::TimeZone>(at: Epoch, unknown: &str, zone: &Z) -> String
where
    Z::Offset: std::fmt::Display,
{
    chrono::DateTime::from_timestamp(at.get(), 0)
        .map(|t| t.with_timezone(zone).format("%b %-d at %H:%M").to_string())
        .unwrap_or_else(|| unknown.to_string())
}

/// The refusal shared by the crossings and the summary.
///
/// Both stop for the same reason and owe the user the same three things: which
/// list failed, what the answer would have claimed if it had been given anyway,
/// and whether running it again continues or starts over. Two copies of that
/// would be two ways of describing one situation.
///
/// `misreading` completes "the accounts missing from it would appear as if …",
/// which is the only part that differs between them.
///
/// The outcome rather than a reason and a code picked out of it. The two have to
/// describe the same walk, and `ListOutcome::exit_code` exists precisely because
/// what Instagram said beats what the store recorded — a caller passing
/// `outcome.reason` next to a bare `exit::from_stop_reason` would undo that
/// precedence while still compiling.
pub fn refuse_incomplete(
    list: ListKind,
    outcome: &crate::engine::ListOutcome,
    misreading: &str,
) -> anyhow::Error {
    ExitError::new(
        outcome.exit_code(),
        format!(
            "the {list} list could not be read in full, so the answer would be wrong: \
             the accounts missing from it would appear as if {misreading}."
        ),
    )
    .with_hint(try_again_advice(outcome.reason, outcome.resumable))
    .into()
}

/// Two stored lists too far apart to be crossed.
///
/// The wording and the code live here rather than in `engine` because that is
/// what AGENTS.md says: engine returns data and where the data came from, and
/// never decides how anything looks. It hands over the two provenances and the
/// two dates; which sentence and which code those deserve is this module's
/// question.
///
/// And they really do differ, in three ways rather than two:
///
/// - A **cooldown** is waited out, so "run it again later" is true and the
///   throttling code is right.
/// - **`--offline`** is the user's own doing, and dropping it is the fix.
/// - A **failed poll** is neither. Nobody asked for storage — the request to
///   check went out and did not come back — so advising them to drop a flag
///   they never typed sends them looking for something that is not there.
pub fn refuse_different_moments(
    a: Provenance,
    b: Provenance,
    a_at: Epoch,
    b_at: Epoch,
) -> anyhow::Error {
    // Each arm asks the same question of the same pair, so each one asks it the
    // same way.
    let either = |wanted: Provenance| a == wanted || b == wanted;
    let (code, hint) = if either(Provenance::Cooldown) {
        (
            ExitCode::RateLimited,
            "Run it again once the cooldown lifts.",
        )
    } else if either(Provenance::CacheFlag) {
        (
            ExitCode::Error,
            "Run it again without --offline, so both lists are checked against the account.",
        )
    } else {
        (
            ExitCode::Error,
            "Instagram could not be reached to check either list. Run it again in a while.",
        )
    };

    ExitError::new(
        code,
        format!(
            "the two stored lists are from different moments ({} and {}), so crossing \
             them would invent results.",
            stored_on(a_at),
            stored_on(b_at)
        ),
    )
    .with_hint(hint)
    .into()
}

/// Why a cooldown could not be served around.
///
/// A plain description of the situation, handed over by `engine` so that this
/// module can choose the words.
#[derive(Debug, Clone, Copy)]
pub enum Blocked<'a> {
    /// `--refresh` was asked for, and walking is exactly what cannot happen.
    RefreshWanted,
    /// The named account has never been tracked, so there is nothing stored.
    AccountUnknown(&'a str),
    /// The account is known but this list has never been walked to the end.
    NothingStored(ListKind),
}

/// The account is in cooldown and storage cannot answer either.
pub fn refuse_in_cooldown(held: &Held, blocked: Blocked<'_>) -> anyhow::Error {
    let held = held_until(held);
    let detail = match blocked {
        Blocked::RefreshWanted => format!("{held}; --refresh cannot walk until it lifts"),
        // Filtered here rather than at the call site, so every caller of the
        // variant gets it. `engine::cooldown` passes `target::clean`'s answer,
        // which only strips a leading `@` — and that name reaches this without
        // anybody typing it, because `Watched::list_args` puts the one from
        // `watch.toml` straight into `ListArgs.target` and nothing validates a
        // username there.
        Blocked::AccountUnknown(name) => format!(
            "{held}, and no list of @{} is stored to serve in the meantime",
            printable(name)
        ),
        Blocked::NothingStored(kind) => format!(
            "{held}, and no complete snapshot of the {kind} list is stored, so there \
             is nothing to serve"
        ),
    };
    ExitError::new(ExitCode::RateLimited, detail).into()
}

/// A cooldown, or the brake, that landed between the check and the walk.
pub fn refuse_cooldown_mid_walk(held: &Held) -> anyhow::Error {
    ExitError::new(
        ExitCode::RateLimited,
        format!("{}; nothing can be walked until it lifts", held_until(held)),
    )
    .into()
}

/// A list another snob is walking right now, awake or asleep on the budget.
pub fn walked_elsewhere(target: Option<&str>, kind: ListKind) -> anyhow::Error {
    let who = crate::app::target_label(target);
    let minutes = snob_store::store::snapshots::CLAIM_TTL_SECS / 60;
    anyhow::anyhow!(
        "another snob is walking {who}'s {kind} list right now; run this again \
         once it has finished, and it will use what that walk stored.\n\
         A walk that stopped without letting go is taken over after {minutes} minutes."
    )
}

/// What somebody is agreeing to when they let a run read a stranger's lists.
///
/// Three facts about the request rather than about the account, which is why
/// there is one of these rather than one per target.
pub const READING_SOMEBODY_ELSES_LIST: &str = "this reads a list that belongs to somebody else, and lands their followers \
     in your local database. It is also a heavier request than reading your own, \
     and Instagram is readier to refuse it";

/// The rest of a list is more than the day's accounts can cover.
pub fn over_the_day(needed: u64, left: u64) -> String {
    format!(
        "the rest of this list is about {needed} accounts and today's budget has {left} left. \
         Reading more than that in a day is the pattern Instagram answers with an \
         automated-activity warning"
    )
}

/// What happens instead, and the flag that changes it. Nothing is asked: see
/// `engine::walk::choose_over_budget`.
pub const PAUSING_BY_DEFAULT: &str = "it will pause when the budget runs out and continue on \
     its own; --same-day finishes it today instead";

/// The walk is asleep on the day's accounts and will go on by itself.
pub fn paused_for_the_day(wait: std::time::Duration) -> String {
    let minutes = wait.as_secs().div_ceil(60);
    let when = if minutes < 120 {
        format!("{minutes} minutes")
    } else {
        format!("about {} hours", minutes.div_ceil(60))
    };
    format!(
        "today's account budget is spent; waiting {when} to continue from where it stopped. \
         Ctrl+C stops here, and the next run picks up from the same page"
    )
}

/// What the monitor says when it stops a walk for the battery.
pub const BATTERY_CRITICAL_STOP: &str = "the battery is critical; the walk stops here, \
     and the next run picks it up from this page";

/// What the monitor says when the battery turned critical before a list it
/// had not started.
pub const BATTERY_CRITICAL_SKIP: &str =
    "the battery is critical; this run reads no more lists, and the next run reads them";

/// What the scheduled monitor says when the run that is due waits for power.
pub const BATTERY_CRITICAL_HOLD: &str = "the battery is critical; the run that is due waits \
     until the machine is on power or charged again";

/// What a walk says when it finds the machine slept in the middle of it.
pub fn slept_during_the_walk(slept: std::time::Duration) -> String {
    let minutes = slept.as_secs().div_ceil(60);
    let how_long = if minutes < 120 {
        format!("{minutes} minutes")
    } else {
        format!("about {} hours", minutes.div_ceil(60))
    };
    format!(
        "the computer slept for {how_long}; picking the walk up again where it stopped \
         once the network is back"
    )
}

/// The consent question, with the account named the way the warning above
/// named it.
pub fn ask_to_continue(shown: &str) -> String {
    format!("Continue with {shown}?")
}

/// Being unable to ask and being told no are two different events, and this is
/// the first one, shaped the same way everywhere a question finds no terminal:
/// what was not done, why nothing could be asked, and how to answer in
/// advance. Exit 130, which is what the README's table and `--help` both
/// promise for a confirmation that was not given.
///
/// The advice rides in the message rather than on a hint, and that is
/// load-bearing: [`rendered`] returns early for this exit code, so a hint set
/// on an interrupted error is advice nobody is ever shown. The scheduled
/// monitor keeps its own variant, because "a scheduled run has nobody to ask"
/// is a different fact from "there is no terminal".
pub fn refuse_unattended(refused: String, in_advance: String) -> anyhow::Error {
    ExitError::new(
        ExitCode::Interrupted,
        format!("{refused}, and there is no terminal to ask at. {in_advance}"),
    )
    .into()
}

/// Nobody is there to be asked, so nothing is enumerated.
///
/// Which way to answer in advance is the **caller's** fact rather than this
/// sentence's, and it arrives as [`ConsentInAdvance`]: the list commands take
/// `-y`, and `watch once` deliberately has no `-y`, so advice naming it there
/// would fail to parse.
///
/// `shown` is `target::label`'s answer, so it is already the at sign and the
/// filtered name.
pub fn refuse_unconsented(shown: &str, in_advance: ConsentInAdvance) -> anyhow::Error {
    let in_advance = match in_advance {
        ConsentInAdvance::Flag => "Pass -y to confirm in advance.".to_string(),
        ConsentInAdvance::WatchConfig => format!(
            "Run \"snob watch setup\" to answer it once, or ask about {shown} \
             while you are here."
        ),
    };
    refuse_unattended(
        format!("reading {shown}'s lists needs confirmation"),
        in_advance,
    )
}

/// They were asked, and they said no.
///
/// No mention of `-y` here, and that is the whole difference from
/// [`refuse_unconsented`]: they have just said no, and answering that with
/// "pass the flag that skips the question" is telling them to do it anyway.
pub fn refuse_declined(shown: &str) -> anyhow::Error {
    ExitError::new(
        ExitCode::Interrupted,
        format!("nothing was done: {shown} was not confirmed"),
    )
    .into()
}

/// Serving a stored list because nothing may be spent.
///
/// The list is named because a crossing serves two of them, and two identical
/// warnings in a row read like the same one printed twice.
pub fn serving_stored_in_cooldown(held: &Held, kind: ListKind, taken_at: Epoch) -> String {
    format!(
        "{}; serving the {kind} list stored on {}",
        held_until(held),
        stored_on(taken_at)
    )
}

/// The counter poll failed and there is a stored list to fall back on.
///
/// Walking the whole list right when Instagram is already having trouble is the
/// worst possible reaction, so the warning says what was served rather than
/// what was refused.
pub fn poll_failed_serving_stored(error: &anyhow::Error) -> String {
    format!(
        "could not check for changes ({}); using the stored list",
        what_went_wrong(error)
    )
}

/// The counter poll failed and nothing is stored, so the walk goes ahead
/// without a number to check it against.
pub fn poll_failed(error: &anyhow::Error) -> String {
    format!("could not read the profile ({})", what_went_wrong(error))
}

/// A failure as one clause inside a sentence.
///
/// Interpolating the error would print [`snob_ig::error::IgError`]'s diagnosis
/// without its advice, on the two warnings a dead session reaches most often.
/// Only the head of the chain is asked, because that is the one whose text is
/// about to be printed.
fn what_went_wrong(error: &anyhow::Error) -> String {
    match error
        .chain()
        .next()
        .and_then(|head| head.downcast_ref::<snob_ig::error::IgError>())
    {
        Some(instagram) => what_instagram_said(instagram),
        None => error.to_string(),
    }
}

/// A private account nobody here can read the lists of.
///
/// Two answers rather than one, because a pending follow request is a different
/// situation from never having asked: the first is waiting on somebody else and
/// the second is waiting on the reader.
///
/// Filtered here rather than at the call site: the name came off Instagram and
/// this sentence is written to a terminal.
pub fn refuse_private(username: &str, requested: bool) -> anyhow::Error {
    if requested {
        return anyhow::anyhow!(
            "@{} is private and your follow request has not been accepted yet, \
             so its lists cannot be read",
            printable(username)
        );
    }
    anyhow::anyhow!(
        "@{} is a private account you do not follow, so its lists cannot be read",
        printable(username)
    )
}

/// The account whose profile Instagram will not serve, and what that costs the
/// run.
///
/// Two things people expect quietly stop happening — the truncation wall cannot
/// be detected without a declared size, and `--offline` has nothing to weigh
/// freshness against — so both are said out loud rather than left to be
/// discovered.
pub fn counters_unknowable(username: &str) -> String {
    format!(
        "Instagram would not serve the profile of @{}, so its id came from search \
         instead. That route carries no follower or following counts, so this run \
         cannot tell a truncated list from a complete one, and cannot judge whether \
         a cached list is still current.",
        printable(username)
    )
}

/// A run that concluded nothing about an account nothing is stored for.
///
/// It is the tick's own refusal rather than a report, so it names no command:
/// there was nothing wrong with what the user asked for, and the next run may
/// well answer it.
pub fn refuse_nothing_looked_at(name: &str, code: ExitCode) -> anyhow::Error {
    ExitError::new(
        code,
        format!(
            "nothing could be looked at for @{} this time, and nothing is stored \
             about that account yet to report against",
            printable(name)
        ),
    )
    .into()
}

/// A name the monitor was pointed at and has never walked.
///
/// Deliberately not [`refuse_nothing_stored`], which talks about `--offline` — a
/// flag the commands that reach this do not have. What the user has to do here
/// is walk the account once, and the sentence says so.
pub fn refuse_never_walked(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nothing is stored about @{}. Run \"snob followers {}\" once and the monitor \
         will have something to compare against from then on.",
        printable(name),
        printable(name),
    )
}

/// "@someone followers" — what a run is walking, said the same way by every
/// command that walks something.
pub fn walking(kind: ListKind, subject: &str) -> String {
    format!("{subject} {kind}")
}

/// "3 unfollowers", plus what took the others away when anything did.
///
/// Three counts, because two things can shorten a list and they are not the
/// same news. `total` is what the crossing produced, `kept` what survived the
/// filters, and `shown` what `--limit` left: `--limit 3` on ten unfollowers
/// with no filters is a trim, not "the rest filtered out".
///
/// Both forms of the noun are handed in rather than an `s` being bolted on:
/// what gets counted here is a whole phrase — "accounts you follow that do not
/// follow you back" — whose singular differs by three words rather than by a
/// final letter. The count of one is not a rare case, either: it is what a
/// filtered list reaches most often.
pub fn counted(shown: usize, kept: usize, total: usize, one: &str, many: &str) -> String {
    let what = if shown == 1 { one } else { many };
    let mut line = format!("{shown} {what}");
    match (kept < total, shown < kept) {
        (false, false) => {}
        (true, false) => line.push_str(&format!(" (of {total}, the rest filtered out)")),
        (false, true) => line.push_str(&format!(" (of {kept}, trimmed by --limit)")),
        (true, true) => line.push_str(&format!(
            " (of {total}: {} filtered out, the rest trimmed by --limit)",
            total - kept
        )),
    }
    line
}

/// "1 request" / "7 requests".
pub fn requests(n: u32) -> String {
    if n == 1 {
        "1 request".to_string()
    } else {
        format!("{n} requests")
    }
}

/// The closing advice for a message about an incomplete walk.
///
/// A dead session and a checkpoint come first: running it again is the one
/// thing that cannot help, and against an account Instagram has just flagged
/// it is what turns a checkpoint into something longer.
///
/// Everything else is told by `resumable`, which the store answered rather than
/// this module guessing from the reason. A walk is continued for a day and a
/// half after it began (`snapshots::RESUME_WINDOW_SECS`), so one stopped by
/// throttling can be continued once the cooldown lifts, if that is soon
/// enough. `Truncated` is proof that nothing is left to continue from in
/// exactly one of the five ways it arrives: the reclassification
/// `verify_completion` makes once pagination has already ended, where there is
/// no cursor to save. The other four — the hard page cap, a cursor that came
/// back unchanged, two empty pages, and several pages with nothing new — stop
/// in the **middle** of the pagination with a cursor stored. So the advice
/// asks instead of assuming.
pub fn try_again_advice(reason: StopReason, resumable: bool) -> &'static str {
    match reason {
        StopReason::RateLimit if !resumable => {
            "Run it again once the cooldown lifts; there is nothing stored to continue from, so \
             it starts over."
        }
        StopReason::RateLimit => {
            "Run it again once the cooldown lifts to continue where it left off, within a day \
             and a half of when this walk began; after that it starts over."
        }
        StopReason::SessionInvalid => {
            "Deal with what Instagram asked for first. Running it again before that cannot get \
             any further."
        }
        StopReason::Truncated if !resumable => {
            "Instagram stopped serving this account's list; there is nothing to continue from, \
             so running it again starts over. Try later."
        }
        StopReason::Truncated => {
            "Instagram stopped serving pages. Run it again within a day and a half of when this \
             walk began and it continues from where it stopped; after that it starts over."
        }
        _ if !resumable => {
            "Run it again; there is nothing stored to continue from, so it starts over."
        }
        _ => "Run it again to continue where it left off.",
    }
}

/// Why a walk did not finish, in the words the summary uses.
pub fn why_incomplete(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::Completed => None,
        StopReason::PageLimit => Some("cut short by the cap you asked for"),
        StopReason::Canceled => Some("interrupted"),
        StopReason::Truncated => Some("Instagram stopped serving pages"),
        StopReason::RateLimit => Some("Instagram is throttling requests"),
        StopReason::Network => Some("network failure"),
        StopReason::SessionInvalid => Some("the session stopped working"),
    }
}

/// What to do about something Instagram's client reported, when there is
/// anything to do about it.
///
/// The advice names `snob` subcommands, which `snob-ig` does not know it is
/// part of: it has no terminal and no exit codes. The diagnosis stays in the
/// variant, because only the client knows what happened; the advice lives
/// here, with the rest of the tool's advice, and reaches the reader as the
/// `hint:` line every other refusal puts it on.
///
/// `&'static str` and not a sentence built per call: none of this depends on
/// what was being asked for. Where a cooldown lifts is the counter-example, and
/// it is a date rather than advice — [`rendered`] adds that one separately.
fn advice_for(error: &snob_ig::error::IgError) -> Option<&'static str> {
    use snob_ig::error::IgError;
    match error {
        IgError::SessionExpired => Some("run \"snob login\" again"),
        IgError::NoCsrfToken => Some(
            "run \"snob login --browser\", or, with SNOB_NO_BROWSER, pass the token with \
             \"snob login --paste --csrftoken\"",
        ),
        _ => None,
    }
}

/// One line: what Instagram's client said, and what to do about it.
///
/// The `hint:` line is what a **reported failure** gets, and the two places
/// that print an `IgError` are not that: `engine::walk` warns about what
/// stopped a walk while the walk's own refusal is still to come, and
/// `engine::check` puts the answer in a column of its own. So this joins the
/// diagnosis and the advice on one line.
pub fn what_instagram_said(error: &snob_ig::error::IgError) -> String {
    match advice_for(error) {
        Some(advice) => format!("{error}; {advice}"),
        None => error.to_string(),
    }
}

/// What the walker noticed, in the words the person watching it reads.
///
/// The pager reports the condition and the words are decided here, beside
/// every other sentence the tool prints: the HTTP crate has no terminal, no
/// format and no business having an opinion about either.
///
/// Three of them carry numbers, which is why this returns a `String` rather than
/// a `&'static str`: what makes a shortfall worth reading is how big it is.
pub fn pager_warning(warning: Warning) -> String {
    match warning {
        Warning::SameCursorTwice => {
            "Instagram returned the same cursor twice; stopping so the request is not repeated"
                .to_string()
        }
        Warning::TwoEmptyPages => "Instagram returned two empty pages in a row".to_string(),
        Warning::GoingInCircles => {
            "several pages in a row with no new accounts; the list is going in circles".to_string()
        }
        Warning::EmptyAndNoCounter => {
            "the list came back empty and the profile counter could not be read, so there is \
             no way to tell an empty list from one Instagram did not serve; treating it as \
             incomplete rather than risking the comparison"
                .to_string()
        }
        Warning::StoppedShort { walked, declared } => format!(
            "Instagram stopped serving pages at {walked} of the {declared} accounts it declared; \
             the list is incomplete and cannot be compared against"
        ),
        Warning::ShortOfDeclared { walked, declared } => format!(
            "walked {walked} accounts while Instagram declared {declared}; \
             the difference is usually deleted accounts"
        ),
        Warning::Repeated { repeated, received } => format!(
            "{repeated} of the {received} accounts Instagram served were ones it had already \
             served, more than one in twenty; the list may be missing accounts and cannot be \
             compared against"
        ),
    }
}

/// Nothing stored to answer with, and `--offline` said not to look.
///
/// Two situations reach this: the account has never been seen at all, and the
/// account is known but this list of it has never been walked. They get the
/// same answer because there is one thing to do about either.
///
/// It carries the `hint:` its four siblings here carry. It gains no exit code:
/// the `anyhow` fallback is already `Error`, and this is the shape that keeps
/// the advice apart from what happened.
pub fn refuse_nothing_stored(kind: ListKind) -> anyhow::Error {
    ExitError::new(
        ExitCode::Error,
        format!("no {kind} list is stored, and --offline says not to look for one"),
    )
    .with_hint(format!("run \"snob {kind}\" once, or drop --offline"))
    .into()
}

/// "pepito, carlos and 4 others", or `None` when there is nobody to name.
///
/// The cap is not about width. Past a handful the line stops being "people you
/// know" and becomes a list, and a list is what the `friends` command is for.
///
/// It lives here rather than in `engine` because it is a finished English
/// sentence: it prefixes each name with `@`, joins with commas, swaps the last
/// separator for "and" and picks between "1 other" and "N others". The count is
/// the last item of the one list, so the line never reads "@ana, @luis and @eva
/// and 2 others".
pub fn name_a_few(people: &[User], cap: usize) -> Option<String> {
    // A cap of zero would name nobody and count everybody, which is not a
    // sentence anyone wants to read. At least one name, always.
    let shown = cap.max(1).min(people.len());
    if shown == 0 {
        return None;
    }

    // Filtered here rather than at each consumer: this line is the first thing
    // `snob scan` prints, with no flag needed, and it also goes into the
    // markdown summary, whose escaping is about table cells rather than about
    // what a terminal obeys.
    let mut parts: Vec<String> = people[..shown]
        .iter()
        .map(|u| format!("@{}", u.safe_username()))
        .collect();
    parts.extend(match people.len() - shown {
        0 => None,
        1 => Some("1 other".to_string()),
        n => Some(format!("{n} others")),
    });

    and_list(&parts)
}

/// "a", "a and b", "a, b and c": names as a sentence says them, or `None` for
/// none, whose wording is the caller's.
///
/// The last one joins with "and" rather than a comma, because this is a
/// sentence rather than a column.
pub fn and_list(names: &[String]) -> Option<String> {
    match names.split_last()? {
        (last, []) => Some(last.clone()),
        (last, rest) => Some(format!("{} and {last}", rest.join(", "))),
    }
}

/// A schedule as clauses, ready to be joined into a sentence.
///
/// Two places read a schedule back to a person — the banner a run opens with,
/// and the summary `status` and `setup` print — and both describe it with these
/// clauses, so a reader comparing them sees the same thing described.
///
/// The caller joins them after its own verb, because "Runs …" and "Running …"
/// are the difference between a description and an announcement.
pub fn schedule_clauses(
    every: Option<std::time::Duration>,
    on: &[String],
    at: &[String],
    cron: Option<&str>,
) -> Vec<String> {
    let mut clauses = Vec::new();
    if let Some(every) = every {
        clauses.push(format!("every {}", snob_core::duration::format(every)));
    }
    if !on.is_empty() {
        clauses.push(format!("on {}", printable(&on.join(", "))));
    }
    if !at.is_empty() {
        clauses.push(format!("at {}", printable(&at.join(", "))));
    }
    if let Some(cron) = cron {
        clauses.push(format!("on the schedule \"{}\"", printable(cron)));
    }
    clauses
}

/// What the jitter does, or nothing at all when there is none.
///
/// **The wording is shared and the number is not.** The banner reports
/// `Schedule::jitter()`, which is clamped to what the calendar can absorb; the
/// summary reports what the file says. Those are two different facts about the
/// same setting, and a reader comparing them is entitled to see both — so each
/// caller passes its own.
pub fn jitter_sentence(jitter: std::time::Duration) -> Option<String> {
    (!jitter.is_zero()).then(|| {
        format!(
            "Each run is pushed up to {} later, so it does not land on the same second every \
             time.",
            snob_core::duration::format(jitter)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ListOutcome;
    use snob_core::Pk;

    /// Every string this module draws goes through the name filter, and the
    /// paragraph breaks it lays out survive it.
    ///
    /// `printable` turns any whitespace into a space, newlines included, so
    /// filtering a refusal whole would collapse the very structure `indented`
    /// exists to produce. Line by line, both hold.
    #[test]
    fn the_layout_survives_the_filter_and_an_escape_sequence_does_not() {
        let hostile = "first line\u{1b}[2K\u{1b}[A\nsecond line";
        let out = indented(hostile);

        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "{out:?} reaches a terminal"
        );
        assert_eq!(
            out.lines().count(),
            2,
            "the deliberate break between sentences is not the filter's business: {out:?}"
        );
        assert!(out.starts_with("first line"));
        assert!(out.trim_end().ends_with("second line"));
    }

    fn people(names: &[&str]) -> Vec<User> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| User {
                pk: Pk::new(i as u64 + 1),
                username: (*name).into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            })
            .collect()
    }

    #[test]
    fn nobody_is_not_a_sentence() {
        assert_eq!(name_a_few(&[], 3), None);
    }

    /// This line opens `snob scan` with no flag asked for, so a username is
    /// the shortest route from somebody else's profile to the terminal.
    #[test]
    fn a_hostile_name_cannot_drive_the_terminal() {
        let hostile = people(&["ana\u{1b}[2K", "lu\u{202e}is"]);
        let line = name_a_few(&hostile, 3).unwrap();
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert_eq!(line, "@ana[2K and @luis");
    }

    #[test]
    fn one_name_stands_alone() {
        assert_eq!(name_a_few(&people(&["ana"]), 3).unwrap(), "@ana");
    }

    #[test]
    fn the_last_one_joins_with_and() {
        assert_eq!(
            name_a_few(&people(&["ana", "luis"]), 3).unwrap(),
            "@ana and @luis"
        );
        assert_eq!(
            name_a_few(&people(&["ana", "luis", "eva"]), 3).unwrap(),
            "@ana, @luis and @eva"
        );
    }

    /// The count is the last item of the one list, not a second list after it.
    #[test]
    fn past_the_cap_the_rest_are_counted() {
        let five = people(&["ana", "luis", "eva", "juan", "sara"]);
        assert_eq!(
            name_a_few(&five, 3).unwrap(),
            "@ana, @luis, @eva and 2 others"
        );
        assert_eq!(
            name_a_few(&five, 4).unwrap(),
            "@ana, @luis, @eva, @juan and 1 other"
        );
        // Exactly at the cap nothing is left over to count.
        assert_eq!(
            name_a_few(&five, 5).unwrap(),
            "@ana, @luis, @eva, @juan and @sara"
        );
    }

    /// A run somebody stopped gets no label, and is filtered all the same. The
    /// refusal below is the one a scheduled run raises, and the name in it
    /// comes from `watch.toml`, where nothing validates a username.
    #[test]
    fn a_stopped_run_is_filtered_even_though_it_carries_no_label() {
        let hostile = "gh\u{1b}[2K\u{1b}[A";
        let error: anyhow::Error = ExitError::new(
            ExitCode::Interrupted,
            format!(
                "reading @{hostile}'s lists needs confirmation, and a scheduled run has \
                 nobody to ask.\nRun \"snob watch setup\" to answer it once."
            ),
        )
        .into();

        let out = rendered(&error);

        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "{out:?} reaches a terminal"
        );
        assert!(out.contains("@gh"), "the name is still shown: {out}");
        assert!(
            !out.contains("error:"),
            "stopping a run is not a failure to report: {out}"
        );
        // No label above it, so the second sentence stays at column zero.
        let mut lines = out.lines();
        assert!(lines.next().is_some_and(|l| l.starts_with("reading @gh")));
        assert_eq!(
            lines.next(),
            Some("Run \"snob watch setup\" to answer it once.")
        );
        assert_eq!(lines.next(), None);
    }

    /// An account name reaches the cooldown refusal without anybody typing it:
    /// `Watched::list_args` puts the one from `watch.toml` straight into
    /// `ListArgs.target`, and nothing validates a username there.
    #[test]
    fn a_cooldown_refusal_cannot_be_made_to_erase_the_line_above_it() {
        let name = "gh\u{1b}[2K\u{1b}[A";
        let error = refuse_in_cooldown(&own_cooldown(1_000), Blocked::AccountUnknown(name));
        let message = error.to_string();

        assert!(
            !message.chars().any(|c| c.is_control()),
            "{message:?} is printed to a terminal"
        );
        assert!(
            message.contains("@gh"),
            "the name is still shown: {message}"
        );
    }

    /// A walk that stopped for `reason`, and what Instagram said about it when
    /// that is more specific than the reason.
    fn stopped(reason: StopReason, stopped_by: Option<ExitCode>) -> ListOutcome {
        ListOutcome {
            stopped_by,
            ..ListOutcome::for_test(Provenance::Walked, reason)
        }
    }

    /// A timestamp that makes no sense still has to read as something, because
    /// the alternative is a message with a hole in the middle of it.
    #[test]
    fn a_date_out_of_range_still_reads_as_something() {
        assert_eq!(stored_on(Epoch::new(i64::MAX)), "earlier");
        assert_eq!(cooldown_ends_at(EpochMs::new(i64::MAX)), "later");
        assert_eq!(
            format_epoch_in(Epoch::new(1_722_700_000), "earlier", &chrono::Utc),
            "Aug 3 at 15:46"
        );
        // The same moment in milliseconds reads as the same second, which is
        // what `EpochMs::to_epoch` is for.
        assert_eq!(
            format_epoch_in(
                EpochMs::new(1_722_700_000_000).to_epoch(),
                "later",
                &chrono::Utc
            ),
            "Aug 3 at 15:46"
        );
    }

    /// What is printed is the wall clock, not UTC with no label.
    #[test]
    fn a_moment_is_printed_in_the_zone_the_reader_is_in() {
        let madrid_in_august = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            format_epoch_in(Epoch::new(1_722_700_000), "earlier", &madrid_in_august),
            "Aug 3 at 17:46"
        );
    }

    /// A failure told to a program carries what the prose carries: the code
    /// the exit status names, the message, the advice set apart, and the
    /// address a challenge is cleared at -- filtered, since the consumer may
    /// print it.
    #[test]
    fn a_failure_in_json_carries_the_code_the_hint_and_the_address() {
        let hostile = "gh\u{1b}[2K";
        let error: anyhow::Error = anyhow::Error::new(snob_ig::error::IgError::Challenge {
            url: Some("https://www.instagram.com/challenge/".into()),
        })
        .context(format!("could not read @{hostile}'s followers list"));
        let json = error_json_for(&error, None);
        let error = &json["error"];

        assert_eq!(error["code"], "challenge");
        assert_eq!(error["exit"], 4);
        assert_eq!(error["message"], "could not read @gh[2K's followers list");
        assert_eq!(error["url"], "https://www.instagram.com/challenge/");
        assert_eq!(error["causes"].as_array().map(Vec::len), Some(1));

        let advised: anyhow::Error = ExitError::new(ExitCode::NoSession, "no session")
            .with_hint("run \"snob login\"")
            .into();
        let json = error_json_for(&advised, None);
        assert_eq!(json["error"]["code"], "no_session");
        assert_eq!(json["error"]["hint"], "run \"snob login\"");
        assert!(json["error"]["url"].is_null());
    }

    /// A failure names the account the run acted as, beside the error, so a
    /// script running several accounts can tell whose it was.
    #[test]
    fn a_failure_in_json_names_the_account_it_acted_as() {
        let error = anyhow::anyhow!("it failed");
        assert!(error_json_for(&error, None)["viewer"].is_null());

        let viewer = Viewer {
            pk: snob_core::Pk::new(42),
            username: Some("me\u{1b}[2K".into()),
        };
        let json = error_json_for(&error, Some(&viewer));
        assert_eq!(json["viewer"]["pk"], 42);
        assert_eq!(json["viewer"]["username"], "me[2K");
        assert_eq!(json["error"]["message"], "it failed");
    }

    /// The refusal has to name the mistake the caller would otherwise have
    /// made, and carry the code that says what to do about it.
    #[test]
    fn refusing_names_the_list_the_misreading_and_the_code() {
        let error = refuse_incomplete(
            ListKind::Followers,
            &stopped(StopReason::RateLimit, None),
            "they did not follow you",
        );
        let text = error.to_string();
        assert!(text.contains("followers list"), "{text}");
        assert!(text.contains("they did not follow you"), "{text}");
        // The advice lives on the hint, not in the sentence: see
        // `a_refusal_keeps_its_advice_apart_from_what_happened`.
        assert!(hint_of(&error).unwrap().contains("starts over"));

        assert_eq!(crate::exit::from_chain(&error), Some(ExitCode::RateLimited));
    }

    #[test]
    fn a_refusal_carries_the_code_of_whatever_stopped_the_walk() {
        for (reason, expected) in [
            (StopReason::Canceled, ExitCode::Interrupted),
            (StopReason::RateLimit, ExitCode::RateLimited),
            (StopReason::SessionInvalid, ExitCode::NoSession),
            (StopReason::Truncated, ExitCode::Error),
        ] {
            // Nothing more specific than the stop reason, so the code comes off
            // the reason — which is `ListOutcome::exit_code`'s fallback.
            let error = refuse_incomplete(ListKind::Following, &stopped(reason, None), "whatever");
            assert_eq!(
                crate::exit::from_chain(&error),
                Some(expected),
                "{reason:?}"
            );
        }
    }

    /// `SessionInvalid` is what the store records for both "log in again" and
    /// "Instagram wants the account verified", and those are different codes.
    /// The caller is allowed to know better than the stop reason does.
    #[test]
    fn a_challenge_keeps_its_own_code_under_a_session_invalid_stop() {
        let error = refuse_incomplete(
            ListKind::Followers,
            &stopped(StopReason::SessionInvalid, Some(ExitCode::Challenge)),
            "whatever",
        );
        assert_eq!(crate::exit::from_chain(&error), Some(ExitCode::Challenge));
    }

    /// Against an account Instagram has just flagged, "run it again" is the
    /// one piece of advice that cannot help and can make it worse.
    #[test]
    fn a_dead_session_is_not_told_to_try_again() {
        // Both ways: a checkpoint can land mid-pagination with a cursor stored,
        // and continuing is still the wrong thing to offer.
        for resumable in [false, true] {
            let advice = try_again_advice(StopReason::SessionInvalid, resumable);
            assert!(!advice.contains("continue where it left off"), "{advice}");
            assert!(advice.contains("Instagram asked for"), "{advice}");
        }
    }

    fn hint_of(error: &anyhow::Error) -> Option<String> {
        error
            .chain()
            .find_map(|c| c.downcast_ref::<ExitError>())
            .and_then(ExitError::hint)
            .map(str::to_string)
    }

    /// The advice is separate from the failure, so the printer can label them
    /// differently and the advice does not read as more of the complaint.
    #[test]
    fn a_refusal_keeps_its_advice_apart_from_what_happened() {
        let error = refuse_incomplete(
            ListKind::Followers,
            &stopped(StopReason::RateLimit, None),
            "they did not follow you",
        );
        assert!(error.to_string().contains("would be wrong"), "{error}");
        assert!(
            !error.to_string().contains("Run it again"),
            "the advice is not part of what happened: {error}"
        );
        assert!(hint_of(&error).unwrap().contains("starts over"));
    }

    /// A cooldown is waited out and the other two causes are not, so the same
    /// refusal owes them different advice. Telling somebody to sit out a
    /// cooldown they are not in is worse than saying nothing.
    #[test]
    fn different_moments_are_explained_by_why_nobody_checked() {
        let throttled = refuse_different_moments(
            Provenance::Cooldown,
            Provenance::Cooldown,
            Epoch::new(0),
            Epoch::new(1),
        );
        assert!(hint_of(&throttled).unwrap().contains("cooldown lifts"));
        assert_eq!(
            crate::exit::from_chain(&throttled),
            Some(ExitCode::RateLimited)
        );

        let asked_for = refuse_different_moments(
            Provenance::CacheFlag,
            Provenance::CacheFlag,
            Epoch::new(0),
            Epoch::new(1),
        );
        let hint = hint_of(&asked_for).unwrap();
        assert!(hint.contains("--offline"), "{hint}");
        assert!(!hint.contains("cooldown"), "{hint}");
        assert_eq!(crate::exit::from_chain(&asked_for), Some(ExitCode::Error));

        // A failed poll is neither of the two. Nobody asked for storage — the
        // request to check went out and did not come back — so it is not told
        // to drop a flag it never passed.
        let nobody_could_check = refuse_different_moments(
            Provenance::PollFailed,
            Provenance::PollFailed,
            Epoch::new(0),
            Epoch::new(1),
        );
        let hint = hint_of(&nobody_could_check).unwrap();
        assert!(!hint.contains("--offline"), "{hint}");
        assert!(!hint.contains("cooldown"), "{hint}");
        assert_eq!(
            crate::exit::from_chain(&nobody_could_check),
            Some(ExitCode::Error)
        );

        // All three say the same thing about what happened; only the advice
        // differs.
        for error in [&throttled, &asked_for, &nobody_could_check] {
            assert!(error.to_string().contains("different moments"), "{error}");
        }
    }

    /// Every way a cooldown can leave the user with nothing says which one it
    /// was, and all of them carry the throttling code.
    #[test]
    fn a_cooldown_refusal_names_what_is_missing() {
        let cases = [
            (Blocked::RefreshWanted, "--refresh"),
            (Blocked::AccountUnknown("someone"), "@someone"),
            (Blocked::NothingStored(ListKind::Followers), "followers"),
        ];
        for (blocked, expected) in cases {
            let error = refuse_in_cooldown(&own_cooldown(1_722_700_000_000), blocked);
            let text = error.to_string();
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("in cooldown until"), "{text}");
            assert_eq!(crate::exit::from_chain(&error), Some(ExitCode::RateLimited));
        }
    }

    fn own_cooldown(until_ms: i64) -> Held {
        Held {
            until_ms: EpochMs::new(until_ms),
            braked: Vec::new(),
        }
    }

    #[test]
    fn names_are_joined_as_a_sentence_says_them() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(and_list(&[]), None);
        assert_eq!(and_list(&names(&["@a"])).as_deref(), Some("@a"));
        assert_eq!(
            and_list(&names(&["@a", "@b"])).as_deref(),
            Some("@a and @b")
        );
        assert_eq!(
            and_list(&names(&["@a", "@b", "@c"])).as_deref(),
            Some("@a, @b and @c")
        );
    }

    /// The brake says it is every account, and names the ones pushed back on,
    /// with the throttling code.
    #[test]
    fn the_brake_names_the_accounts_behind_it() {
        let held = Held {
            until_ms: EpochMs::new(1_722_700_000_000),
            braked: vec!["@one".to_string(), "@two".to_string()],
        };
        let error = refuse_in_cooldown(&held, Blocked::NothingStored(ListKind::Followers));
        let text = error.to_string();
        assert!(text.starts_with("every account is paused until"), "{text}");
        assert!(
            text.contains("pushed back on @one and @two within an hour"),
            "{text}"
        );
        assert_eq!(crate::exit::from_chain(&error), Some(ExitCode::RateLimited));

        let three = ["@a", "@b", "@c"].map(String::from);
        assert!(
            brake_sentence(held.until_ms, &three).contains("@a, @b and @c"),
            "three accounts are listed as a sentence would"
        );
    }

    /// A refusal is several sentences, and the ones after the first sit under
    /// the label rather than at column zero, where they would read as separate
    /// unattributed text.
    #[test]
    fn continuation_lines_sit_under_the_label() {
        let indented = indented(
            "first
second
third",
        );
        assert_eq!(
            indented,
            "first
       second
       third"
        );
        // A single line is left exactly as it was.
        assert_eq!(super::indented("only one"), "only one");
    }

    /// Filters that took nothing out must not leave a clause saying they did.
    #[test]
    fn the_count_only_mentions_filtering_when_something_was_filtered() {
        assert_eq!(
            counted(3, 3, 3, "unfollower", "unfollowers"),
            "3 unfollowers"
        );
        assert_eq!(
            counted(3, 3, 10, "unfollower", "unfollowers"),
            "3 unfollowers (of 10, the rest filtered out)"
        );
    }

    /// A cap is not a filter: `--limit 3` on ten unfollowers with no filters
    /// at all does not report the other seven as filtered out.
    #[test]
    fn a_cap_is_reported_as_a_cap() {
        assert_eq!(
            counted(3, 10, 10, "unfollower", "unfollowers"),
            "3 unfollowers (of 10, trimmed by --limit)"
        );
        // And when both happened, both are named.
        assert_eq!(
            counted(2, 4, 10, "unfollower", "unfollowers"),
            "2 unfollowers (of 10: 6 filtered out, the rest trimmed by --limit)"
        );
    }

    /// A filtered list lands on one result often enough that the singular is
    /// the summary users see most.
    #[test]
    fn a_count_of_one_reads_as_one() {
        assert_eq!(
            counted(1, 1, 1, "unfollower", "unfollowers"),
            "1 unfollower"
        );
        assert_eq!(
            counted(1, 1, 9, "unfollower", "unfollowers"),
            "1 unfollower (of 9, the rest filtered out)"
        );
        assert_eq!(
            counted(0, 0, 0, "unfollower", "unfollowers"),
            "0 unfollowers"
        );
    }

    #[test]
    fn the_request_count_is_pluralized() {
        assert_eq!(requests(0), "0 requests");
        assert_eq!(requests(1), "1 request");
        assert_eq!(requests(2), "2 requests");
    }

    #[test]
    fn the_advice_depends_on_the_stop_reason() {
        assert!(try_again_advice(StopReason::RateLimit, false).contains("starts over"));
        // These stop mid-pagination, so the store has a cursor.
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
            StopReason::RateLimit,
        ] {
            assert!(try_again_advice(reason, true).contains("continue where it left off"));
        }
    }

    /// The two ways this module offers a continuation. Both are matched,
    /// because saying "nothing to continue from" also contains the word.
    fn offers_a_continuation(advice: &str) -> bool {
        advice.contains("continue where it left off") || advice.contains("continues from")
    }

    /// The offer to continue follows what the store kept, not what the reason
    /// suggests.
    ///
    /// Every reason here can arrive either way. A `Canceled` walk normally has
    /// a cursor, but one interrupted more than a day and a half after it began
    /// has already aged out of the resume window — and the advice must not
    /// offer a continuation the next run cannot make.
    #[test]
    fn nothing_is_offered_to_continue_when_there_is_nothing_stored() {
        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Network,
            StopReason::Truncated,
            StopReason::RateLimit,
        ] {
            let advice = try_again_advice(reason, false);
            assert!(
                !offers_a_continuation(advice),
                "{reason:?} offered a continuation: {advice}"
            );
            assert!(advice.contains("starts over"), "{reason:?}: {advice}");
        }
    }

    /// The reclassified truncation is the one that really has nothing left.
    ///
    /// `verify_completion` reaches it **after** the pagination has ended, so the
    /// snapshot closes with no cursor and `snapshots::resumable` will not
    /// return a row without one. The four guards that stop in the middle of the
    /// pagination do leave one, which is why the reason alone cannot answer
    /// this and `resumable` is asked separately.
    #[test]
    fn a_truncated_walk_says_which_of_the_two_it_was() {
        let ended = try_again_advice(StopReason::Truncated, false);
        assert!(!offers_a_continuation(ended), "{ended}");
        assert!(ended.contains("starts over"), "{ended}");

        let stopped_short = try_again_advice(StopReason::Truncated, true);
        assert!(stopped_short.contains("continues from where it stopped"));
    }

    /// Advice that names a `snob` subcommand belongs to the binary, not to its
    /// HTTP client — and it still has to reach the person reading.
    ///
    /// Every path an `IgError` takes carries its advice: the labeled failure,
    /// the JSON shape, and the two places that print an `IgError` as one line.
    #[test]
    fn the_advice_an_ig_error_carried_still_reaches_the_reader() {
        use snob_ig::error::IgError;

        for (error, advice) in [
            (IgError::SessionExpired, "run \"snob login\" again"),
            (
                IgError::NoCsrfToken,
                "run \"snob login --browser\", or, with SNOB_NO_BROWSER, pass the token with \
                 \"snob login --paste --csrftoken\"",
            ),
        ] {
            // One line: the diagnosis, then the advice.
            assert_eq!(
                what_instagram_said(&error),
                format!("{error}; {advice}"),
                "the two halves have to rejoin where they were joined before"
            );

            let said = error.to_string();
            let error: anyhow::Error = anyhow::Error::new(error);
            let out = rendered(&error);
            assert!(out.contains(&said), "{out}");
            assert!(out.contains("hint:"), "{out}");
            assert!(out.contains(advice), "{out}");

            let json = error_json_for(&error, None);
            assert_eq!(json["error"]["hint"], advice);
            assert_eq!(json["error"]["message"], said);
        }

        // Everything else has nothing to advise, and an invented hint would be
        // worse than none.
        assert_eq!(advice_for(&IgError::TooManyRedirects), None);
        assert_eq!(
            what_instagram_said(&IgError::TooManyRedirects),
            IgError::TooManyRedirects.to_string()
        );
    }

    /// A cooldown the backstop met says when it lifts, in prose and in JSON.
    #[test]
    fn a_cooldown_from_the_client_says_when_it_lifts() {
        let until_ms = EpochMs::new(1_700_000_000_000);
        let error = anyhow::Error::new(snob_ig::error::IgError::InCooldown { until_ms });

        let out = rendered(&error);
        assert!(out.contains("hint:"), "{out}");
        assert!(out.contains("it lifts "), "{out}");

        let json = error_json_for(&error, None);
        assert_eq!(
            json["error"]["cooldown_until"],
            serde_json::json!(until_ms.to_epoch())
        );
    }

    /// One hint, whoever wrote it. A reader is being told what to do, and two
    /// answers to that is worse than either of them alone.
    #[test]
    fn a_failure_carries_one_piece_of_advice() {
        let from_the_client: anyhow::Error =
            anyhow::Error::new(snob_ig::error::IgError::SessionExpired);
        assert_eq!(rendered(&from_the_client).matches("hint:").count(), 1);

        // A command's own advice is the one that shows, and the client's is
        // not consulted. They cannot in fact meet — an `ExitError` carries no
        // source, so nothing of Instagram's is ever underneath one — but the
        // order is written down rather than left to that.
        let from_the_command: anyhow::Error =
            ExitError::new(ExitCode::NoSession, "no session is stored")
                .with_hint("run \"snob login\"")
                .into();
        let out = rendered(&from_the_command);
        assert_eq!(out.matches("hint:").count(), 1, "{out}");
        assert!(out.contains("run \"snob login\"\n"), "{out}");
    }

    /// The pager reports a condition and this is where it becomes a sentence,
    /// so this is where the sentence is asserted on.
    ///
    /// `pager.rs`'s own tests match on the variant, which is the right test
    /// over there and leaves the words untested unless something checks them
    /// here.
    #[test]
    fn every_warning_the_walk_raises_says_something() {
        assert!(
            pager_warning(Warning::EmptyAndNoCounter).contains("came back empty"),
            "{}",
            pager_warning(Warning::EmptyAndNoCounter)
        );
        for warning in [
            Warning::SameCursorTwice,
            Warning::TwoEmptyPages,
            Warning::GoingInCircles,
            Warning::EmptyAndNoCounter,
        ] {
            assert!(!pager_warning(warning).is_empty(), "{warning:?}");
        }

        // The three that carry numbers name both of them: a shortfall with
        // only one of its halves shown says nothing about how big it is.
        let short = pager_warning(Warning::StoppedShort {
            walked: 39,
            declared: 21_631,
        });
        assert!(short.contains("39") && short.contains("21631"), "{short}");

        let deleted = pager_warning(Warning::ShortOfDeclared {
            walked: 80,
            declared: 100,
        });
        assert!(
            deleted.contains("80") && deleted.contains("100"),
            "{deleted}"
        );

        let repeated = pager_warning(Warning::Repeated {
            repeated: 9,
            received: 150,
        });
        assert!(
            repeated.contains("9 of the 150") && repeated.contains("cannot be compared"),
            "{repeated}"
        );
    }

    /// Only a full walk has nothing to explain. Every other ending owes the
    /// user a reason, and a missing one would print an empty clause.
    #[test]
    fn every_incomplete_ending_has_something_to_say() {
        assert_eq!(why_incomplete(StopReason::Completed), None);
        for reason in StopReason::ALL {
            if reason != StopReason::Completed {
                assert!(why_incomplete(reason).is_some(), "{reason:?}");
            }
        }
    }
}
