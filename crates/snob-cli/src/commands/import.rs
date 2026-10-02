//! `snob import dyi`: reads the archive Instagram hands over under "Download
//! your information" and works out the same relationships the live commands do.
//!
//! It costs nothing and risks nothing: no session, no network, no request. That
//! is the whole point — it is the answer for anyone who would rather not have a
//! tool talk to Instagram on their behalf at all. It reads, crosses and prints,
//! and two things it deliberately does not do are rules rather than gaps:
//!
//! - **It stores nothing.** The export names accounts by username, and
//!   everything in the database is keyed by the numeric id, which an export
//!   never carries. Rather than guess at that mapping, the stored snapshots
//!   keep meaning "walked live", and the monitor can never diff against a
//!   file somebody dropped in.
//! - **It crosses nothing live.** Both lists here come out of one export,
//!   written at one instant, which is what makes the username arithmetic
//!   below sound. A username crossed against a list walked at another moment
//!   is not sound — a name can change hands in between — and a crossing that
//!   silently mixed the two would be a wrong answer dressed as a partial one.
//!
//! # What an export looks like
//!
//! Instagram offers the export in two forms, HTML and JSON, and both are read
//! here; either arrives as a zip, and a person who has already extracted it can
//! point at the folder instead. The two lists live under
//! `connections/followers_and_following/`: `following` as one file, and
//! `followers` split over `followers_1`, `followers_2` and so on. **The file
//! names are English whatever language the account is set to; the contents of
//! the HTML form are not** — headings and dates come out in the account's
//! language (a Spanish account gets a Spanish heading, and dates its rows
//! `sept. 03, 2026 9:33 pm`), which is why nothing below reads a
//! heading or a date, and every name is taken from the profile link instead.
//! An empty section is not an empty file: Instagram writes a `no-data.txt`
//! where the file would have been.
//!
//! The rest of the folder — the other relationship lists, blocked accounts,
//! close friends, pending requests — is left alone. Each is a different
//! relationship, and they render differently too (a name in a table cell
//! rather than a link), so a reader that swept them in would either miscount
//! or read nothing.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::Value;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{self, Rendered};
use crate::ui;

/// Ceiling on everything pulled out of the export, across all its files.
///
/// The largest real follower list runs to a few megabytes, so this is already
/// generous by an order of magnitude. It is a total rather than a per-file
/// limit because the number of matching files is not bounded either:
/// `followers_1` through `followers_9999` are all valid names, and deflate
/// packs about a thousand to one, so a per-file cap leaves a one-megabyte
/// archive able to ask for gigabytes.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// How far below the folder it was pointed at the reader looks.
///
/// The lists sit two levels down (`connections/followers_and_following/`), and
/// an export extracted into a folder of its own, inside another, is still
/// well within this. What it stops is a walk of a whole drive because somebody
/// pointed at the wrong folder.
const MAX_FOLDER_DEPTH: u8 = 6;

/// How many files and folders the walk will look at before deciding it was
/// not pointed at an export. A real one holds a few thousand, most of them
/// message attachments.
const MAX_FOLDER_ENTRIES: usize = 100_000;

/// `snob import`'s subcommands. Next to the one function that takes them.
#[derive(Subcommand, Debug)]
pub enum ImportCommand {
    /// Import Instagram's "Download your information" archive
    Dyi {
        /// The archive Instagram sent, or the folder it was extracted into
        path: std::path::PathBuf,
    },
}

pub fn run(command: ImportCommand) -> Result<ExitCode> {
    let ImportCommand::Dyi { path } = command;
    let (shape, export) = read_export(&path)?;

    ui::info(&format!(
        "Read {} followers and {} following from the {} export at {}",
        export.followers.len(),
        export.following.len(),
        shape.as_str(),
        path.display()
    ));
    ui::warn(
        "an export describes the moment Instagram built it, not this one; \
         anything that changed since is not in here",
    );

    let analysis = analyze(&export);
    let format = output::effective_format(None, None);
    output::write_rendered(&render(&analysis, format)?, None)?;
    Ok(ExitCode::Ok)
}

/// Sets rather than lists: Instagram repeats accounts across the split files,
/// so the duplicates have to go anyway, and a set drops them as they arrive
/// instead of in a pass of its own afterwards.
#[derive(Debug, Default)]
struct Export {
    followers: HashSet<String>,
    following: HashSet<String>,
}

struct Analysis {
    followers: usize,
    following: usize,
    friends: Vec<String>,
    fans: Vec<String>,
    unfollowers: Vec<String>,
}

/// The set arithmetic, on usernames.
///
/// Everywhere else the tool crosses lists by numeric id, because a username can
/// be given up and taken by somebody else between two walks. Here it is safe:
/// both lists were written by the same export at the same instant, so no rename
/// can have happened in between.
fn analyze(export: &Export) -> Analysis {
    let (followers, following) = (&export.followers, &export.following);

    fn sorted<'a>(names: impl Iterator<Item = &'a String>) -> Vec<String> {
        let mut names: Vec<String> = names.cloned().collect();
        names.sort();
        names
    }

    Analysis {
        followers: followers.len(),
        following: following.len(),
        friends: sorted(followers.intersection(following)),
        fans: sorted(followers.difference(following)),
        unfollowers: sorted(following.difference(followers)),
    }
}

fn render(analysis: &Analysis, format: Format) -> Result<Rendered> {
    // Without `--format` or `-o` the choice is only ever a table on a terminal
    // or JSON down a pipe; the other formats cannot be reached from here.
    if format == Format::Table {
        let mut out = String::new();
        for (label, count) in [
            ("Followers:", analysis.followers),
            ("Following:", analysis.following),
            ("Friends:", analysis.friends.len()),
            ("Fans:", analysis.fans.len()),
            ("Unfollowers:", analysis.unfollowers.len()),
        ] {
            out.push_str(&format!("{label:<14}{count}\n"));
        }
        if !analysis.unfollowers.is_empty() {
            out.push('\n');
            for name in &analysis.unfollowers {
                // Off a file somebody else could have written, and drawn on a
                // terminal: the same rule as every name off the wire.
                out.push_str(&snob_core::model::printable(name));
                out.push('\n');
            }
        }
        return Ok(Rendered::Text(out));
    }

    // The keys match `snob scan`, so a script reading one reads the other.
    let object = serde_json::json!({
        "source": "dyi",
        "counts": {
            "followers": analysis.followers,
            "following": analysis.following,
            "friends": analysis.friends.len(),
            "fans": analysis.fans.len(),
            "unfollowers": analysis.unfollowers.len(),
        },
        "unfollowers": analysis.unfollowers,
        "fans": analysis.fans,
        "friends": analysis.friends,
    });
    let mut text = serde_json::to_string_pretty(&object)?;
    text.push('\n');
    Ok(Rendered::Text(text))
}

/// Which of the two lists a file inside the export belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Followers,
    Following,
}

/// The two forms Instagram offers the export in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Shape {
    Json,
    Html,
}

impl Shape {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Html => "HTML",
        }
    }
}

/// What a file's name says about it: which list, in which form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listed {
    which: Which,
    shape: Shape,
}

/// Recognizes `followers.json`, `followers_1.html`, `following.json` and so on,
/// wherever they sit in the export.
///
/// The numeric suffix is checked rather than assumed, so neighbors like
/// `follow_requests_sent.json` are not swept in — they hold different
/// relationships and would corrupt every count.
fn list_in(name: &str) -> Option<Listed> {
    let file = name.rsplit(['/', '\\']).next()?;
    let (stem, shape) = if let Some(stem) = file.strip_suffix(".json") {
        (stem, Shape::Json)
    } else {
        let stem = file.strip_suffix(".html")?;
        (stem, Shape::Html)
    };

    for (prefix, which) in [
        ("followers", Which::Followers),
        ("following", Which::Following),
    ] {
        let Some(rest) = stem.strip_prefix(prefix) else {
            continue;
        };
        let numbered = rest
            .strip_prefix('_')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if rest.is_empty() || numbered {
            return Some(Listed { which, shape });
        }
    }
    None
}

/// What a walk of the export accumulates, whichever container it came in.
///
/// The two containers — a zip and a folder — only differ in how a file is
/// found and opened; everything after that is the same, and lives here so
/// that it is the same. A file is offered by name first ([`Intake::wanted`])
/// and opened only if the name says it is a list, so a folder walk does not
/// open a thousand message attachments to find out they are not.
#[derive(Default)]
struct Intake {
    export: Export,
    /// The forms of the lists actually read. More than one is two exports on
    /// top of each other, and is refused in [`Intake::finish`].
    shapes: HashSet<Shape>,
    json_seen: bool,
    html_seen: bool,
    spent: u64,
}

impl Intake {
    /// Notes that a file exists and says whether it is one of the lists.
    fn wanted(&mut self, name: &str) -> Option<Listed> {
        self.json_seen |= name.ends_with(".json");
        self.html_seen |= name.ends_with(".html");
        list_in(name)
    }

    /// Reads one list file, within the total budget, and adds its names.
    fn read(&mut self, name: &str, listed: Listed, reader: impl Read) -> Result<()> {
        let left = MAX_TOTAL_BYTES - self.spent;
        let mut text = String::new();
        let read = reader
            .take(left + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("could not read {name} out of the export"))?
            as u64;
        if read > left {
            bail!(
                "the lists in that export add up to more than {} MB, which no real \
                 export does",
                MAX_TOTAL_BYTES / (1024 * 1024)
            );
        }
        self.spent += read;

        let names = match listed.shape {
            Shape::Json => usernames_from_json(&text, listed.which),
            Shape::Html => usernames_from_html(&text),
        }
        .with_context(|| format!("could not make sense of {name}"))?;
        self.shapes.insert(listed.shape);
        match listed.which {
            // Instagram splits a long list over followers_1, followers_2 and so
            // on, so these accumulate rather than replace.
            Which::Followers => self.export.followers.extend(names),
            Which::Following => self.export.following.extend(names),
        }
        Ok(())
    }

    /// The export, once everything in the container has been offered.
    ///
    /// `container` is the word for what was pointed at, for the messages.
    fn finish(self, container: &str) -> Result<(Shape, Export)> {
        let mut shapes = self.shapes.into_iter();
        let shape = match (shapes.next(), shapes.next()) {
            (Some(shape), None) => shape,
            (Some(_), Some(_)) => bail!(
                "that {container} holds the lists in both HTML and JSON, which is two \
                 exports on top of each other; point at one of them"
            ),
            (None, _) if self.json_seen || self.html_seen => bail!(
                "that {container} has no followers or following lists in it. They live \
                 under connections/followers_and_following/ — check the export included \
                 \"Followers and following\""
            ),
            (None, _) => bail!(
                "that {container} is not an Instagram export: it holds neither the HTML \
                 nor the JSON form"
            ),
        };
        Ok((shape, self.export))
    }
}

/// Reads an export, from the zip Instagram hands over or the folder it was
/// extracted into.
fn read_export(path: &Path) -> Result<(Shape, Export)> {
    let mut intake = Intake::default();
    if path.is_dir() {
        read_folder(path, &mut intake)?;
        intake.finish("folder")
    } else {
        read_archive(path, &mut intake)?;
        intake.finish("archive")
    }
}

#[cfg(feature = "xlsx")]
fn read_archive(path: &Path, intake: &mut Intake) -> Result<()> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable zip archive", path.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if let Some(listed) = intake.wanted(&name) {
            intake.read(&name, listed, entry.by_ref())?;
        }
    }
    Ok(())
}

/// The `zip` crate arrives with the `xlsx` feature, and a build made without
/// it can still read the export — extracted.
#[cfg(not(feature = "xlsx"))]
fn read_archive(path: &Path, _intake: &mut Intake) -> Result<()> {
    if !path.exists() {
        bail!("could not open {}", path.display());
    }
    bail!(
        "this build cannot open a zip archive; extract {} and point at the folder",
        path.display()
    )
}

/// Walks a folder for the lists, without following links out of it.
///
/// Symbolic links are skipped rather than followed: the walk should stay
/// inside what it was pointed at, and a link is the one way a folder can
/// reach outside itself.
fn read_folder(root: &Path, intake: &mut Intake) -> Result<()> {
    let mut pending = vec![(root.to_path_buf(), 0u8)];
    let mut visited = 0usize;

    while let Some((dir, depth)) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("could not list {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("could not list {}", dir.display()))?;
            visited += 1;
            if visited > MAX_FOLDER_ENTRIES {
                bail!(
                    "that folder holds more than {MAX_FOLDER_ENTRIES} files and folders, \
                     which no export does; point at the export itself"
                );
            }

            let kind = entry
                .file_type()
                .with_context(|| format!("could not read {}", entry.path().display()))?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if depth < MAX_FOLDER_DEPTH {
                    pending.push((entry.path(), depth + 1));
                }
                continue;
            }

            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(listed) = intake.wanted(name) {
                let path = entry.path();
                let file = std::fs::File::open(&path)
                    .with_context(|| format!("could not open {}", path.display()))?;
                intake.read(name, listed, file)?;
            }
        }
    }
    Ok(())
}

/// Instagram has spelled the JSON payload two ways over the years: a bare
/// array, or an object wrapping one under `relationships_followers` and
/// friends.
///
/// The key belonging to *this* list is tried first, then the shape, because the
/// naming is what has moved. Taking the first array of whichever key happened
/// to come first would let a file carrying both hand back the wrong one and
/// swap fans with unfollowers. An object holding more than one unnamed array is
/// refused rather than guessed at, for the same reason.
fn entries(value: &Value, which: Which) -> Option<&[Value]> {
    if let Some(items) = value.as_array() {
        return Some(items);
    }

    let map = value.as_object()?;
    let expected = match which {
        Which::Followers => "relationships_followers",
        Which::Following => "relationships_following",
    };
    if let Some(items) = map.get(expected).and_then(Value::as_array) {
        return Some(items);
    }

    let mut arrays = map.values().filter_map(Value::as_array);
    let only = arrays.next()?;
    arrays.next().is_none().then_some(only.as_slice())
}

/// The names in one JSON list file.
///
/// An entry is an object with `title`, `media_list_data` and
/// `string_list_data`, whose first element carries `href`, `value` and a
/// `timestamp`. The HTML form is rendered from the same data, and it shows the
/// two lists filling those fields differently: a follower has an empty `title`
/// and the name in `value`; a followed account has the name in `title`, the
/// app's `/_u/NAME` deep link in `href`, and for link text — which is where
/// `value` lands on the page — the address rather than the name. Whether the
/// JSON's `value` is the name or the address there has not been checked
/// against a JSON export; the page says address. So the name is read out of
/// `href` first, which both lists have and which is unambiguous, and `value`
/// and `title` are the fallbacks, each accepted only if it looks like a name.
/// Reading `value` alone would risk taking one list as names and the other
/// as addresses and crossing them into nothing.
fn usernames_from_json(text: &str, which: Which) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).context("it is not valid JSON")?;
    let Some(items) = entries(&value, which) else {
        bail!("expected a list of accounts and found something else");
    };

    let names: Vec<String> = items.iter().filter_map(username_in_entry).collect();

    // A list with entries in it that yields no names at all means the shape
    // moved again. Returning an empty list would be worse than failing: every
    // account on the other side would be reported as an unfollower, confidently
    // and wrongly.
    if names.is_empty() && !items.is_empty() {
        bail!(
            "it holds {} entries but no usernames, so the export's shape is not the one \
             this understands",
            items.len()
        );
    }
    Ok(names)
}

/// The name in one JSON entry, wherever this export put it.
fn username_in_entry(entry: &Value) -> Option<String> {
    let first = entry
        .get("string_list_data")
        .and_then(Value::as_array)
        .and_then(|data| data.first());
    let field = |key: &str| first.and_then(|data| data.get(key)).and_then(Value::as_str);

    field("href")
        .and_then(username_in_profile_link)
        .or_else(|| {
            field("value")
                .and_then(|value| username_in_profile_link(value).or_else(|| plain_username(value)))
        })
        .or_else(|| {
            entry
                .get("title")
                .and_then(Value::as_str)
                .and_then(plain_username)
        })
}

/// The names in one page of the HTML export.
///
/// The page is one `<div>` per account inside `<main>`, and the two lists do
/// not render alike. A follower is
/// `<a href="https://www.instagram.com/NAME">NAME</a>` and a date; a followed
/// account is `<h2>NAME</h2>`, then
/// `<a href="https://www.instagram.com/_u/NAME">https://www.instagram.com/_u/NAME</a>`
/// and a date — the address is the app's deep link, and the link text is the
/// address rather than the name. The link's target is the one thing both
/// have, so that is what is read, and the heading and the date, which are in
/// the account's language, are not.
///
/// Read with string searches rather than an HTML parser on purpose: the
/// page is machine-written and regular, a parser is a dependency this binary
/// would carry for one file, and the only thing wanted out of the page is a
/// list of attribute values.
fn usernames_from_html(text: &str) -> Result<Vec<String>> {
    // Everything before <main> is boilerplate: a stylesheet, a logo, a heading
    // and the moment the export was built.
    let body = text.find("<main").map_or(text, |at| &text[at..]);

    let names: Vec<String> = hrefs_in(body)
        .filter_map(|href| username_in_profile_link(&decode_entities(href)))
        .collect();

    // The same guard as the JSON reader's: a page with rows on it that yields
    // no names means the shape moved, and an empty list would be a wrong
    // answer rather than a partial one.
    if names.is_empty() && body.contains("<div") {
        bail!(
            "it has rows on it but no profile links, so the export's shape is not the \
             one this understands"
        );
    }
    Ok(names)
}

/// Every `href` attribute value in the markup, undecoded.
fn hrefs_in(html: &str) -> impl Iterator<Item = &str> {
    html.match_indices("href=").filter_map(move |(at, found)| {
        let rest = &html[at + found.len()..];
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let rest = &rest[1..];
        let end = rest.find(quote)?;
        Some(&rest[..end])
    })
}

/// The five named entities and the numeric ones. That is the whole of what
/// the export writes into an attribute — `&amp;` in an address, `&#064;` for
/// an at sign — and anything else is left as it came.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Paths under instagram.com that are not profiles, so a link to one is not
/// read as an account named `p` or `explore`. Instagram reserves these names.
const NOT_A_PROFILE: [&str; 8] = [
    "p", "reel", "reels", "explore", "stories", "accounts", "direct", "_u",
];

/// The account a link points at, if it is a profile link on instagram.com.
///
/// Both spellings the export uses resolve here: `instagram.com/NAME`, and the
/// app's deep link `instagram.com/_u/NAME`. Anything on another host, or
/// pointing at a post rather than a profile, is not a name.
fn username_in_profile_link(href: &str) -> Option<String> {
    let rest = href.trim();
    let rest = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !host.eq_ignore_ascii_case("www.instagram.com")
        && !host.eq_ignore_ascii_case("instagram.com")
    {
        return None;
    }

    let path = path.trim_start_matches('/');
    let path = match path.strip_prefix("_u/") {
        Some(deep) if !deep.is_empty() => deep,
        _ => path,
    };
    let segment = path.split(['/', '?', '#']).next()?;
    if NOT_A_PROFILE.contains(&segment) {
        return None;
    }
    plain_username(segment)
}

/// A name as Instagram allows one to be spelled: letters, digits, `.` and
/// `_`, nothing else. Lowercased, because usernames are case-insensitive and
/// the export has been seen to disagree with itself about capitalization —
/// crossing two lists that spell one account differently would invent both an
/// unfollower and a fan out of one person.
///
/// The alphabet check is what keeps a display name, an address on another
/// host or a stray piece of markup from being counted as an account.
fn plain_username(text: &str) -> Option<String> {
    let name = text.trim();
    let allowed = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_');
    allowed.then(|| name.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// One entry in the shape every recent export uses.
    fn entry(name: &str) -> String {
        format!(
            r#"{{"title":"","media_list_data":[],"string_list_data":
               [{{"href":"https://www.instagram.com/{name}","value":"{name}",
                  "timestamp":1704067200}}]}}"#
        )
    }

    fn wrapped(key: &str, names: &[&str]) -> String {
        let items: Vec<String> = names.iter().map(|n| entry(n)).collect();
        format!(r#"{{"{key}":[{}]}}"#, items.join(","))
    }

    fn bare(names: &[&str]) -> String {
        let items: Vec<String> = names.iter().map(|n| entry(n)).collect();
        format!("[{}]", items.join(","))
    }

    /// A follower's row as the HTML export writes it: the name is the link
    /// text and the link is the plain profile address.
    fn follower_row(name: &str) -> String {
        format!(
            r#"<div class="pam _3-95 _2ph- _a6-g uiBoxWhite noborder"><div class="_a6-p"><div><div><a target="_blank" href="https://www.instagram.com/{name}">{name}</a></div><div>03. Sept. 2026, 9:33 pm</div></div></div></div>"#
        )
    }

    /// A followed account's row: the name is a heading, the link is the app's
    /// deep link, and the link text is the address itself.
    fn following_row(name: &str) -> String {
        format!(
            r#"<div class="pam _3-95 _2ph- _a6-g uiBoxWhite noborder"><h2 class="_3-95 _2pim _a6-h _a6-i">{name}</h2><div class="_a6-p"><div><div><a target="_blank" href="https://www.instagram.com/_u/{name}">https://www.instagram.com/_u/{name}</a></div><div>08. Sept. 2026, 3:54 pm</div></div></div></div>"#
        )
    }

    /// A whole page, with the boilerplate a real one carries around the rows:
    /// a base href, a stylesheet with links in it, a logo, a heading in the
    /// account's language and the moment the export was built.
    fn page(heading: &str, rows: &str) -> String {
        format!(
            r#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=UTF-8" /><base href="../../" /><style type="text/css" nonce="x">a{{color:#385898}}.s{{background:url(https://static.xx.fbcdn.net/rsrc.php/v4/yB/r/x.png)}}</style><title></title></head><body class="_5vb_ _2yq _a7o5"><div class="_li"><div class="_as_0" style="background:white"><img src="files/Instagram-Logo.png" height="28" alt="Instagram" /></div><div class="_a705"><header class="_as-_ _a70a" aria-labelledby="u_0_1t_Ib"><div class="_a70d"><h1 id="u_0_1t_Ib">{heading}</h1><aside role="contentinfo" class="_aoaa"><time datetime="2026-09-09T08:16Z">2026-09-09T08:16Z</time></aside></div></header><main class="_a706" role="main">{rows}</main></div></div></body></html>"#
        )
    }

    fn followers_page(names: &[&str]) -> String {
        let rows: String = names.iter().map(|n| follower_row(n)).collect();
        page("Follower", &rows)
    }

    fn following_page(names: &[&str]) -> String {
        let rows: String = names.iter().map(|n| following_row(n)).collect();
        page("Abonniert", &rows)
    }

    #[test]
    fn it_reads_the_wrapped_shape() {
        let names = usernames_from_json(
            &wrapped("relationships_following", &["ann", "bob"]),
            Which::Following,
        )
        .unwrap();
        assert_eq!(names, vec!["ann", "bob"]);
    }

    /// The older exports handed followers over as a naked array.
    #[test]
    fn it_reads_the_bare_array_shape() {
        assert_eq!(
            usernames_from_json(&bare(&["ann"]), Which::Followers).unwrap(),
            vec!["ann"]
        );
    }

    /// The wrapper key has been renamed before, so an unfamiliar one must not
    /// stop it working.
    #[test]
    fn an_unfamiliar_wrapper_key_still_works() {
        let names = usernames_from_json(
            &wrapped("relationships_something_new", &["ann"]),
            Which::Followers,
        )
        .unwrap();
        assert_eq!(names, vec!["ann"]);
    }

    /// Two arrays and no known key is ambiguous. Guessing would give a
    /// confident wrong answer, which is worse than saying so.
    #[test]
    fn an_ambiguous_object_is_refused() {
        let json = format!(r#"{{"a":[{}],"b":[{}]}}"#, entry("ann"), entry("bob"));
        assert!(usernames_from_json(&json, Which::Followers).is_err());
    }

    #[test]
    fn entries_without_a_username_are_skipped() {
        let json = format!(
            r#"[{},{{"string_list_data":[]}},{{"string_list_data":[{{"href":"x"}}]}}]"#,
            entry("ann")
        );
        assert_eq!(
            usernames_from_json(&json, Which::Followers).unwrap(),
            vec!["ann"]
        );
    }

    /// The following list's entries carry the app's deep link, and — on the
    /// evidence of the HTML rendered from the same data — may carry the
    /// address where the name is expected. Both have to read as the name.
    #[test]
    fn a_following_entry_is_read_from_its_deep_link() {
        let json = r#"{"relationships_following":[
            {"title":"ann","media_list_data":[],"string_list_data":[
                {"href":"https://www.instagram.com/_u/ann",
                 "value":"https://www.instagram.com/_u/ann","timestamp":1}]},
            {"title":"bob","media_list_data":[],"string_list_data":[
                {"href":"https://www.instagram.com/_u/bob","value":"bob","timestamp":2}]}
        ]}"#;
        assert_eq!(
            usernames_from_json(json, Which::Following).unwrap(),
            vec!["ann", "bob"]
        );
    }

    /// If the link and the value both move, the title is the last place the
    /// name can be; and a title that is not a name is not taken for one.
    #[test]
    fn the_title_is_the_fallback_and_only_when_it_is_a_name() {
        let json = r#"[{"title":"ann","string_list_data":[]},
                       {"title":"Ann Smith","string_list_data":[]}]"#;
        assert_eq!(
            usernames_from_json(json, Which::Following).unwrap(),
            vec!["ann"]
        );
    }

    /// A file carrying both keys must hand back the one it was opened for.
    /// Taking the other would swap fans with unfollowers and say nothing.
    #[test]
    fn a_file_holding_both_keys_yields_the_list_it_was_read_for() {
        let json = format!(
            r#"{{"relationships_followers":[{}],"relationships_following":[{}]}}"#,
            entry("follower"),
            entry("followed")
        );
        assert_eq!(
            usernames_from_json(&json, Which::Followers).unwrap(),
            vec!["follower"]
        );
        assert_eq!(
            usernames_from_json(&json, Which::Following).unwrap(),
            vec!["followed"]
        );
    }

    /// If the shape moves again, entries that yield no username at all have to
    /// be an error. Reporting an empty list would mark everyone on the other
    /// side as an unfollower, confidently and wrongly.
    #[test]
    fn entries_that_yield_no_names_are_an_error_not_an_empty_list() {
        let json = r#"[{"title":"","string_list_data":[{"href":"https://x/ann"}]},
                       {"title":"","string_list_data":[{"href":"https://x/bob"}]}]"#;
        let error = usernames_from_json(json, Which::Followers).unwrap_err();
        assert!(error.to_string().contains("no usernames"), "{error}");

        // A genuinely empty list is still fine: some accounts follow nobody.
        assert!(
            usernames_from_json("[]", Which::Followers)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn capitalization_does_not_split_one_account_in_two() {
        let names = usernames_from_json(&bare(&["Ann"]), Which::Followers).unwrap();
        assert_eq!(names, vec!["ann"]);
    }

    /// The HTML follower page: names are link text and plain profile links.
    #[test]
    fn it_reads_the_html_follower_page() {
        let names = usernames_from_html(&followers_page(&["ann", "bob.b", "c_3"])).unwrap();
        assert_eq!(names, vec!["ann", "bob.b", "c_3"]);
    }

    /// The HTML following page: names are headings, links are the app's deep
    /// links and the link text is the address. The name still comes out.
    #[test]
    fn it_reads_the_html_following_page() {
        let names = usernames_from_html(&following_page(&["ann", "Bob"])).unwrap();
        assert_eq!(names, vec!["ann", "bob"]);
    }

    /// The heading, the date and the stylesheet's own addresses are in the
    /// account's language or on other hosts, and none of them is a name.
    #[test]
    fn the_boilerplate_around_the_rows_yields_no_names() {
        let names = usernames_from_html(&followers_page(&["ann"])).unwrap();
        assert_eq!(names, vec!["ann"]);

        // A page with nothing on it is an empty list, not an error.
        assert!(
            usernames_from_html(&page("Follower", ""))
                .unwrap()
                .is_empty()
        );
    }

    /// The other files in the same folder write the name into a table cell
    /// with no link. A page shaped like that is a changed shape, not an empty
    /// list.
    #[test]
    fn rows_without_profile_links_are_an_error_not_an_empty_list() {
        let rows = r#"<div class="pam"><div class="_a6-p"><table><tr><td>Name</td><td>ann</td></tr></table></div><div>03. Sept. 2026</div></div>"#;
        let error = usernames_from_html(&page("Follower", rows)).unwrap_err();
        assert!(error.to_string().contains("no profile links"), "{error}");
    }

    #[test]
    fn a_profile_link_is_read_in_every_spelling_and_nothing_else_is() {
        for (href, expected) in [
            ("https://www.instagram.com/ann", Some("ann")),
            ("https://www.instagram.com/ann/", Some("ann")),
            ("https://www.instagram.com/_u/ann", Some("ann")),
            ("https://instagram.com/Ann.B_2", Some("ann.b_2")),
            ("http://www.instagram.com/ann?igsh=abc&x=1", Some("ann")),
            ("https://www.instagram.com/ann#top", Some("ann")),
            // Not profiles.
            ("https://www.instagram.com/p/C1a2b3/", None),
            ("https://www.instagram.com/explore/tags/x/", None),
            ("https://www.instagram.com/_u/", None),
            ("https://www.instagram.com/", None),
            ("https://www.instagram.com", None),
            // Not Instagram.
            ("https://static.xx.fbcdn.net/rsrc.php/v4/x.png", None),
            ("https://www.instagram.com.evil.example/ann", None),
            ("../../", None),
            ("files/Instagram-Logo.png", None),
            ("", None),
        ] {
            assert_eq!(
                username_in_profile_link(href).as_deref(),
                expected,
                "{href}"
            );
        }
    }

    #[test]
    fn entities_in_an_address_are_decoded_first() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&#064;ann"), "@ann");
        assert_eq!(decode_entities("&#x40;ann"), "@ann");
        assert_eq!(decode_entities("&quot;&apos;&lt;&gt;"), "\"'<>");
        // Left as they came: an unknown entity, a bare ampersand, one unclosed.
        assert_eq!(decode_entities("&bogus;&"), "&bogus;&");
        assert_eq!(decode_entities("a & b"), "a & b");
        assert_eq!(decode_entities("&#zz;"), "&#zz;");

        let row = r#"<main><div><a href="https://www.instagram.com/ann?a=1&amp;b=2">ann</a></div></main>"#;
        assert_eq!(usernames_from_html(row).unwrap(), vec!["ann"]);
    }

    #[test]
    fn it_recognizes_the_files_and_leaves_the_neighbors_alone() {
        let followers = |shape| {
            Some(Listed {
                which: Which::Followers,
                shape,
            })
        };
        let following = |shape| {
            Some(Listed {
                which: Which::Following,
                shape,
            })
        };
        assert_eq!(
            list_in("connections/followers_and_following/followers_1.json"),
            followers(Shape::Json)
        );
        assert_eq!(list_in("followers.json"), followers(Shape::Json));
        assert_eq!(list_in("followers_12.json"), followers(Shape::Json));
        assert_eq!(
            list_in("connections/followers_and_following/following.json"),
            following(Shape::Json)
        );
        assert_eq!(list_in("following_2.json"), following(Shape::Json));
        assert_eq!(
            list_in("connections/followers_and_following/followers_1.html"),
            followers(Shape::Html)
        );
        assert_eq!(list_in("following.html"), following(Shape::Html));

        // Different relationships that must not be counted as either.
        assert_eq!(list_in("follow_requests_sent.json"), None);
        assert_eq!(list_in("pending_follow_requests.json"), None);
        assert_eq!(list_in("recently_unfollowed_profiles.json"), None);
        assert_eq!(list_in("recently_unfollowed_profiles.html"), None);
        assert_eq!(list_in("follow_requests_you've_received.html"), None);
        assert_eq!(list_in("followers_and_following.html"), None);
        assert_eq!(list_in("followers_extra.json"), None);
        assert_eq!(list_in("followers_1.txt"), None);
    }

    /// An export extracted into a folder, laid out as Instagram lays it out,
    /// with the files around the lists that a real one has.
    fn folder(files: &[(&str, String)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    /// A small export with one of everything: a friend, a fan, an unfollower.
    fn small_export() -> tempfile::TempDir {
        folder(&[
            ("start_here.html", page("Start", "")),
            (
                "connections/followers_and_following/followers_1.html",
                followers_page(&["ann", "bob"]),
            ),
            (
                "connections/followers_and_following/following.html",
                following_page(&["bob", "dan"]),
            ),
            (
                "connections/followers_and_following/recently_unfollowed_profiles.html",
                page("Left", "<div><table><tr><td>ann</td></tr></table></div>"),
            ),
            (
                "your_instagram_activity/messages/inbox/x_1/message_1.html",
                page(
                    "Chat",
                    r#"<div><a href="https://www.instagram.com/zed">zed</a></div>"#,
                ),
            ),
        ])
    }

    #[test]
    fn an_extracted_html_export_is_read_from_its_folder() {
        let dir = small_export();
        let (shape, export) = read_export(dir.path()).unwrap();
        assert_eq!(shape, Shape::Html);
        let analysis = analyze(&export);
        assert_eq!(analysis.followers, 2);
        assert_eq!(analysis.following, 2);
        assert_eq!(analysis.friends, vec!["bob"]);
        assert_eq!(analysis.fans, vec!["ann"]);
        assert_eq!(analysis.unfollowers, vec!["dan"]);
    }

    /// Pointing at the folder the export was extracted *into*, or at the
    /// `connections` folder inside it, both work: the lists are found by name
    /// wherever they sit.
    #[test]
    fn the_lists_are_found_wherever_the_folder_is_pointed() {
        let dir = folder(&[
            (
                "instagram-someone-2026-09-09-abc/connections/followers_and_following/followers_1.json",
                bare(&["ann"]),
            ),
            (
                "instagram-someone-2026-09-09-abc/connections/followers_and_following/following.json",
                wrapped("relationships_following", &["ann"]),
            ),
        ]);
        let (shape, export) = read_export(dir.path()).unwrap();
        assert_eq!(shape, Shape::Json);
        assert_eq!(export.followers.len(), 1);

        let inner = dir
            .path()
            .join("instagram-someone-2026-09-09-abc/connections");
        assert_eq!(read_export(&inner).unwrap().1.following.len(), 1);
    }

    /// A folder nested past the depth an export ever has is not searched.
    #[test]
    fn the_walk_stops_at_the_depth_an_export_has() {
        let too_deep = "a/b/c/d/e/f/g/followers_1.json";
        let dir = folder(&[(too_deep, bare(&["ann"]))]);
        let error = read_export(dir.path()).unwrap_err().to_string();
        assert!(error.contains("not an Instagram export"), "{error}");
    }

    /// HTML and JSON lists in one folder are two exports extracted on top of
    /// each other, and reading both would merge two moments into one.
    #[test]
    fn two_forms_in_one_folder_are_refused() {
        let dir = folder(&[
            ("followers_1.html", followers_page(&["ann"])),
            (
                "following.json",
                wrapped("relationships_following", &["ann"]),
            ),
        ]);
        let error = read_export(dir.path()).unwrap_err().to_string();
        assert!(error.contains("both HTML and JSON"), "{error}");
    }

    #[test]
    fn a_folder_that_is_not_an_export_says_so() {
        let dir = folder(&[("notes.txt", "hello".into()), ("photo.jpg", "".into())]);
        let error = read_export(dir.path()).unwrap_err().to_string();
        assert!(error.contains("not an Instagram export"), "{error}");
    }

    #[test]
    fn a_folder_without_the_lists_says_where_they_should_be() {
        let dir = folder(&[("start_here.html", page("Start", ""))]);
        let error = read_export(dir.path()).unwrap_err().to_string();
        assert!(error.contains("followers_and_following"), "{error}");
    }

    #[test]
    fn a_path_that_does_not_exist_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_export(&dir.path().join("missing.zip"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not open"), "{error}");
    }

    #[cfg(feature = "xlsx")]
    fn archive(files: &[(&str, String)]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in files {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
            let bytes = writer.finish().unwrap().into_inner();
            file.write_all(&bytes).unwrap();
            file.flush().unwrap();
        }
        file
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn a_split_follower_list_is_read_as_one() {
        let zipped = archive(&[
            (
                "connections/followers_and_following/followers_1.json",
                bare(&["ann", "bob"]),
            ),
            (
                "connections/followers_and_following/followers_2.json",
                bare(&["cat"]),
            ),
            (
                "connections/followers_and_following/following.json",
                wrapped("relationships_following", &["bob", "dan"]),
            ),
        ]);

        let (shape, export) = read_export(zipped.path()).unwrap();
        assert_eq!(shape, Shape::Json);
        let mut followers: Vec<&String> = export.followers.iter().collect();
        followers.sort();
        assert_eq!(followers, ["ann", "bob", "cat"].iter().collect::<Vec<_>>());
        assert_eq!(export.following.len(), 2);

        let analysis = analyze(&export);
        assert_eq!(analysis.followers, 3);
        assert_eq!(analysis.following, 2);
        assert_eq!(analysis.friends, vec!["bob"]);
        assert_eq!(analysis.fans, vec!["ann", "cat"]);
        assert_eq!(analysis.unfollowers, vec!["dan"]);
    }

    /// The zip as Instagram builds it for the HTML form, with the pages that
    /// sit next to the lists and must not be read as them.
    #[cfg(feature = "xlsx")]
    #[test]
    fn an_html_archive_is_read_as_the_json_one_is() {
        let zipped = archive(&[
            ("start_here.html", page("Start", "")),
            (
                "connections/followers_and_following/followers_1.html",
                followers_page(&["ann", "bob"]),
            ),
            (
                "connections/followers_and_following/following.html",
                following_page(&["bob", "dan"]),
            ),
            (
                "connections/followers_and_following/pending_follow_requests.html",
                page("Pending", "<div><table><tr><td>eve</td></tr></table></div>"),
            ),
        ]);

        let (shape, export) = read_export(zipped.path()).unwrap();
        assert_eq!(shape, Shape::Html);
        let analysis = analyze(&export);
        assert_eq!(analysis.fans, vec!["ann"]);
        assert_eq!(analysis.friends, vec!["bob"]);
        assert_eq!(analysis.unfollowers, vec!["dan"]);
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn an_archive_without_the_lists_says_where_they_should_be() {
        let zipped = archive(&[("personal_information.json", "{}".into())]);
        let error = read_export(zipped.path()).unwrap_err().to_string();
        assert!(error.contains("followers_and_following"), "{error}");
    }

    #[cfg(feature = "xlsx")]
    #[test]
    fn a_file_that_is_not_an_archive_is_reported_as_such() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not a zip at all").unwrap();
        let error = read_export(file.path()).unwrap_err().to_string();
        assert!(error.contains("zip"), "{error}");
    }

    /// Without the `zip` crate the archive cannot be opened, and the message
    /// has to name the way around it.
    #[cfg(not(feature = "xlsx"))]
    #[test]
    fn a_build_without_zip_says_to_extract_the_archive() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"PK").unwrap();
        let error = read_export(file.path()).unwrap_err().to_string();
        assert!(error.contains("extract"), "{error}");
    }

    /// The counts have to add up the same way the live commands' do.
    #[test]
    fn the_arithmetic_holds() {
        let export = Export {
            followers: ["a", "b", "c"].iter().map(|s| s.to_string()).collect(),
            following: ["b", "c", "d", "e"].iter().map(|s| s.to_string()).collect(),
        };
        let a = analyze(&export);
        assert_eq!(a.fans.len() + a.friends.len(), a.followers);
        assert_eq!(a.unfollowers.len() + a.friends.len(), a.following);
    }

    fn one_of_each() -> Analysis {
        analyze(&Export {
            followers: HashSet::from(["ann".to_string(), "bob".to_string()]),
            following: HashSet::from(["bob".to_string(), "dan".to_string()]),
        })
    }

    fn rendered_text(analysis: &Analysis, format: Format) -> String {
        let Rendered::Text(text) = render(analysis, format).unwrap() else {
            panic!("every import form is text");
        };
        text
    }

    #[test]
    fn the_json_output_names_where_it_came_from() {
        let value: Value =
            serde_json::from_str(&rendered_text(&one_of_each(), Format::Json)).unwrap();
        assert_eq!(value["source"], "dyi");
        assert_eq!(value["counts"]["unfollowers"], 1);
        assert_eq!(value["unfollowers"][0], "dan");
        assert_eq!(value["fans"][0], "ann");
        assert_eq!(value["friends"][0], "bob");
    }

    #[test]
    fn the_table_lists_the_unfollowers_under_the_counts() {
        let table = rendered_text(&one_of_each(), Format::Table);
        assert!(table.contains("Followers:    2\n"), "{table}");
        assert!(table.contains("Unfollowers:  1\n"), "{table}");
        assert!(table.ends_with("\ndan\n"), "{table}");
    }

    /// The whole reader, end to end: an extracted export in, the table out.
    #[test]
    fn an_export_reads_through_to_the_table() {
        let dir = small_export();
        let (_, export) = read_export(dir.path()).unwrap();
        let table = rendered_text(&analyze(&export), Format::Table);
        assert!(table.contains("Followers:    2\n"), "{table}");
        assert!(table.ends_with("\ndan\n"), "{table}");
    }
}
