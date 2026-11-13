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
use snob_ig::model::PictureVersion;
use snob_ig::model::dash::Track;

use crate::cli::{DownloadSelection, Format};
use crate::exit::{ExitCode, ExitError};
use crate::ffmpeg::Ffmpeg;
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
    /// A small picture of it for a view to draw: a photo's own, a video's
    /// cover ([`thumb_among`]). Not in the JSON contract.
    pub thumb: Option<String>,
}

/// The narrowest picture at least this wide is a thumbnail. The views draw
/// it in real pixels where the terminal can (`ui::graphics`), and the panel
/// beside a post's text reaches some 600 pixels wide, so a picture of 320
/// was drawn blown up and looked pixelated (Dennis, 2026-11-13); 640 fills
/// it and is still a fraction of the full picture's bytes.
const THUMB_WIDTH: u32 = 640;

/// The picture a view draws small, of the `candidates` Instagram offers for
/// one item (a video's are its cover): the narrowest at least
/// [`THUMB_WIDTH`] wide, and when none is, the widest; on a tie, the one
/// listed first. The one rule for posts, stories and highlights.
pub(crate) fn thumb_among(candidates: &[PictureVersion]) -> Option<String> {
    let width = |v: &&PictureVersion| v.width.unwrap_or(0);
    candidates
        .iter()
        .filter(|v| width(v) >= THUMB_WIDTH)
        .min_by_key(width)
        .or_else(|| candidates.iter().rev().max_by_key(width))
        .map(|v| v.url.clone())
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
    table.load_style(presets::UTF8_FULL_CONDENSED);
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

/// The most one small picture may weigh ([`small_pictures`]): half a
/// megabyte. A thumbnail is the narrowest picture at least [`THUMB_WIDTH`]
/// wide, some tens of kilobytes; where Instagram offered nothing that
/// narrow it is the widest, and one past this is left out, not cut.
pub(crate) const MAX_SMALL_PICTURE: usize = 512 * 1024;

/// The most the small pictures of one listing weigh together: four
/// megabytes, three pages of a grid at a hundred kilobytes a picture. They
/// travel in the listing's JSON and from there into a model's context,
/// which is no place for more.
pub(crate) const MOST_SMALL_PICTURES: usize = 4 * 1024 * 1024;

/// A small picture of each of `wanted`, a listing's things by their number
/// in it and the address of each one's thumbnail (a video's is its cover),
/// for a reader that looks rather than saves: `snob mcp`, which hands them
/// to a model as images (`--small-pictures`). Each is `{of, mime, bytes,
/// data}`, `data` the picture in base64, or `{of, missing}` saying why
/// there is none: nothing was offered, it was past a ceiling, it could not
/// be fetched, or what came is no picture.
///
/// **In the JSON, never on standard output as bytes**, and never an
/// address: what is handed on is the picture, which names nobody and
/// expires nowhere. Fetched from the CDN as every thumbnail a view draws
/// is, one after another; nothing here is a request to Instagram, and
/// nothing tells anybody what was looked at.
pub(crate) async fn small_pictures(
    client: &IgClient,
    wanted: Vec<(usize, Option<String>)>,
) -> Result<Vec<serde_json::Value>> {
    use base64::Engine as _;

    let mut pictures = Vec::with_capacity(wanted.len());
    let mut total = 0usize;
    for (of, address) in wanted {
        let missing = |why: &str| serde_json::json!({ "of": of, "missing": why });
        let Some(address) = address else {
            pictures.push(missing("none is offered"));
            continue;
        };
        if total >= MOST_SMALL_PICTURES {
            pictures.push(missing("past what one answer carries"));
            continue;
        }
        match client.download_capped(&address, MAX_SMALL_PICTURE).await {
            Ok(bytes) => match picture_type(&bytes) {
                Some(mime) => {
                    total += bytes.len();
                    pictures.push(serde_json::json!({
                        "of": of,
                        "mime": mime,
                        "bytes": bytes.len(),
                        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }));
                }
                None => pictures.push(missing("what came is not a picture")),
            },
            Err(IgError::Canceled) => {
                return Err(ExitError::new(ExitCode::Interrupted, "stopped").into());
            }
            // Not the error's own words: they can hold the address.
            Err(_) => pictures.push(missing("it could not be fetched")),
        }
    }
    Ok(pictures)
}

/// What kind of picture `bytes` are, by how they start: the three kinds
/// Instagram serves. `None` for anything else, a video among them.
fn picture_type(bytes: &[u8]) -> Option<&'static str> {
    let webp = bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP");
    match bytes {
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        _ if webp => Some("image/webp"),
        _ => None,
    }
}

/// A listing's JSON document with `pictures` beside what it lists
/// ([`small_pictures`]). Without any, and for NDJSON, whose lines are the
/// rows alone, the listing as it came.
pub(crate) fn with_pictures(
    text: String,
    pictures: Option<Vec<serde_json::Value>>,
) -> Result<String> {
    let Some(pictures) = pictures else {
        return Ok(text);
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(mut document)) => {
            document.insert("pictures".into(), serde_json::Value::Array(pictures));
            Ok(serde_json::to_string_pretty(&document)?)
        }
        _ => Ok(text),
    }
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
/// One number keeps the single-download contract: `-o` names a file (or a
/// directory that exists, which it is saved into) and a failure is the run's
/// failure; without `-o` the file lands in the working
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
    let to_stdout = destination.is_some_and(output::is_stdout);
    if to_stdout {
        output::one_file_to_stdout(numbers.len())?;
    }
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
        // `-o -`: the file itself, and nothing said about where it went.
        Some(_) if to_stdout => {
            let bytes = bytes_of(&client, story).await?;
            output::write_bytes(&bytes, None)?;
        }
        // A directory that is there already: the file goes into it under
        // the listing's name, as several would. Taken as a file name, it
        // failed with "Is a directory" after the story had been fetched.
        Some(dir) if dir.is_dir() => {
            match save_story(&client, &stem_base, items, number, dir).await? {
                Saved::Now(path) => ui::info(&format!("Saved {}", path.display())),
                Saved::Already(path) => ui::info(&format!("Already saved {}", path.display())),
            }
        }
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
///
/// **The stem passes `output::usable_stem` before anything is made of it.**
/// `printable` leaves a `..`, a separator and a share name as they are, and
/// the look for an earlier copy and the `.part` are paths built from the
/// stem: checked only by `default_path` at the end, a name off the server
/// such as `../x` was probed and streamed to outside `dir`, and on Windows
/// one such as `\\host\share\x` reached for that host over SMB before the
/// refusal.
/// Refused here, nothing is looked at, created or sent.
pub(crate) async fn save_named(
    client: &IgClient,
    stem: &str,
    url: Option<&str>,
    what: &str,
    cap: usize,
    dir: &Path,
) -> Result<Saved> {
    output::usable_stem(stem)?;
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
            best: None,
            ffmpeg: None,
        })
        .collect();
    download_named(client, files, destination, "stories").await
}

/// One file of a [`download_named`]: the number it is reported under, the
/// name it is saved as (filtered, without its extension), where it is, what
/// a sentence calls it, and the ceiling on its size; and for a post's video,
/// its best tracks and the ffmpeg that joins them, when it is to be saved
/// from those ([`save_file`]).
#[derive(Debug, Clone)]
pub(crate) struct Named {
    pub number: usize,
    pub stem: String,
    pub url: Option<String>,
    pub what: String,
    pub cap: usize,
    pub best: Option<Best>,
    pub ffmpeg: Option<std::sync::Arc<Ffmpeg>>,
}

/// A video's widest track and its sound, from its DASH manifest, when they
/// are wider than its progressive file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Best {
    pub video: Track,
    pub audio: Option<Track>,
}

/// The ceiling on a video saved from its best tracks, the two together. The
/// largest best track of the capture of 2026-11-13 was 210.6 MB, past
/// [`crate::posts::MAX_POST_BYTES`]; both stream to disk, so the ceiling is
/// only there so that something endless cannot fill it. A pair past it is
/// saved from the progressive file, said before a byte is fetched.
pub const MAX_BEST_BYTES: u64 = 512 * 1024 * 1024;

impl Best {
    /// What the two tracks say they weigh, together.
    fn bytes(&self) -> u64 {
        let video = self.video.bytes.unwrap_or(0);
        let audio = self.audio.as_ref().and_then(|a| a.bytes).unwrap_or(0);
        video.saturating_add(audio)
    }
}

/// `file` saved into `dir`: from its best tracks, joined by ffmpeg, when it
/// has them and an ffmpeg to join them; from its progressive file otherwise,
/// and when the join fails, which is said on standard error. Ctrl+C stops
/// either.
pub(crate) async fn save_file(client: &IgClient, file: &Named, dir: &Path) -> Result<Saved> {
    let (saved, said) = save_file_saying(client, file, dir).await?;
    if let Some(said) = said {
        ui::warn(&said);
    }
    Ok(saved)
}

/// [`save_file`], with what it would have said handed back rather than
/// written: a full-screen view draws on standard error, where a line written
/// in the middle of a frame breaks it, and shows the sentence in its own
/// note line instead.
pub(crate) async fn save_file_saying(
    client: &IgClient,
    file: &Named,
    dir: &Path,
) -> Result<(Saved, Option<String>)> {
    let (best, mut said) = joinable(file);
    if let Some(best) = best {
        if let Some(existing) = already_saved(dir, &file.stem) {
            return Ok((Saved::Already(existing), None));
        }
        sweep_stale_parts(dir, &file.stem);
        match joined(client, &file.stem, best, file.ffmpeg.as_deref(), dir).await {
            Ok(part) => return Ok((place(&part, dir, &file.stem)?, None)),
            Err(e) if was_canceled(&e) => return Err(e),
            Err(e) => said = Some(smaller(file, &format!("{e:#}"))),
        }
    }
    let saved = save_named(
        client,
        &file.stem,
        file.url.as_deref(),
        &file.what,
        file.cap,
        dir,
    )
    .await?;
    Ok((saved, said))
}

/// [`save_file`] into `path`, replacing it as `-o` does, but only with a
/// file that arrived whole.
pub(crate) async fn save_file_to(client: &IgClient, file: &Named, path: &Path) -> Result<()> {
    let (best, said) = joinable(file);
    if let Some(said) = said {
        ui::warn(&said);
    }
    if let Some(best) = best {
        let dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        match joined(client, &file.stem, best, file.ffmpeg.as_deref(), dir).await {
            Ok(part) => {
                return std::fs::rename(&part, path).map_err(|e| {
                    let _ = std::fs::remove_file(&part);
                    anyhow::Error::new(e).context(format!("could not replace {}", path.display()))
                });
            }
            Err(e) if was_canceled(&e) => return Err(e),
            Err(e) => ui::warn(&smaller(file, &format!("{e:#}"))),
        }
    }
    let url = file
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("{} has no downloadable version", file.what))?;
    let mut out = output::Replacing::open(path)?;
    client.download_to(url, file.cap, out.file()).await?;
    out.commit()
}

/// The sentence for a video saved as the app's file because `why`.
fn smaller(file: &Named, why: &str) -> String {
    format!("{} is saved as the app's smaller file: {why}", file.what)
}

/// `file`'s best tracks, when they are to be what is saved: there are some,
/// the sound can be joined (or there is none to join), and they weigh no
/// more than [`MAX_BEST_BYTES`]; with the sentence that says so when they
/// do not, for the caller to say where it can.
fn joinable(file: &Named) -> (Option<&Best>, Option<String>) {
    let Some(best) = file.best.as_ref() else {
        return (None, None);
    };
    if best.audio.is_some() && file.ffmpeg.is_none() {
        return (None, None);
    }
    if best.bytes() > MAX_BEST_BYTES {
        let why = format!(
            "its best tracks weigh {} MB, past {} MB",
            best.bytes() / 1_000_000,
            MAX_BEST_BYTES / 1_000_000
        );
        return (None, Some(smaller(file, &why)));
    }
    (Some(best), None)
}

/// The best tracks of `stem` fetched into `dir` and joined into one MP4,
/// answered as the finished `.part` the caller moves into place. Every part
/// is the process's own (its id in the name) and removed on any failure.
async fn joined(
    client: &IgClient,
    stem: &str,
    best: &Best,
    ffmpeg: Option<&Ffmpeg>,
    dir: &Path,
) -> Result<PathBuf> {
    let pid = std::process::id();
    let video = dir.join(format!("{stem}.{pid}.v.part"));
    let audio = dir.join(format!("{stem}.{pid}.a.part"));
    let out = dir.join(format!("{stem}.{pid}.part"));
    let cap = usize::try_from(MAX_BEST_BYTES).unwrap_or(usize::MAX);
    let result = async {
        fetch_track(client, &best.video.url, cap, &video).await?;
        let (Some(track), Some(ffmpeg)) = (best.audio.as_ref(), ffmpeg) else {
            // A video with no sound is its video track alone.
            std::fs::rename(&video, &out)
                .with_context(|| format!("could not move {}", video.display()))?;
            return Ok(());
        };
        fetch_track(client, &track.url, cap, &audio).await?;
        ffmpeg
            .join(&video, &audio, &out, client.pacer().cancel_token())
            .await
    }
    .await;
    let _ = std::fs::remove_file(&video);
    let _ = std::fs::remove_file(&audio);
    match result {
        Ok(()) => Ok(out),
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            Err(e)
        }
    }
}

/// One track from `url` into `part`, a file this creates, synced.
async fn fetch_track(client: &IgClient, url: &str, cap: usize, part: &Path) -> Result<()> {
    let mut file = output::create_new(part)?;
    client.download_track_to(url, cap, &mut file).await?;
    file.sync_all()
        .with_context(|| format!("could not finish writing {}", part.display()))
}

/// `part` moved into `dir` as `<stem>.mp4`.
fn place(part: &Path, dir: &Path, stem: &str) -> Result<Saved> {
    let name = match output::default_path(dir, stem, "mp4") {
        Ok(name) => name,
        Err(e) => {
            let _ = std::fs::remove_file(part);
            return Err(e);
        }
    };
    let path = dir.join(name);
    if let Err(e) = std::fs::rename(part, &path) {
        let _ = std::fs::remove_file(part);
        return Err(
            anyhow::Error::new(e).context(format!("could not move into place {}", path.display()))
        );
    }
    Ok(Saved::Now(path))
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
    // is on `CdnClient::download_capped`), the client to it carries nothing
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
            let saved = save_file(&client, &file, &dir).await;
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

    fn track(url: &str, bytes: Option<u64>) -> Track {
        Track {
            url: url.into(),
            width: Some(1080),
            height: Some(1920),
            bandwidth: None,
            codecs: None,
            bytes,
        }
    }

    fn file(best: Option<Best>, ffmpeg: bool) -> Named {
        Named {
            number: 1,
            stem: "a-b".into(),
            url: Some("https://x.cdninstagram.com/p.mp4".into()),
            what: "item 1".into(),
            cap: 1,
            best,
            ffmpeg: ffmpeg.then(|| {
                std::sync::Arc::new(Ffmpeg::find(Some(&std::env::current_exe().unwrap())).unwrap())
            }),
        }
    }

    /// Best tracks are what is saved only when their sound can be joined
    /// (or there is none) and they weigh no more than the ceiling, which is
    /// known before a byte is fetched.
    #[test]
    fn a_track_past_the_ceiling_falls_back_before_a_byte_is_fetched() {
        let pair = |video: u64, audio: u64| Best {
            video: track("https://x.fbcdn.net/v", Some(video)),
            audio: Some(track("https://x.fbcdn.net/a", Some(audio))),
        };
        assert!(joinable(&file(Some(pair(30, 20)), true)).0.is_some());
        let heavy = file(Some(pair(MAX_BEST_BYTES, 1)), true);
        let (best, said) = joinable(&heavy);
        assert!(best.is_none());
        assert!(said.is_some_and(|s| s.contains("past 536 MB")), "said why");
        // A sound with nothing to join it.
        assert!(joinable(&file(Some(pair(30, 20)), false)).0.is_none());
        // No sound needs no ffmpeg.
        let silent = Best {
            video: track("https://x.fbcdn.net/v", None),
            audio: None,
        };
        assert!(joinable(&file(Some(silent), false)).0.is_some());
        assert_eq!(joinable(&file(None, true)), (None, None));
    }

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

    /// A stem off the server that is no file name is refused before a path
    /// is built from it: an earlier copy is not looked for outside the
    /// directory (here one is waiting there, and was handed back as saved),
    /// and no `.part` is made anywhere.
    #[tokio::test]
    async fn a_stem_that_is_no_file_name_is_refused_before_any_path_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("downloads");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(tmp.path().join("escaped-1.jpg"), b"theirs").unwrap();

        let session = snob_core::session::Session::from_sessionid(
            "42%3Aabc%3A1",
            "Mozilla/5.0",
            snob_core::session::SessionOrigin::Paste,
        )
        .unwrap();
        let client = IgClient::new(session, snob_ig::pace::Pacer::unlimited()).unwrap();

        // No address, so nothing could be sent whatever the order of the
        // steps; what is under test is what is looked at and made first.
        let saved = save_named(&client, "../escaped-1", None, "story 1", 1024, &dir).await;
        let error = saved.err().expect("a stem with a separator was used");
        assert!(
            error.to_string().contains("cannot be used as a file name"),
            "{error}"
        );

        let left: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left.len(),
            2,
            "something was made beside the directory: {left:?}"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
