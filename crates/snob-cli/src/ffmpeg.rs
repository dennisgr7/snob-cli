//! ffmpeg, which joins a video's best tracks into one file.
//!
//! A post's widest video is a video track and a sound track apart
//! (`snob_ig::model::dash`), and one file that plays needs both in it. snob
//! never bundles ffmpeg and never re-encodes: it starts the ffmpeg the person
//! already has, as it starts the system viewer, to copy both streams into one
//! MP4 untouched (`-c copy`), which is disk speed. Without one, a download is
//! the progressive file, and the person is told once how to get ffmpeg.
//!
//! How it is started is the rule here: by an absolute path (`--ffmpeg`,
//! [`ENV`], or a `PATH` entry that is absolute), with no shell, standard input
//! and output closed, every path given absolute so none can be read as an
//! option, its errors kept to [`STDERR_KEPT`] bytes, and stopped, by its own
//! handle, on Ctrl+C or past [`WITHIN`].

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use snob_ig::error::IgError;
use snob_ig::pace::CancelToken;

/// The variable that names the ffmpeg to use, past the `PATH`.
pub const ENV: &str = "SNOB_FFMPEG";

/// The longest a join may take. A stream copy of the largest best track of
/// the capture of 2026-11-13 (210.6 MB) is seconds of disk; five minutes is
/// a disk that is not working, and the join is then given up.
pub const WITHIN: Duration = Duration::from_secs(5 * 60);

/// How much of what ffmpeg says on failure is kept for the sentence.
const STDERR_KEPT: usize = 4 * 1024;

/// How often a running join is looked at, for its end, a Ctrl+C or its time.
const POLL: Duration = Duration::from_millis(100);

/// The program's name on this system.
const NAME: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};

/// An ffmpeg found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ffmpeg {
    path: PathBuf,
}

impl Ffmpeg {
    /// The ffmpeg to use: `named` (`--ffmpeg`), else [`ENV`], else the first
    /// on the `PATH`. A name that is not a file is none, and so is a relative
    /// one: what a relative path names depends on the directory snob was
    /// started in.
    pub fn find(named: Option<&Path>) -> Option<Self> {
        let given = named
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(ENV).map(PathBuf::from));
        if let Some(path) = given {
            return (path.is_absolute() && path.is_file()).then_some(Self { path });
        }
        on_path(&std::env::var_os("PATH")?).map(|path| Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `video` and `audio` copied into one MP4 at `out`, untouched. Stops,
    /// and answers `IgError::Canceled`, when `cancel` is, and fails past
    /// [`WITHIN`]; `out` is then left for the caller to remove.
    pub async fn join(
        &self,
        video: &Path,
        audio: &Path,
        out: &Path,
        cancel: &CancelToken,
    ) -> Result<()> {
        let paths = [video, audio, out]
            .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
        let mut command = Command::new(&self.path);
        command
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&paths[0])
            .arg("-i")
            .arg(&paths[1])
            // The first video of the first input and the first sound of the
            // second, copied; `+faststart` puts the index first so a player
            // starts before the end is read; `-f mp4` because the file it
            // writes ends in `.part`.
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-f",
                "mp4",
            ])
            .arg(&paths[2])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || run(command, &cancel, WITHIN))
            .await
            .map_err(|e| anyhow!("ffmpeg could not be waited for: {e}"))?
    }
}

/// The first `ffmpeg` in the absolute entries of `path`, a `PATH`.
fn on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(NAME))
        .find(|candidate| candidate.is_file())
}

/// Runs `command` to its end, stopping it on `cancel` or past `within`.
fn run(mut command: Command, cancel: &CancelToken, within: Duration) -> Result<()> {
    let mut child = command
        .spawn()
        .map_err(|e| anyhow!("ffmpeg would not start: {e}"))?;
    // Read on a thread of its own, so a child that says a lot never stops on
    // a full pipe; what is past the first few kilobytes is read and dropped.
    let said = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut stderr = stderr;
            let _ = (&mut stderr)
                .take(STDERR_KEPT as u64)
                .read_to_end(&mut kept);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
            kept
        })
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if cancel.is_canceled() => break Err(IgError::Canceled.into()),
            Ok(None) if started.elapsed() > within => {
                break Err(anyhow!(
                    "ffmpeg took longer than {} minutes",
                    within.as_secs() / 60
                ));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => break Err(anyhow!("ffmpeg could not be waited for: {e}")),
        }
    };
    if status.is_err() {
        // By its own handle, never a pid read from anywhere.
        let _ = child.kill();
        let _ = child.wait();
    }
    let said = said
        .and_then(|reader| reader.join().ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .unwrap_or_default();
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let why = snob_core::model::printable(said.lines().last().unwrap_or("no reason given"));
    Err(anyhow!(
        "ffmpeg could not join the tracks ({status}): {why}"
    ))
}

/// How to install ffmpeg here, for the one line that says it.
pub fn how_to_install() -> &'static str {
    if cfg!(windows) {
        "winget install Gyan.FFmpeg, or scoop install ffmpeg"
    } else if cfg!(target_os = "macos") {
        "brew install ffmpeg"
    } else {
        "your system's package manager (apt install ffmpeg, dnf install ffmpeg, pacman -S ffmpeg)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_is_found_on_an_absolute_path_entry_only() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join(NAME);
        std::fs::write(&program, b"").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(on_path(&path), Some(program.clone()));
        // A relative entry is not searched, whatever it holds.
        let relative = std::env::join_paths([Path::new("relative-dir")]).unwrap();
        assert_eq!(on_path(&relative), None);
        let empty = tempfile::tempdir().unwrap();
        let none = std::env::join_paths([empty.path()]).unwrap();
        assert_eq!(on_path(&none), None);
    }

    #[test]
    fn a_named_ffmpeg_wins_and_must_be_an_absolute_file() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join(NAME);
        std::fs::write(&program, b"").unwrap();
        assert_eq!(
            Ffmpeg::find(Some(&program)).map(|f| f.path().to_path_buf()),
            Some(program.clone())
        );
        assert_eq!(Ffmpeg::find(Some(Path::new(NAME))), None);
        assert_eq!(Ffmpeg::find(Some(&dir.path().join("missing"))), None);
    }

    /// A program that fails says why, kept short; one that is canceled
    /// stops at once and says it was canceled.
    #[test]
    fn a_join_that_fails_says_why_and_a_canceled_one_stops() {
        let cancel = CancelToken::default();
        let failing = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo no such file 1>&2 & exit 1"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo no such file >&2; exit 1"]);
            c
        };
        let mut failing = failing;
        failing
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let failed = run(failing, &cancel, WITHIN).unwrap_err().to_string();
        assert!(failed.contains("no such file"), "{failed}");

        let mut waiting = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 30 127.0.0.1 >NUL"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 30"]);
            c
        };
        waiting
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cancel.cancel();
        let started = Instant::now();
        let stopped = run(waiting, &cancel, WITHIN).unwrap_err();
        assert!(
            stopped
                .chain()
                .any(|c| matches!(c.downcast_ref::<IgError>(), Some(IgError::Canceled))),
            "{stopped}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
