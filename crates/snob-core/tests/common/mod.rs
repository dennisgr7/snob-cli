//! The walk the source-reading guards share.
//!
//! Four tests in this directory read the repository's own source rather than
//! calling into the library: `keyring.rs` checks that no test points at the
//! real keyring, `sandbox.rs` that the sandbox seam stays behind its feature,
//! `no_seen.rs` that no call reports what was viewed, and `language.rs` that
//! no Spanish and no punched-out sentence is left in anything shipped. None of
//! them can be done any other way — a runtime assertion cannot see a file that
//! was never compiled into this binary — and all of them need the same awkward
//! part: find the repository, walk it, skip the same directories, and name a
//! file the same way on every platform.
//!
//! One walk rather than a copy per guard: these functions decide what "the
//! source of this repository" means, and a copy that drifted would be a guard
//! quietly walking less than it says.
//!
//! **The allowlists stay where they are.** Each names files exempt from one
//! guard for that guard's own reason, and a shared list would read as though
//! the exemptions were about the file rather than about the rule.
//!
//! A `tests/common/mod.rs` is compiled separately into each binary that
//! declares it, so anything one of them does not touch is dead code there.
//! `allow` rather than `expect`, for the reason the CLI's copy records: whether
//! something is in fact unused differs per binary.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Walks up from the manifest until a `Cargo.lock` shows up.
///
/// `None` from a packaged build, where there is no repository to walk and the
/// guard has nothing to say rather than something to fail about.
pub fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// Directories the walk never descends into, and never takes a new file
/// from.
///
/// `target` and `.git` are the obvious ones. The rest are where a developer's
/// own files live, and with `json` in the extension list the walk reaches
/// things that are nobody's source: an editor's settings, and — the one that
/// matters — an Instagram data export or a `snob followers -o out.json` written
/// from the repository root. Those are full of real names with real accents,
/// so a guard would fail on them **and print them into the assertion
/// message**. That is the "permanent noise and someone switches it off"
/// outcome the language rule warns about, arriving with someone else's
/// personal data attached.
pub const SKIP_DIRS: [&str; 6] = ["target", ".git", ".claude", ".vscode", ".idea", "exports"];

/// Every file of the repository at `root` whose extension is one of
/// `extensions`.
///
/// **What git would commit:** every tracked file, whatever `.gitignore` says,
/// and every new one it does not ignore. `.gitignore` names the scratch that
/// is nobody's source — `/docs/` holds a developer's own notes, in whatever
/// language they were written — and a guard that read it would fail on a
/// file that is never shipped. Without git, a walk of the directory, which
/// knows the scratch only by name.
pub fn source_files(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    listed_by_git(root)
        .unwrap_or_else(|| walked(root))
        .into_iter()
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| extensions.contains(&e))
        })
        .collect()
}

/// The tracked files and the new ones git does not ignore, or `None` where
/// git cannot say: not installed, or not a repository.
fn listed_by_git(root: &Path) -> Option<Vec<PathBuf>> {
    let list = |which: &[&str]| -> Option<Vec<String>> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-z"])
            .args(which)
            .output()
            .ok()?;
        out.status.success().then(|| {
            out.stdout
                .split(|b| *b == 0)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect()
        })
    };
    let tracked = list(&["--cached"])?;
    // An editor's settings are not ignored everywhere, and are not source.
    let new = list(&["--others", "--exclude-standard"])?
        .into_iter()
        .filter(|name| !name.split('/').any(|part| SKIP_DIRS.contains(&part)));
    Some(
        tracked
            .into_iter()
            .chain(new)
            .map(|name| root.join(name))
            // A tracked file deleted in the working tree has nothing to read.
            .filter(|path| path.is_file())
            .collect(),
    )
}

/// Every file under `root`, skipping [`SKIP_DIRS`] and the notes directory
/// `.gitignore` declares at the root.
fn walked(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && path != root.join("docs") {
                    pending.push(path);
                }
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// How a guard names a file: relative to the repository root, forward slashes
/// whichever platform it ran on.
///
/// Both halves matter. The allowlists are written with forward slashes, so on
/// Windows an unnormalized path matches none of them and every exempt file is
/// reported; and the assertion message is read by a person who wants a path
/// they can open, not one rooted at somebody's home directory.
pub fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The walk reaches this repository's own source, and stops where it says.
///
/// Every guard in this directory is worth exactly what the walk finds, and a
/// walk that finds nothing passes all of them in silence — which is the shape
/// of defect they exist to catch, arriving in the one place nothing was
/// watching. Every binary that includes this file runs it.
#[test]
fn the_walk_reaches_the_source_it_is_meant_to_read() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let found: Vec<String> = source_files(&root, &["rs"])
        .iter()
        .map(|file| relative(&root, file))
        .collect();

    for anchor in [
        "crates/snob-core/src/lib.rs",
        // One anchor per crate, so adding a crate to the workspace and not to
        // this list is what fails rather than what goes unnoticed: every guard
        // that reads sources has to reach every crate, or it is checking part
        // of the tree.
        "crates/snob-store/src/lib.rs",
        "crates/snob-cli/src/main.rs",
        "crates/snob-ig/src/client/mod.rs",
    ] {
        assert!(
            found.iter().any(|f| f == anchor),
            "the walk did not reach {anchor}, so it is not reading this repository"
        );
    }

    assert!(
        !found.iter().any(|f| f.starts_with("target/")),
        "the walk descended into a build directory"
    );

    // The extension filter is a filter rather than a suggestion.
    assert!(
        source_files(&root, &["rs"])
            .iter()
            .all(|f| f.extension().is_some_and(|e| e == "rs")),
        "the walk returned something that is not what was asked for"
    );
}

/// A directory of the test's own, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("snob-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The walk reads what git would commit: a tracked file even where
/// `.gitignore` covers it, and a new one it does not ignore, but not the
/// ignored notes nor an editor's settings.
#[test]
fn the_walk_reads_what_git_would_commit() {
    let scratch = Scratch::new("walk");
    let root = scratch.0.as_path();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .is_ok_and(|out| out.status.success())
    };
    if !git(&["init", "--quiet"]) {
        return; // no git here, so the directory walk is what runs
    }
    for (name, text) in [
        (".gitignore", "/docs/\n/tracked.rs\n"),
        ("docs/notes.rs", ""),
        ("tracked.rs", ""),
        ("src/new.rs", ""),
        (".vscode/settings.rs", ""),
    ] {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
        std::fs::write(&path, text).expect("a file");
    }
    assert!(git(&["add", "--force", "tracked.rs"]));

    let mut found: Vec<String> = source_files(root, &["rs"])
        .iter()
        .map(|file| relative(root, file))
        .collect();
    found.sort();
    assert_eq!(found, ["src/new.rs", "tracked.rs"]);
}
