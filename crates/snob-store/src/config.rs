//! The monitor's configuration file.
//!
//! The one file in the configuration directory, which is created by whatever
//! writes there and not before, so [`write`] creates it.
//!
//! **Read with serde, written from a template by hand.** A serializer produces
//! a correct file and a useless one: it cannot put the reason for a value next
//! to the value, and this project puts the reason next to the thing it governs
//! everywhere else. A file somebody is meant to edit has to explain itself, so
//! the template is written out and the parser is what has to keep up with it —
//! and a test writes the template and reads it back, so it cannot drift.
//!
//! **No secrets live here.** The webhook's token and signing key go to the
//! keyring beside the session, because this file is plain text at a guessable
//! path and a token in it is a token in every backup of the home directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::paths::AppPaths;
use snob_core::duration;
use snob_core::{Epoch, Pk};

/// What the file says.
///
/// `deny_unknown_fields` throughout, and that is not pedantry: this file drives
/// something that runs unattended for months, and a mistyped key that is
/// silently ignored is a schedule nobody is running or a consent nobody gave.
/// Better to refuse at startup, where somebody is watching.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    /// Which shape of this file it is. Read before anything else, so a future
    /// version can recognize an older one instead of failing at a field.
    #[serde(default = "one")]
    pub schema: u32,

    /// How often to run: `6h`, `2d`, `2w`.
    #[serde(default, deserialize_with = "duration_opt")]
    pub every: Option<Duration>,
    /// Times of day, `09:00`.
    #[serde(default)]
    pub at: Vec<String>,
    /// Days of the week, `mon`.
    #[serde(default)]
    pub on: Vec<String>,
    /// A five-field cron expression.
    #[serde(default)]
    pub cron: Option<String>,
    /// How far a run may be pushed later.
    #[serde(default, deserialize_with = "duration_opt")]
    pub jitter: Option<Duration>,

    #[serde(default)]
    pub webhook: Option<WebhookConfig>,

    /// The accounts to watch. Empty means your own.
    #[serde(default, rename = "account")]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    pub url: String,
    /// Extra headers, as a table. The values here are **not** secrets: a token
    /// belongs in the keyring, and `snob watch setup` puts it there.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub heartbeat: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// The username, or `self` for the account the session belongs to.
    pub target: String,
    /// The signed-in account this entry is read as. Absent, the account snob
    /// is using.
    #[serde(default)]
    pub viewer: Option<Pk>,
    /// The recorded answer to "may this walk somebody else's lists?".
    ///
    /// A table rather than a bool, because what has to be on record is that
    /// somebody answered and when — a `true` is something any editor can type
    /// without having been asked anything.
    #[serde(default)]
    pub consent: Option<ConsentConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConsentConfig {
    /// When it was given.
    pub agreed_at: Epoch,
}

impl AccountConfig {
    /// Whether this line names the account the session belongs to.
    ///
    /// The at sign comes off first, because everywhere else in the tool it does:
    /// `@self` is what somebody writes who has just written `@friend` on the
    /// line above. Without it that line is read as a stranger named `self`, and
    /// a scheduled run refuses to start asking for confirmation to read an
    /// account it owns.
    pub fn is_own(&self) -> bool {
        self.target
            .trim_start_matches('@')
            .eq_ignore_ascii_case("self")
    }
}

fn one() -> u32 {
    1
}

/// The shape this version writes. [`parse`] also reads schema 1, the
/// single-account shape, whose entries name no viewer.
pub const SCHEMA: u32 = 2;

/// Whether this version reads a file of that schema.
fn understood(schema: u32) -> bool {
    (1..=SCHEMA).contains(&schema)
}

/// Durations are written the way a person types them, so they are parsed the
/// same way rather than as a number of seconds.
fn duration_opt<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let text = Option::<String>::deserialize(deserializer)?;
    text.map(|t| duration::parse(&t).map_err(D::Error::custom))
        .transpose()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} could not be read: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(
        "{path} says it is schema {found}, and this version of snob understands up to {SCHEMA}.\n\
         It was probably written by a newer snob; update, or move that file aside."
    )]
    Unknown { path: PathBuf, found: u32 },
    #[error(transparent)]
    Paths(#[from] crate::paths::PathError),
}

/// Where the file lives.
pub fn path(paths: &AppPaths) -> PathBuf {
    paths.config_dir().join("watch.toml")
}

/// Reads it, if it is there.
pub fn load(paths: &AppPaths) -> Result<Option<WatchConfig>, ConfigError> {
    let path = path(paths);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    parse(&text, &path).map(Some)
}

/// Parses one, naming the file in anything it complains about.
pub fn parse(text: &str, path: &Path) -> Result<WatchConfig, ConfigError> {
    // **The schema is read first.** `WatchConfig` carries `deny_unknown_fields`,
    // and a file from a newer version is by definition one with keys this build
    // has never heard of: parsed whole first, it would be refused as a typo
    // ("unknown field `something_new`") rather than as a newer file.
    //
    // Two lines and no `deny_unknown_fields`, so every other key is ignored. A
    // missing schema key reaches the full parse, where it reads as schema 1.
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        schema: u32,
    }
    if let Ok(Version { schema }) = toml::from_str::<Version>(text)
        && !understood(schema)
        && schema != 0
    {
        return Err(ConfigError::Unknown {
            path: path.to_path_buf(),
            found: schema,
        });
    }

    let config: WatchConfig = toml::from_str(text).map_err(|e| ConfigError::Invalid {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    if !understood(config.schema) {
        return Err(ConfigError::Unknown {
            path: path.to_path_buf(),
            found: config.schema,
        });
    }

    // `cron` and the `at`/`on` pair are two syntaxes for the same thing, and
    // only one of them can win. `schedule_from` takes `cron` first while the
    // sentence the monitor prints is built from every field that is populated,
    // so the banner would describe a schedule nobody is on. The flags cannot
    // reach this state, `cli.rs` declares them as conflicting; the file can,
    // and `deny_unknown_fields` is on this struct for exactly the reason that
    // applies here, that a schedule nobody is running must not be accepted in
    // silence.
    //
    // `every` alongside a calendar stays legal: those two combine rather than
    // compete, which is what `--every 2w --on mon` means.
    if config.cron.is_some() && !(config.at.is_empty() && config.on.is_empty()) {
        let other = if config.at.is_empty() { "on" } else { "at" };
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "\"cron\" and \"{other}\" are two ways of saying the same thing, and only \
                 \"cron\" would be used. Keep whichever one you meant."
            ),
        });
    }

    Ok(config)
}

/// Writes the file, creating the configuration directory if it is not there:
/// `AppPaths::ensure_dirs` leaves that directory to what writes there.
pub fn write(paths: &AppPaths, contents: &str) -> Result<PathBuf, ConfigError> {
    let path = path(paths);
    if let Some(parent) = path.parent() {
        // `create_private_dir` rather than `create_dir_all`, which is what the
        // data directory already uses. This holds the webhook address — which
        // for an n8n, Slack or Discord hook *is* the credential — the accounts
        // being watched and when consent was given, and 0755 would let every
        // other account on the machine read all three.
        crate::paths::create_private_dir(parent)?;
    }

    // Written whole and then moved into place, and readable only by its owner:
    // a process that dies mid-write must not leave a half-written schedule
    // that the next start refuses to parse.
    let temporary = path.with_extension("toml.new");
    crate::paths::replace_private(&path, &temporary, contents.as_bytes())
        .map_err(|(path, source)| ConfigError::Write { path, source })?;
    Ok(path)
}

/// Renders a configuration file a person can read and edit.
///
/// Written out rather than serialized, so every value can carry the sentence
/// that explains it. What comes back has to parse — [`parse`] is what reads it
/// — and a test writes one of these and reads it back for exactly that reason.
/// **It takes the type [`parse`] produces, so the round trip is a comparison
/// rather than a description of one**, and so it cannot be handed what the
/// file cannot hold: a list of header pairs can name one header twice, and
/// `[webhook.headers]` is a TOML table that cannot, so the wizard would write
/// a file it could not read back after every question had been answered.
///
/// `signed` stays a parameter, because it is not in the file and must not be: it
/// says a key went into the keyring, and all this writes is the sentence telling
/// the reader to look there.
///
/// `schema` is what this build writes rather than what the argument holds, so
/// the round trip is an equality for every configuration of this schema.
///
/// `name_of` gives the username beside each viewer's id, for the reader: the
/// id is what the file means, and a number alone says nothing to a person.
pub fn template(
    config: &WatchConfig,
    signed: bool,
    name_of: impl Fn(Pk) -> Option<String>,
) -> String {
    let mut out = String::new();
    out.push_str(
        "# snob watch. Written by \"snob watch setup\", and safe to edit by hand.\n\
         #\n\
         # Times are your local ones. Durations are written the way you would say\n\
         # them: 30m, 6h, 2d, 2w.\n\n",
    );
    out.push_str(&format!("schema = {SCHEMA}\n\n"));

    out.push_str("# When to run.\n");
    if let Some(every) = config.every {
        out.push_str(&format!("every = \"{}\"\n", duration::format(every)));
    }
    if !config.on.is_empty() {
        out.push_str(&format!("on = [{}]\n", quoted_list(&config.on)));
    }
    if !config.at.is_empty() {
        out.push_str(&format!("at = [{}]\n", quoted_list(&config.at)));
    }
    if let Some(cron) = &config.cron {
        out.push_str(&format!("cron = {}\n", quote(cron)));
    }

    if let Some(jitter) = config.jitter {
        out.push_str(
            "\n# How far each run may be pushed past its due moment, so the walks do\n\
             # not start on the same second every day. \"0\" turns it off.\n",
        );
        out.push_str(&format!("jitter = \"{}\"\n", duration::format(jitter)));
    }

    if let Some(webhook) = &config.webhook {
        out.push_str("\n[webhook]\n");
        out.push_str(&format!("url = {}\n", quote(&webhook.url)));
        out.push_str(
            "# Send a report even when nothing changed, so something watching for\n\
             # silence can tell \"nothing happened\" from \"it stopped running\".\n",
        );
        out.push_str(&format!("heartbeat = {}\n", webhook.heartbeat));
        if signed {
            out.push_str(
                "# The body is signed: the key is in the system keyring, not here.\n\
                 # So is any token below that you gave to \"snob watch setup\".\n",
            );
        }
        if !webhook.headers.is_empty() {
            out.push_str("\n[webhook.headers]\n");
            for (name, value) in &webhook.headers {
                out.push_str(&format!("{} = {}\n", quote(name), quote(value)));
            }
        }
    }

    if config.accounts.iter().any(|a| a.viewer.is_some()) {
        out.push_str(
            "\n# viewer names the signed-in account an entry below is read as.\n\
             # An entry that names none is read as the account snob is using.\n",
        );
    }
    for account in &config.accounts {
        out.push_str("\n[[account]]\n");
        out.push_str(&format!("target = {}\n", quote(&account.target)));
        if let Some(viewer) = account.viewer {
            out.push_str(&format!("viewer = {viewer}"));
            // A comment ends at the line's end, so a name that could break
            // the line is left out rather than written.
            if let Some(name) = name_of(viewer).filter(|n| !n.chars().any(char::is_control)) {
                out.push_str(&format!("  # @{name}"));
            }
            out.push('\n');
        }
        if let Some(consent) = account.consent {
            out.push_str(
                "# You were asked whether this may read that account's lists, and you\n\
                 # said yes. A scheduled run cannot ask, so it reads this instead.\n",
            );
            out.push_str("[account.consent]\n");
            out.push_str(&format!("agreed_at = {}\n", consent.agreed_at));
        }
    }
    out
}

/// A new file as this version writes it: schema [`SCHEMA`] and nothing else.
/// Not what a file missing the `schema` key reads as, which is schema 1.
impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            every: None,
            at: vec![],
            on: vec![],
            cron: None,
            jitter: None,
            webhook: None,
            accounts: vec![],
        }
    }
}

impl WatchConfig {
    /// Brings a schema-1 file to this schema. Every entry in it was read as
    /// the one account there was, so each now names it, and an empty list,
    /// which meant that account's own lists, says so.
    pub fn upgrade(&mut self, viewer: Pk) {
        if self.accounts.is_empty() {
            self.accounts.push(AccountConfig {
                target: "self".to_string(),
                viewer: None,
                consent: None,
            });
        }
        for account in &mut self.accounts {
            account.viewer.get_or_insert(viewer);
        }
        self.schema = SCHEMA;
    }
}

/// A TOML array of basic strings.
///
/// Through [`quote`] rather than a bare `"{v}"`. Every day and time in one is
/// validated before it can reach here, so
/// nothing can carry a quote today — and the file's own first line invites
/// hand-editing, which is the route by which "nothing can" stops being true.
fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| quote(value))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A TOML basic string. The values here come from a person, so a quote or a
/// backslash in one has to survive being written and read back.
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Every other control character, as the unicode escape TOML
            // defines for it: left raw, one -- a form feed pasted into a
            // webhook header, say -- would make the file unparseable after
            // setup had reported success and put both secrets in the keyring.
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What setup writes has to read back, whatever was pasted into it.
    #[test]
    fn a_quoted_value_with_any_control_character_reads_back() {
        let pasted = "a\u{0c}b\"c\\d\ne\u{1b}[2K";
        let text = format!(
            "schema = 1\nevery = \"6h\"\n\n[webhook]\nurl = \"https://h.test/x\"\n[webhook.headers]\nX-Note = {}\n",
            quote(pasted)
        );
        let config = parse(&text, std::path::Path::new("watch.toml")).expect("it parses");
        assert_eq!(
            config
                .webhook
                .unwrap()
                .headers
                .get("X-Note")
                .map(String::as_str),
            Some(pasted)
        );
    }

    fn at(text: &str) -> Result<WatchConfig, ConfigError> {
        parse(text, Path::new("watch.toml"))
    }

    #[test]
    fn it_reads_an_interval_and_a_webhook() {
        let config = at(r#"
schema = 1
every = "6h"
jitter = "15m"

[webhook]
url = "https://n8n.local/webhook/snob"
heartbeat = true

[webhook.headers]
"X-Source" = "homelab"
"#)
        .unwrap();

        assert_eq!(config.every, Some(Duration::from_secs(21_600)));
        assert_eq!(config.jitter, Some(Duration::from_secs(900)));
        let webhook = config.webhook.unwrap();
        assert_eq!(webhook.url, "https://n8n.local/webhook/snob");
        assert!(webhook.heartbeat);
        assert_eq!(webhook.headers["X-Source"], "homelab");
    }

    /// Two syntaxes for the same thing, and only one of them would be used;
    /// `parse` says why the pair is refused.
    #[test]
    fn a_file_cannot_name_a_schedule_twice() {
        let both = at(r#"
schema = 1
cron = "0 9 * * 1"
at = ["21:00"]
"#)
        .unwrap_err();
        let message = both.to_string();
        assert!(message.contains("cron"), "{message}");
        assert!(message.contains("at"), "{message}");

        assert!(
            at(r#"
schema = 1
cron = "0 9 * * 1"
on = ["thu"]
"#)
            .is_err(),
            "the day half names it twice just as much as the time half"
        );

        // An interval alongside a calendar is not the same thing: those combine,
        // which is what `--every 2w --on mon` means.
        assert!(
            at(r#"
schema = 1
cron = "0 9 * * 1"
every = "2w"
"#)
            .is_ok()
        );
    }

    #[test]
    fn it_reads_a_calendar() {
        let config = at(r#"
schema = 1
on = ["mon", "thu"]
at = ["09:00", "21:00"]
"#)
        .unwrap();
        assert_eq!(config.on, vec!["mon", "thu"]);
        assert_eq!(config.at, vec!["09:00", "21:00"]);
    }

    /// A typo in a file that drives something unattended for months would
    /// otherwise be a schedule nobody is running, discovered weeks later.
    #[test]
    fn a_key_that_is_not_a_key_is_refused_rather_than_ignored() {
        let error = at("schema = 1\nevry = \"6h\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("evry"), "{error}");
    }

    /// Checked before any field is read, so a newer file is refused as what it
    /// is rather than as a missing key.
    #[test]
    fn a_file_from_a_newer_version_says_so() {
        let error = at("schema = 99\n").unwrap_err();
        assert!(
            matches!(error, ConfigError::Unknown { found: 99, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("update"), "{error}");
    }

    /// And it is still refused for its schema when it carries a key this
    /// version has never had — which is what a newer file actually looks like,
    /// and what a full parse first would call a typo. The test above cannot
    /// tell the two orders apart, because `schema = 99` alone parses cleanly
    /// either way.
    #[test]
    fn a_newer_file_is_refused_for_its_schema_even_when_it_carries_a_key_this_version_never_had() {
        for text in [
            "schema = 99\nsomething_new = true\n",
            "schema = 99\n[webhook]\nurl = \"https://n8n.local/hook\"\nretries = 3\n",
        ] {
            let error = at(text).unwrap_err();
            assert!(
                matches!(error, ConfigError::Unknown { found: 99, .. }),
                "{text:?} was refused as a typo rather than as a newer file: {error}"
            );
        }

        // A key this version does not know, on a file that claims *this*
        // schema, is still a typo — which is what `deny_unknown_fields` is for.
        let typo = at("schema = 1\nsomething_new = true\n").unwrap_err();
        assert!(matches!(typo, ConfigError::Invalid { .. }), "{typo}");
    }

    #[test]
    fn a_duration_that_is_not_one_is_refused_where_it_is_written() {
        let error = at("schema = 1\nevery = \"six hours\"\n").unwrap_err();
        assert!(
            error.to_string().contains("not a valid duration"),
            "{error}"
        );
    }

    /// The record has to say somebody answered, not merely that the answer is
    /// yes — which is a thing any editor can type without having been asked.
    #[test]
    fn a_third_party_carries_when_it_was_agreed_to() {
        let config = at(r#"
schema = 1
every = "6h"

[[account]]
target = "self"

[[account]]
target = "someone"
[account.consent]
agreed_at = 1786925176
"#)
        .unwrap();

        assert_eq!(config.accounts.len(), 2);
        assert!(config.accounts[0].is_own());
        assert!(config.accounts[0].consent.is_none());
        assert_eq!(
            config.accounts[1].consent.unwrap().agreed_at,
            Epoch::new(1_786_925_176)
        );
    }

    /// What the template writes is what the parser reads -- the whole of it, as
    /// one comparison.
    ///
    /// The template is written by hand so it can explain itself, so nothing but
    /// a test stops it drifting away from the parser.
    ///
    /// Two configurations rather than one, because `cron` beside `at` or `on` is
    /// a file `parse` deliberately refuses -- so the two syntaxes cannot be
    /// covered by the same round trip and each has to have its own.
    #[test]
    fn what_the_template_writes_is_what_the_parser_reads() {
        let name_of = |pk: Pk| Some(format!("user{pk}"));
        let calendar = WatchConfig {
            schema: SCHEMA,
            every: Some(Duration::from_secs(1_209_600)),
            at: vec!["09:00".to_string(), "21:30".to_string()],
            on: vec!["mon".to_string(), "thu".to_string()],
            cron: None,
            jitter: Some(Duration::from_secs(900)),
            webhook: Some(WebhookConfig {
                url: r#"https://n8n.local/webhook/a"b\c"#.to_string(),
                headers: [
                    ("X-Source".to_string(), "homelab".to_string()),
                    ("X-Odd".to_string(), "a\"b".to_string()),
                ]
                .into_iter()
                .collect(),
                heartbeat: true,
            }),
            accounts: vec![
                AccountConfig {
                    target: "self".to_string(),
                    viewer: Some(Pk::new(1)),
                    consent: None,
                },
                AccountConfig {
                    target: "someone".to_string(),
                    viewer: Some(Pk::new(2)),
                    consent: Some(ConsentConfig {
                        agreed_at: Epoch::new(1_700_000_000),
                    }),
                },
            ],
        };
        assert_eq!(
            at(&template(&calendar, true, name_of)).expect("the template has to parse"),
            calendar,
            "a field the template does not write is a setting that disappears"
        );

        // The other syntax, and the smallest whole file: no jitter, no webhook,
        // no accounts. Each of those is an `if` in the template, and an `if`
        // with no test is a branch that can write anything.
        let cron = WatchConfig {
            cron: Some("0 9 * * 1,4".to_string()),
            ..Default::default()
        };
        assert_eq!(at(&template(&cron, false, name_of)).unwrap(), cron);

        // And `signed` is not in the file: it changes a sentence for the reader
        // and nothing the parser sees.
        assert_eq!(
            at(&template(&calendar, false, name_of)).unwrap(),
            at(&template(&calendar, true, name_of)).unwrap(),
            "the signing note is prose, not configuration"
        );
    }

    /// The default is the file this version writes with nothing in it.
    #[test]
    fn the_default_is_an_empty_file_of_this_schema() {
        assert_eq!(
            at(&format!("schema = {SCHEMA}\n")).unwrap(),
            WatchConfig::default()
        );
    }

    /// A file written before there were several accounts still reads, with no
    /// viewer anywhere in it; one from after reads with its viewers.
    #[test]
    fn both_schemas_read() {
        let entry = "[[account]]\ntarget = \"friend\"\n";
        let old = at(&format!("schema = 1\nevery = \"6h\"\n\n{entry}")).unwrap();
        assert_eq!(old.schema, 1);
        assert_eq!(old.accounts[0].viewer, None);

        let new = at(&format!(
            "schema = 2\nevery = \"6h\"\n\n{entry}viewer = 42\n"
        ))
        .unwrap();
        assert_eq!(new.schema, 2);
        assert_eq!(new.accounts[0].viewer, Some(Pk::new(42)));

        let newer = at("schema = 3\n").unwrap_err();
        assert!(
            matches!(newer, ConfigError::Unknown { found: 3, .. }),
            "{newer}"
        );
    }

    /// The viewer goes in its entry's own table, so it has to come before the
    /// consent table opens, and the name beside it is only a comment.
    #[test]
    fn the_viewer_is_written_with_its_name_before_the_consent() {
        let config = WatchConfig {
            every: Some(Duration::from_secs(21_600)),
            accounts: vec![AccountConfig {
                target: "friend".to_string(),
                viewer: Some(Pk::new(42)),
                consent: Some(ConsentConfig {
                    agreed_at: Epoch::new(1_700_000_000),
                }),
            }],
            ..Default::default()
        };
        let text = template(&config, false, |pk| {
            (pk == Pk::new(42)).then(|| "me".to_string())
        });
        let viewer = text.find("viewer = 42  # @me\n").expect(&text);
        let consent = text.find("[account.consent]").expect(&text);
        assert!(viewer < consent, "{text}");
        assert_eq!(at(&text).unwrap(), config);

        // A name that would end the comment early is left out.
        let text = template(&config, false, |_| Some("me\nevery = \"1m\"".to_string()));
        assert!(text.contains("viewer = 42\n"), "{text}");
        assert_eq!(at(&text).unwrap(), config);
    }

    /// Every entry of a schema-1 file was read as the one account there was,
    /// and an empty list meant that account's own lists.
    #[test]
    fn a_schema_1_file_upgrades_to_its_one_account() {
        let me = Pk::new(7);
        let mut listed = at(
            "schema = 1\nevery = \"6h\"\n\n[[account]]\ntarget = \"self\"\n\n\
             [[account]]\ntarget = \"friend\"\n",
        )
        .unwrap();
        listed.upgrade(me);
        assert_eq!(listed.schema, SCHEMA);
        assert!(listed.accounts.iter().all(|a| a.viewer == Some(me)));
        assert_eq!(listed.accounts.len(), 2);

        let mut empty = at("schema = 1\nevery = \"6h\"\n").unwrap();
        empty.upgrade(me);
        assert_eq!(
            empty.accounts,
            vec![AccountConfig {
                target: "self".to_string(),
                viewer: Some(me),
                consent: None,
            }]
        );
        assert_eq!(at(&template(&empty, false, |_| None)).unwrap(), empty);
    }

    /// A file with only a schedule is complete. Everything else is optional,
    /// and a parser that demanded a webhook would make the no-webhook mode --
    /// `snob watch --json >> events.ndjson` -- unconfigurable.
    #[test]
    fn a_schedule_on_its_own_is_a_whole_file() {
        let config = at("schema = 1\nevery = \"6h\"\n").unwrap();
        assert!(config.webhook.is_none());
        assert!(config.accounts.is_empty());
    }
}
