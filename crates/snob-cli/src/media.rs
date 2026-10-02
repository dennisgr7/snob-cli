//! Stories and highlight items once they are read, and how they are kept:
//! the words a listing and a view both use for them, and the downloads from
//! the CDN. `stories`, `highlights`, `profile` and their views all share it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use comfy_table::{ContentArrangement, Table, presets};
use snob_core::Epoch;
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::error::IgError;

use crate::cli::{DownloadSelection, Format};
use crate::exit::{ExitCode, ExitError};
use crate::output::{self, Presentation, Rendered};
use crate::report;
use crate::ui;
use crate::ui::browser::scratch::ABANDONED_AFTER;

/// Ceiling on one downloaded story.
///
/// Higher than the profile-picture ceiling, and it has to be: a fifteen-second
/// story video at Instagram's own bitrate lands in the single-digit megabytes
/// and the 8 MB cap on `IgClient::download` refuses some of them. Sixty-four is
/// far above anything Instagram serves for a format capped at fifteen seconds,
/// so it is still a ceiling rather than a formality — its job is that a
/// redirect to something else cannot make this read until memory runs out.
pub const MAX_STORY_BYTES: usize = 64 * 1024 * 1024;

/// A photo or a video, as Instagram's `media_type` integer means it.
///
/// The integer stops here. Nothing downstream compares a number to 1 or 2, and
/// an unknown value is its own case rather than being folded into either — a
/// third kind arriving should read as "unknown" and be downloadable, not be
/// mislabeled as a photo and given a `.jpg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Photo,
    Video,
    Unknown,
}

impl Kind {
    pub(crate) fn of(media_type: u8) -> Self {
        match media_type {
            1 => Self::Photo,
            2 => Self::Video,
            _ => Self::Unknown,
        }
    }

    /// What the printed listings and the views write for it: different
    /// shapes, and the words in them are one decision.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Unknown => "unknown",
        }
    }
}

/// One story or highlight item, reduced to what a listing and a download need.
#[derive(Debug, Clone)]
pub struct Story {
    pub kind: Kind,
    pub taken_at: Epoch,
    pub expiring_at: Option<Epoch>,
    /// Where the best copy is. Absent when Instagram described a story and
    /// offered no version of it, which happens and must not be a crash.
    pub url: Option<String>,
    /// Accounts tagged in it, already filtered for a terminal.
    pub mentions: Vec<String>,
}

/// Everything the story listing needs, gathered before anything is printed.
#[derive(Debug, Clone)]
pub struct Stories {
    /// As Instagram spells it, not as it was typed.
    pub username: String,
    pub items: Vec<Story>,
}

/// Writes a listing and, at a terminal, the hint about what to do with it.
pub(crate) fn print_listing(
    mut text: String,
    destination: Option<&Path>,
    hint: &str,
) -> Result<ExitCode> {
    // Every rendering ends in a newline, like `output::render`'s do. Neither
    // `serde_json::to_string_pretty` nor comfy-table adds one, and without it
    // the shell prompt comes back glued to the last row.
    text.push('\n');
    output::write_rendered(&Rendered::Text(text), destination)?;

    // On standard error, so it does not land in a redirect. The listing is the
    // answer; this is the hint about what to do with it.
    if destination.is_none() && Presentation::detect(None).interactive {
        ui::info(hint);
    }
    Ok(ExitCode::Ok)
}

/// The table a listing is drawn in, with its header and nothing else yet.
pub(crate) fn listing_table(headers: &[&str], presentation: Presentation) -> Table {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = presentation.width {
        table.set_width(width);
    }
    table.set_header(output::table::header_cells(headers, presentation));
    if presentation.color {
        table.enforce_styling();
    }
    table
}

/// A listing's rows as JSON: NDJSON is the rows alone, one to a line, and
/// JSON is the document `envelope` wraps them in.
pub(crate) fn rows_or_envelope(
    rows: Vec<serde_json::Value>,
    format: Format,
    envelope: impl FnOnce(Vec<serde_json::Value>) -> serde_json::Value,
) -> Result<String> {
    Ok(match format {
        Format::Ndjson => rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        _ => serde_json::to_string_pretty(&envelope(rows))?,
    })
}

/// The accounts a story tags, `@a @b`, for a table cell. Already filtered
/// when the story was read, because they came off somebody else's profile.
pub(crate) fn mentions_of(story: &Story) -> String {
    story
        .mentions
        .iter()
        .map(|m| format!("@{m}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An empty tray is still an answer, and a document asked for is written
/// even when there is nothing in it: `{"username": …, "stories": []}`, the
/// way an empty list command prints `[]`, so a script reading JSON from a
/// pipe has something to parse. NDJSON with no rows is
/// already nothing, and a table has nothing to draw; the sentence on
/// standard error says what happened in both.
pub(crate) fn empty_document(
    format: Format,
    destination: Option<&Path>,
    json: impl FnOnce() -> Result<String>,
) -> Result<()> {
    if format != Format::Json {
        return Ok(());
    }
    let mut text = json()?;
    text.push('\n');
    output::write_rendered(&Rendered::Text(text), destination)
}

/// When a story was posted, and what is left of it: the two time columns of
/// the printed listing and of the browser, read off the same clock code.
pub(crate) fn posted_of(story: &Story) -> String {
    report::stored_on(story.taken_at)
}

pub(crate) fn left_of(story: &Story) -> String {
    remaining(story.expiring_at)
}

/// How long a story has left, or nothing when Instagram did not say.
///
/// An absent `expiring_at` prints as a dash rather than as an expired story:
/// zero would be read as "gone", which is a statement, and what is true is that
/// nobody said.
fn remaining(expiring_at: Option<Epoch>) -> String {
    let Some(at) = expiring_at else {
        return "-".into();
    };
    countdown(at - snob_core::clock::now())
}

/// "7h 23m", "12m", "expired".
///
/// Not `snob_core::duration::format`, which exists to write a duration back
/// into the monitor's configuration file the way somebody would have typed it
/// — so it uses the largest unit that divides **exactly** and falls back to
/// seconds. A story with seven hours and twenty-three minutes left divides
/// exactly by nothing, and would have printed as `26580s`.
fn countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "expired".into();
    }
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    match (hours, minutes) {
        (0, 0) => "under a minute".into(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Turns a parsed selection into item numbers and refuses any the listing
/// does not have -- before the first request, so `-d 3,9` against a listing
/// of five downloads nothing rather than three items and an error.
pub(crate) fn numbers_of(
    selection: DownloadSelection,
    len: usize,
    no_such: impl Fn(usize) -> anyhow::Error,
) -> Result<Vec<usize>> {
    let numbers: Vec<usize> = match selection {
        DownloadSelection::All => (1..=len).collect(),
        DownloadSelection::These(numbers) => numbers,
    };
    match numbers.iter().find(|&&number| number > len) {
        Some(&number) => Err(no_such(number)),
        None => Ok(numbers),
    }
}

/// Downloads items of one listing by the numbers it printed.
///
/// One number keeps the single-download contract: `-o` names a file and a
/// failure is the run's failure; without `-o` the file lands in the working
/// directory under the listing's name, and one already there is an answer
/// rather than a second copy. Several go through [`download_many`]: `-o`
/// names a directory and the loop keeps going past one that fails.
///
/// `stem_base` is what comes before the number in a file name. `no_such` is
/// the listing's sentence for a number it does not have, the one sentence
/// for both paths so they cannot drift.
pub(crate) async fn download_selected(
    client: std::sync::Arc<IgClient>,
    stem_base: String,
    items: &[Story],
    selection: DownloadSelection,
    destination: Option<&Path>,
    no_such: impl Fn(usize) -> anyhow::Error,
) -> Result<ExitCode> {
    let numbers = numbers_of(selection, items.len(), &no_such)?;
    let [number] = numbers[..] else {
        return download_many(client, stem_base, items.to_vec(), numbers, destination).await;
    };
    // One-based because that is what the listing shows. Zero is its own
    // message rather than an underflow.
    let story = number
        .checked_sub(1)
        .and_then(|i| items.get(i))
        .ok_or_else(|| no_such(number))?;
    match destination {
        // The user named it, so replacing what is there is their call -- and
        // so is downloading it again. Held whole: a named destination may be
        // anywhere.
        Some(path) => {
            let bytes = bytes_of(&client, story).await?;
            output::write_bytes(&bytes, Some(path))?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None => match save_story(&client, &stem_base, items, number, Path::new(".")).await? {
            Saved::Now(path) => ui::info(&format!("Saved {}", path.display())),
            Saved::Already(path) => ui::info(&format!("Already saved {}", path.display())),
        },
    }
    Ok(ExitCode::Ok)
}

/// What [`save_story`] found: the file it wrote, or the one already there.
pub(crate) enum Saved {
    Now(PathBuf),
    Already(PathBuf),
}

/// Saves one story under the name the listing implies, to disk as it arrives.
///
/// **Nothing is fetched for a story already on disk.** The name depends on
/// the extension and the extension is read from the bytes, so asking
/// `default_name` would mean downloading the whole story first -- on a second
/// `-d all`, every story in the tray fetched and thrown away. The four
/// extensions `extension_of` can answer are tried first; a hit is "already
/// saved" and costs nothing.
///
/// That look is an optimization in front of the atomic check, not a
/// replacement for it: the write still goes through `output::create_new`,
/// which refuses a name that appeared between the look and the open. The
/// doc on `output::write_new` says why the creation has to be the check.
///
/// **The file is written to `<stem>.<pid>.part` and renamed at the end.** The
/// bytes stream to disk through `IgClient::download_to`, and a stream that
/// stops halfway must not leave a truncated file under the real name, where
/// the look above would take it for a finished one. A `.part` is removed on
/// any failure here; one left by a killed process is removed by age the next
/// time the same story is asked for.
///
/// `stem_base` is what comes before the number in the file name: the username
/// for a story, the username and the highlight's number for a highlight item.
/// Filtered here, because it carries a name that came off the server.
pub(crate) async fn save_story(
    client: &IgClient,
    stem_base: &str,
    items: &[Story],
    number: usize,
    dir: &Path,
) -> Result<Saved> {
    let stem = format!("{}-{number}", printable(stem_base));
    let url = items[number - 1].url.as_deref();
    save_named(
        client,
        &stem,
        url,
        &format!("story {number}"),
        MAX_STORY_BYTES,
        dir,
    )
    .await
}

/// [`save_story`] for any file of the CDN: `url` saved into `dir` as
/// `<stem>.<extension>`, the extension read from the bytes, by the same
/// steps and for the same reasons. `stem` is already filtered and is the
/// whole name but its extension; `what` names the file in the sentence for
/// one that offers no address; `cap` is the ceiling on its size.
pub(crate) async fn save_named(
    client: &IgClient,
    stem: &str,
    url: Option<&str>,
    what: &str,
    cap: usize,
    dir: &Path,
) -> Result<Saved> {
    if let Some(existing) = already_saved(dir, stem) {
        return Ok(Saved::Already(existing));
    }
    let url = url.ok_or_else(|| anyhow!("{what} has no downloadable version"))?;

    // The scratch name carries the process id, so two runs saving into the
    // same directory never share one: neither can unlink the other's
    // half-written download, nor publish the other's truncated bytes under
    // the final name with its rename. `create_new` is the only arbiter for
    // this process's own name; a `.part` a killed run abandoned is cleared by
    // age instead.
    sweep_stale_parts(dir, stem);
    let part = dir.join(format!("{stem}.{}.part", std::process::id()));
    let mut file = output::create_new(&part)?;
    let downloaded = match client.download_to(url, cap, &mut file).await {
        Ok(downloaded) => downloaded,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(e.into());
        }
    };
    file.sync_all()
        .with_context(|| format!("could not finish writing {}", part.display()))?;
    drop(file);

    // `default_name` answers a bare name and checks the directory only for a
    // collision; joined here, or `-o somewhere` made the directory and the
    // file landed in the working directory.
    let name = match output::default_path(dir, stem, extension_of(&downloaded.head)) {
        Ok(name) => name,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    let path = dir.join(name);
    if let Err(e) = std::fs::rename(&part, &path) {
        let _ = std::fs::remove_file(&part);
        return Err(
            anyhow::Error::new(e).context(format!("could not move into place {}", path.display()))
        );
    }
    Ok(Saved::Now(path))
}

/// Removes `.part` files for this stem that no live download can still own.
///
/// A `.part` younger than [`ABANDONED_AFTER`] may be a sibling process still
/// streaming -- its mtime moves with every written chunk -- and is left
/// alone. Failures are ignored: this is housekeeping, and the story saves
/// either way.
fn sweep_stale_parts(dir: &Path, stem: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // `user-3.part` and `user-3.<pid>.part` both belong to this stem;
        // `user-33.part` does not, which is what the dot after the stem asks.
        let ours = name
            .strip_prefix(stem)
            .is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".part"));
        if !ours {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > ABANDONED_AFTER)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The file a story with this stem was saved to earlier, if any extension
/// `extension_of` can produce is already there.
fn already_saved(dir: &Path, stem: &str) -> Option<PathBuf> {
    ["mp4", "webp", "png", "jpg"]
        .into_iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|candidate| candidate.is_file())
}

/// Downloads all of them into a directory.
///
/// Keeps going past one that fails, and says so at the end. Stopping at the
/// first would leave the user with a partial set and no idea which ones are
/// missing — and a story URL expiring mid-run is the ordinary case here, not
/// the exceptional one. That covers the write as well as the fetch: a name
/// already taken in the directory is the ordinary case on a second run, and
/// must not stop the loop at story one with the rest never attempted.
///
/// A Ctrl+C is the one failure that is not collected. Every fetch after it
/// answers `Canceled` at once, so carrying on would count them all as
/// failures and exit 1 for what the user did on purpose.
pub(crate) async fn download_many(
    client: std::sync::Arc<IgClient>,
    stem_base: String,
    items: Vec<Story>,
    numbers: Vec<usize>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let files = numbers
        .iter()
        .map(|&number| Named {
            number,
            stem: format!("{}-{number}", printable(&stem_base)),
            url: items[number - 1].url.clone(),
            what: format!("story {number}"),
            cap: MAX_STORY_BYTES,
        })
        .collect();
    download_named(client, files, destination, "stories").await
}

/// One file of a [`download_named`]: the number it is reported under, the
/// name it is saved as (filtered, without its extension), where it is, what
/// a sentence calls it, and the ceiling on its size.
#[derive(Debug, Clone)]
pub(crate) struct Named {
    pub number: usize,
    pub stem: String,
    pub url: Option<String>,
    pub what: String,
    pub cap: usize,
}

/// [`download_many`] for any set of files of the CDN, each under the name it
/// carries: into `destination`, three at a time, past the ones that fail,
/// said at the end as so many of the `noun`.
pub(crate) async fn download_named(
    client: std::sync::Arc<IgClient>,
    files: Vec<Named>,
    destination: Option<&Path>,
    noun: &str,
) -> Result<ExitCode> {
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let dir = destination.unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    // Several in flight rather than one after another. The CDN is a different
    // host with its own limits and is deliberately not paced (the reasoning
    // is on `IgClient::download_capped`), the client to it carries nothing
    // that names the account, and the connection is HTTP/2 -- so three
    // stories share one TCP+TLS connection instead of each waiting its own
    // round trip. Three is what a browser does when it opens a tray, and
    // past it a home link is the limit, not the latency.
    let dir = std::sync::Arc::new(dir.to_path_buf());
    let slots = std::sync::Arc::new(Semaphore::new(STORY_DOWNLOADS_IN_FLIGHT));
    let total = files.len();
    let mut tasks = JoinSet::new();
    for file in files {
        let (client, dir, slots) = (
            std::sync::Arc::clone(&client),
            std::sync::Arc::clone(&dir),
            std::sync::Arc::clone(&slots),
        );
        tasks.spawn(async move {
            // A closed semaphore is impossible here -- nothing closes it --
            // so the only way this fails is the runtime shutting down, and
            // then there is nobody to report to.
            let _slot = slots.acquire_owned().await.ok()?;
            let saved = save_named(
                &client,
                &file.stem,
                file.url.as_deref(),
                &file.what,
                file.cap,
                &dir,
            )
            .await;
            Some((file.number, saved))
        });
    }

    let mut failed: Vec<(usize, String)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok(Some((number, saved))) = joined else {
            // A task that panicked or was aborted: the runtime is going
            // down, or Ctrl+C already won below. Nothing to add.
            continue;
        };
        match saved {
            // Numbered, because they finish in whichever order the network
            // decides and the listing's numbers are what the user reads.
            Ok(Saved::Now(path)) => ui::info(&format!("Saved {number}: {}", path.display())),
            Ok(Saved::Already(path)) => {
                ui::info(&format!("Already saved {number}: {}", path.display()));
            }
            Err(e) if was_canceled(&e) => {
                // The token is shared, so every download still running has
                // already answered `Canceled` or is about to; aborting only
                // stops their reports from arriving after "stopped".
                tasks.abort_all();
                return Err(ExitError::new(ExitCode::Interrupted, "stopped").into());
            }
            Err(e) => failed.push((number, e.to_string())),
        }
    }

    if failed.is_empty() {
        return Ok(ExitCode::Ok);
    }
    failed.sort_by_key(|(number, _)| *number);
    let lines: Vec<String> = failed
        .iter()
        .map(|(number, why)| format!("{number}: {why}"))
        .collect();
    Err(ExitError::new(
        ExitCode::Error,
        format!(
            "{} of {} {noun} could not be downloaded:\n{}",
            failed.len(),
            total,
            lines.join("\n")
        ),
    )
    .into())
}

/// How many stories download at once under `-d all` or a set.
///
/// See the comment in [`download_many`]. Not configurable: a flag would be a
/// way to go faster, and this program's knobs only ever turn the other way.
const STORY_DOWNLOADS_IN_FLIGHT: usize = 3;

/// Whether a failed download was the user stopping it.
pub(crate) fn was_canceled(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| matches!(cause.downcast_ref::<IgError>(), Some(IgError::Canceled)))
}

/// The bytes of one story, from the CDN.
pub(crate) async fn bytes_of(client: &IgClient, story: &Story) -> Result<Vec<u8>> {
    let url = story
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("Instagram described this story but offered no media for it"))?;
    Ok(client.download_capped(url, MAX_STORY_BYTES).await?)
}

/// What arrived, read from the bytes rather than from the URL.
///
/// The URL is no guide: Instagram's signed links carry `stp=dst-jpg`, an
/// instruction to the CDN to convert, so a path ending in `.webp` regularly
/// returns JPEG. The one sniffer -- `pfp::Picture::extension` reads through
/// it too. MP4's `ftyp` box sits at offset four, after the box length, and
/// JPEG is both the common case and the sensible guess for anything
/// unrecognizable: it is what Instagram serves almost everywhere.
pub(crate) fn extension_of(bytes: &[u8]) -> &'static str {
    let webp = bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP");
    match bytes {
        _ if bytes.get(4..8) == Some(b"ftyp") => "mp4",
        _ if webp => "webp",
        [0x89, b'P', b'N', b'G', ..] => "png",
        _ => "jpg",
    }
}

/// `someone-3.mp4`, next to whatever is already there.
pub(crate) fn default_name(
    dir: &Path,
    username: &str,
    number: usize,
    extension: &str,
) -> Result<PathBuf> {
    output::default_path(dir, &format!("{}-{number}", printable(username)), extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_comes_from_the_bytes() {
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        assert_eq!(extension_of(&mp4), "mp4");
        assert_eq!(extension_of(b"\x89PNG\r\n\x1a\n"), "png");
        assert_eq!(extension_of(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(extension_of(b"\xff\xd8\xff\xe0anything"), "jpg");
        // A PNG signature cut short after five bytes is still a PNG, and
        // unrecognizable bytes get the usual case rather than no name at all.
        assert_eq!(extension_of(&[0x89, b'P', b'N', b'G', 0x0D]), "png");
        assert_eq!(extension_of(b"???"), "jpg");
    }

    #[test]
    fn an_unknown_expiry_is_not_reported_as_expired() {
        assert_eq!(remaining(None), "-");
    }
}
