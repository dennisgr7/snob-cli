//! What the updater knows, in `update.json` in the data directory, and the
//! files it keeps beside it.
//!
//! **One file, for snob and for anything that shows snob's state** (an app
//! around it, `snob status --json`, `snob update --status --json`): whether
//! a version is out, whether it is downloaded and how it will be applied,
//! what was applied last and what went wrong. Written whole and renamed into
//! place (`paths::replace_private`), so a reader sees one version or the
//! next; every change is a read, an edit and a write under `update.lock`
//! ([`change`]), since the background download and a command noting what it
//! told the person write it from two processes.
//!
//! A file that will not read is no state: the updater starts again from
//! nothing, which costs one check.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use snob_store::paths::AppPaths;

/// Which shape of the file this is.
pub(crate) const SCHEMA: u32 = 1;

/// How much the updater does on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Auto {
    /// Nothing: no check, no download, no notice.
    Off,
    /// Check once a day and say when a version is out.
    Notify,
    /// Check, and download a version ahead of applying it.
    Download,
    /// Check, download, and apply when snob next starts with no other snob
    /// running.
    #[default]
    Apply,
}

impl Auto {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Auto::Off => "off",
            Auto::Notify => "notify",
            Auto::Download => "download",
            Auto::Apply => "apply",
        }
    }
}

/// A version downloaded and checked, waiting to be applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Staged {
    pub(crate) version: String,
    /// The new binary (a script install's) or package (the `.deb`), when
    /// snob downloaded it; none when the package manager holds it.
    pub(crate) file: Option<PathBuf>,
    /// The file's SHA-256, checked again before it is applied.
    pub(crate) sha256: Option<String>,
    /// When it was staged, in seconds since the epoch.
    pub(crate) at: i64,
}

/// The last version applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Applied {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) at: i64,
    /// Whether a person has been told, once.
    #[serde(default)]
    pub(crate) told: bool,
}

/// The last thing that went wrong, until something goes right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Failure {
    pub(crate) at: i64,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct State {
    pub(crate) schema: u32,
    pub(crate) auto: Auto,
    /// When GitHub was last asked, whatever it answered.
    pub(crate) last_check: Option<i64>,
    /// GitHub's tag for the answer it gave, so an unchanged one costs it
    /// nothing to give again.
    pub(crate) etag: Option<String>,
    /// The newest version GitHub has named. Never moves back, so an older
    /// release named later is no version to move to.
    pub(crate) latest: Option<String>,
    pub(crate) staged: Option<Staged>,
    pub(crate) applied: Option<Applied>,
    pub(crate) error: Option<Failure>,
    /// The version a person was last told about, so each is said once.
    pub(crate) told: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            auto: Auto::default(),
            last_check: None,
            etag: None,
            latest: None,
            staged: None,
            applied: None,
            error: None,
            told: None,
        }
    }
}

/// Where the updater keeps its files: all of them in the data directory, so
/// `snob purge` takes them with it.
#[derive(Debug, Clone)]
pub(crate) struct Files {
    data: PathBuf,
}

impl Files {
    pub(crate) fn of(paths: &AppPaths) -> Self {
        Self {
            data: paths.data_dir().to_path_buf(),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(data: &Path) -> Self {
        Self {
            data: data.to_path_buf(),
        }
    }

    pub(crate) fn state(&self) -> PathBuf {
        self.data.join("update.json")
    }

    /// Held for every change to the state, briefly.
    fn state_lock(&self) -> PathBuf {
        self.data.join("update.lock")
    }

    /// Held by the one process that checks and downloads, for as long as it
    /// does.
    pub(crate) fn fetch_lock(&self) -> PathBuf {
        self.data.join("update-fetch.lock")
    }

    /// Held shared by every snob that counts as running, and alone by the
    /// one that applies a version (`running`).
    pub(crate) fn running_lock(&self) -> PathBuf {
        self.data.join("running.lock")
    }

    /// The updater's own directory.
    pub(crate) fn dir(&self) -> PathBuf {
        self.data.join("update")
    }

    /// The one version downloaded and waiting.
    pub(crate) fn staged_dir(&self) -> PathBuf {
        self.dir().join("staged")
    }

    /// What the background check and a package manager's upgrade said.
    pub(crate) fn log(&self) -> PathBuf {
        self.dir().join("update.log")
    }
}

/// The state as stored, or a fresh one.
pub(crate) fn load(files: &Files) -> State {
    read(&files.state())
}

fn read(path: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(path) else {
        return State::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::debug!(error = %e, "update.json does not read; starting again");
        State::default()
    })
}

/// Reads the state, lets `edit` change it, and writes it back, all under
/// `update.lock`. Returns what `edit` returned and the state as written.
pub(crate) fn change<T>(
    files: &Files,
    edit: impl FnOnce(&mut State) -> T,
) -> std::io::Result<(T, State)> {
    snob_store::paths::create_private_dir(&files.data).map_err(std::io::Error::other)?;
    let _held = snob_store::paths::lock_file(&files.state_lock())?;
    let mut state = read(&files.state());
    let answer = edit(&mut state);
    state.schema = SCHEMA;
    let text = serde_json::to_vec_pretty(&state).map_err(std::io::Error::other)?;
    let temporary = files
        .data
        .join(format!("update.json.{}.tmp", std::process::id()));
    snob_store::paths::replace_private(&files.state(), &temporary, &text).map_err(|(_, e)| e)?;
    Ok((answer, state))
}

/// Now, in seconds since the epoch.
pub(crate) fn now() -> i64 {
    snob_core::clock::now_ms().get() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_written_reads_back_the_same() {
        let tmp = tempfile::tempdir().unwrap();
        let files = Files::at(tmp.path());
        let ((), written) = change(&files, |state| {
            state.auto = Auto::Notify;
            state.latest = Some("0.10.0".into());
            state.staged = Some(Staged {
                version: "0.10.0".into(),
                file: Some("/x/snob.new".into()),
                sha256: Some("ab".into()),
                at: 7,
            });
        })
        .unwrap();
        assert_eq!(load(&files), written);
        assert_eq!(written.schema, SCHEMA);
    }

    /// A damaged or missing file is a fresh start, never a failure.
    #[test]
    fn a_file_that_does_not_read_is_a_fresh_state() {
        let tmp = tempfile::tempdir().unwrap();
        let files = Files::at(tmp.path());
        assert_eq!(load(&files), State::default());
        std::fs::write(files.state(), "{not json").unwrap();
        assert_eq!(load(&files), State::default());
    }

    /// A field a later snob adds, or one this one does not know, does not
    /// make the file unreadable; one it leaves out takes its default.
    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let files = Files::at(tmp.path());
        std::fs::write(files.state(), r#"{"auto":"off","later":1}"#).unwrap();
        let state = load(&files);
        assert_eq!(state.auto, Auto::Off);
        assert_eq!(state.latest, None);
    }
}
