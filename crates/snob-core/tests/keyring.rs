//! No test may point at the real keyring.
//!
//! It belongs to the operating system rather than to this process, so a test
//! that reaches it deletes the session of whoever is developing, and on a
//! shared CI runner somebody else's. AGENTS.md carries the rule as a standing
//! instruction and says "a test checks that they do".
//!
//! A test cannot check that about other tests:
//! `tests_never_point_at_the_real_keyring` in `secrets.rs` sees only the store
//! it builds itself. And the free function `secrets::remove` calls
//! `delete_credential()` against the operating system's store whatever backend
//! was chosen, so a forgotten `with_service` anywhere takes the developer's
//! live entries with it while the suite stays green.
//!
//! So this reads the source instead. A runtime assertion cannot do the job: an
//! integration test compiles the library without `cfg(test)`, so `cfg!(test)`
//! inside `secrets.rs` is false in exactly the files that matter most.
//!
//! **What it checks**: every `SecretStore::new(` written in a test has
//! `.with_service(` in the same statement. Test means a file under a `tests/`
//! directory — all of which is test code by definition — or a line after the
//! first `#[cfg(test)]` in a `src/` file. That second half is a heuristic and
//! is meant to be: it is the cheap direction to be wrong in, since the worst it
//! does is ask for `with_service` on a line that did not need it.

mod common;
use common::{relative, repo_root, source_files};

/// Whether a `cfg` line is the start of test code.
///
/// Not only the literal `#[cfg(test)]`: `#[cfg(any(test, feature = "testing"))]`
/// is the gate a helper wears when both the unit tests and the sandbox build
/// need it, and therefore exactly the kind of place a store gets built.
///
/// **`#[cfg(feature = "testing")]` on its own is deliberately not test code.**
/// It ships in a `--features testing` binary, so an item behind it is
/// production for that build; `main.rs` has one above the real
/// `SecretStore::new`, and treating it as a boundary would declare the
/// program's own store a test violation. The `test` token is what separates
/// them, and it is looked for as a token so that the `test` inside `"testing"`
/// does not answer for it.
fn admits_a_test_build(line: &str) -> bool {
    let line = line.trim();
    if !line.starts_with("#[cfg") {
        return false;
    }
    // The feature's name contains the word, so it is removed before looking.
    let predicate = line.replace("feature = \"testing\"", "");
    predicate.match_indices("test").any(|(at, _)| {
        let before = predicate[..at].chars().next_back();
        let after = predicate[at + 4..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

#[test]
fn no_test_builds_a_secret_store_pointing_at_the_real_service() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut offenders = Vec::new();
    for file in source_files(&root, &["rs"]) {
        let relative = relative(&root, &file);

        if ALLOWLIST.contains(&relative.as_str()) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };
        offenders.extend(offenders_in(&relative, &contents));
    }

    assert!(
        offenders.is_empty(),
        "{} test(s) build a `SecretStore` without `.with_service(...)`, so they point at \
         the real keyring and will delete whatever is stored there:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The walk, over one file's text, so a test can hand it a fixture.
fn offenders_in(relative: &str, contents: &str) -> Vec<String> {
    let mut offenders = Vec::new();

    {
        // Everything under a `tests/` directory is test code. In `src/`, only
        // what comes after the first gate that admits test builds — which is
        // **not only `#[cfg(test)]`**: this project ships a `testing` feature
        // whose whole purpose is test code compiled into the binary, so a
        // helper behind `#[cfg(any(test, feature = "testing"))]` is exactly the
        // kind of place a store gets built.
        let in_a_test_file = relative.contains("/tests/");
        let mut reached_the_tests = in_a_test_file;

        // The offset is carried rather than searched for. `contents.find(line)`
        // answers with the **first** occurrence of that text, so two identical
        // `SecretStore::new(` lines would both be checked against the first
        // one's statement -- a shape the tree has, because rustfmt wraps a long
        // builder chain and leaves the call on a line of its own.
        // `split_inclusive` rather than `lines`, so the arithmetic is exact on
        // both line endings: `lines()` strips a trailing `\r` as well as the
        // `\n`, and adding a fixed one back drifts by a byte per line on a CRLF
        // file — which every file in this repository is, in the working tree.
        let mut offset = 0usize;
        for (number, raw) in contents.split_inclusive('\n').enumerate() {
            let at = offset;
            offset += raw.len();
            let line = raw.trim_end_matches(['\n', '\r']);

            if admits_a_test_build(line) {
                reached_the_tests = true;
            }
            if !reached_the_tests || !line.contains("SecretStore::new(") {
                continue;
            }
            // The escape hatch, in the shape the other guards in this directory
            // already use. Not expected to be needed: a test that reaches the
            // keyring on purpose still has to name a service of its own.
            if line.contains("keyring-allow") {
                continue;
            }
            // The statement, not the line: `cargo fmt` breaks a long builder
            // chain across several of them, so the `.with_service(` that
            // belongs to this call is usually not on the line the call is on.
            let statement = contents[at..].split(';').next().unwrap_or("");
            if !statement.contains(".with_service(") {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }

    offenders
}

/// The guard has to be able to fail, or it is a comment, so it is handed a
/// fixture and has to say which lines are wrong.
#[test]
fn the_guard_notices_a_store_built_without_a_service() {
    let fixture = "\
fn setup() {
    let ok = SecretStore::new(paths.clone(), true)
        .with_service(&service(name));
    let bad = SecretStore::new(paths.clone(), true);
}
";
    assert_eq!(
        offenders_in("crates/x/tests/y.rs", fixture),
        vec!["crates/x/tests/y.rs:4".to_string()],
        "line 2 is fine across the wrap, line 4 is not"
    );
}

/// Two identical calls are two calls, and the second is checked against its own
/// statement; `offenders_in` says why the offset is carried.
#[test]
fn a_repeated_call_is_not_excused_by_an_earlier_one() {
    // The two calls are byte-identical, which is the whole point: that is what
    // a copy-paste produces, and what `find` cannot tell apart.
    let fixture = "\
fn setup() {
    let store = SecretStore::new(paths.clone(), true)
        .with_service(&service(name));
    let store = SecretStore::new(paths.clone(), true)
        .using(Backend::File);
}
";
    assert_eq!(
        offenders_in("crates/x/tests/y.rs", fixture),
        vec!["crates/x/tests/y.rs:4".to_string()],
        "only the second is an offender, and it must not inherit the first's service"
    );
}

/// In `src/`, only what comes after the first `#[cfg(test)]` is test code —
/// production building its own store is the ordinary case.
#[test]
fn production_code_is_not_asked_to_name_a_test_service() {
    let fixture = "\
fn main() {
    let store = SecretStore::new(paths, true);
}

#[cfg(test)]
mod tests {
    fn t() {
        let store = SecretStore::new(paths, true);
    }
}
";
    assert_eq!(
        offenders_in("crates/x/src/main.rs", fixture),
        vec!["crates/x/src/main.rs:8".to_string()]
    );
}

/// Files exempt from the walk.
///
/// This file holds the shapes it is looking for, so it would flag itself.
const ALLOWLIST: [&str; 1] = ["crates/snob-core/tests/keyring.rs"];

/// The gate spellings, told apart.
///
/// The longer test gates are the easy ones to miss, and the feature gate on
/// its own must stay unrecognized: an item shipped behind the feature is
/// production code for that build, not a test.
#[test]
fn a_test_gate_is_recognized_in_every_spelling_it_is_written_in() {
    for gate in [
        "#[cfg(test)]",
        "#[cfg(any(test, feature = \"testing\"))]",
        "    #[cfg(all(test, unix))]",
    ] {
        assert!(admits_a_test_build(gate), "not seen as a test gate: {gate}");
    }
    for not_a_gate in [
        "#[cfg(feature = \"testing\")]",
        "#[cfg(windows)]",
        "let testing = true;",
    ] {
        assert!(
            !admits_a_test_build(not_a_gate),
            "wrongly seen as a test gate: {not_a_gate}"
        );
    }
}
