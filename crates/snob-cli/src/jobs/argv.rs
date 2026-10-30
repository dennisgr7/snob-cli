//! What a job may run, checked where it is started and again where it runs.
//!
//! **A job runs a list walk and nothing else**, with a handful of flags that
//! shape what the walk returns and none that change where it reads or writes.
//! The command line is stored in `job.json`, a file on disk that anything
//! running as this user can edit, so the supervisor checks it again before it
//! runs it ([`check`] in `supervise.rs` as in `start.rs`) and refuses what does
//! not pass, running nothing. It is never re-planned from the call's
//! arguments: `-y` is in the stored command line because the call that
//! started the job was given it, and re-planning would put it there for a
//! call that never was.
//!
//! The command is judged by clap, on the subcommand it parses to, and the
//! flags token by token against [`ALLOWED`]: an account called `follow` is a
//! positional argument of `followers`, not a `follow`.
//!
//! The global flags a job's processes run with are never read from disk:
//! [`globals`] builds them from the command line of the process that starts
//! the supervisor, and the supervisor's own command line is what it passes on.

use std::ffi::OsString;

use clap::Parser;
use snob_core::Pk;
use snob_core::model::ListKind;

use crate::cli::{Cli, Command, ListArgs};

/// A list command a job may run, checked.
pub(crate) struct Checked {
    /// The command line, as it will be run: what was given, less
    /// `--no-progress`, which would otherwise silence the progress the job is
    /// read by (`--progress-json` wins over it in `progress.rs` as well).
    pub(crate) args: Vec<String>,
    /// The command, by the name the help gives it.
    pub(crate) command: &'static str,
    /// The lists it walks, in order.
    pub(crate) kinds: &'static [ListKind],
    /// Whose lists, as typed less a leading `@`; `None` is the viewer's own.
    pub(crate) target: Option<String>,
    /// What clap made of it, for the estimate.
    pub(crate) list: ListArgs,
}

/// The flags a job's command may carry, beside the account it names.
/// `--format` only as `json`, and `--no-progress` is taken out rather than
/// refused (see [`Checked::args`]); the two `--confirmed-*` are how a walk run
/// on a yes is recorded (`cli::AgentArgs`).
const ALLOWED: &[&str] = &[
    "--format",
    "--no-interactive",
    "--offline",
    "--refresh",
    "--limit",
    "--max-pages",
    "-y",
    "--yes",
    "--confirmed-via",
    "--confirmed-for",
    "--no-progress",
];

/// Of [`ALLOWED`], the flags that take a value.
const TAKES_A_VALUE: &[&str] = &[
    "--format",
    "--limit",
    "--max-pages",
    "--confirmed-via",
    "--confirmed-for",
];

/// Checks `args`, a command line with no program name, as one a job may run.
///
/// The error says why in words a person can read, and names no more of the
/// command line than the flag that was refused.
pub(crate) fn check(args: &[String]) -> Result<Checked, String> {
    let Some(first) = args.first() else {
        return Err("a job needs a command to run".into());
    };
    // The command first, so no global flag can stand before it.
    if first.starts_with('-') {
        return Err("a job's command line starts with the command".into());
    }
    let parsed =
        Cli::try_parse_from(std::iter::once("snob").chain(args.iter().map(String::as_str)))
            .map_err(|e| {
                format!(
                    "a job's command line does not parse: {}",
                    e.kind().as_str().unwrap_or("it was refused")
                )
            })?;
    let (command, kinds, list) = match parsed.command {
        Command::Followers(list) => ("followers", &[ListKind::Followers][..], list),
        Command::Following(list) => ("following", &[ListKind::Following][..], list),
        Command::Unfollowers(list) => (
            "unfollowers",
            &[ListKind::Followers, ListKind::Following][..],
            list,
        ),
        Command::Fans(list) => (
            "fans",
            &[ListKind::Followers, ListKind::Following][..],
            list,
        ),
        Command::Friends(list) => (
            "friends",
            &[ListKind::Followers, ListKind::Following][..],
            list,
        ),
        _ => {
            return Err(
                "a job walks a list: followers, following, unfollowers, fans or friends".into(),
            );
        }
    };
    if list.walk.dry_run {
        return Err("a job walks a list; an estimate is not one".into());
    }

    let mut kept = vec![first.clone()];
    let mut rest = args[1..].iter();
    while let Some(token) = rest.next() {
        if !token.starts_with('-') {
            // The account; clap has already said there is at most one.
            kept.push(token.clone());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (token.as_str(), None),
        };
        if !ALLOWED.contains(&flag) {
            return Err(format!(
                "a job does not take {}",
                snob_core::model::printable(flag)
            ));
        }
        let value = if TAKES_A_VALUE.contains(&flag) {
            match inline {
                Some(value) => Some(value.to_string()),
                None => Some(
                    rest.next()
                        .ok_or_else(|| format!("{flag} needs a value"))?
                        .clone(),
                ),
            }
        } else if inline.is_some() {
            return Err(format!("{flag} takes no value"));
        } else {
            None
        };
        if flag == "--format" && value.as_deref() != Some("json") {
            return Err("a job's result is JSON, so --format can only be json".into());
        }
        if flag == "--no-progress" {
            continue;
        }
        kept.push(token.clone());
        if inline.is_none()
            && let Some(value) = value
        {
            kept.push(value);
        }
    }

    let target = list
        .target
        .as_deref()
        .map(|t| t.strip_prefix('@').unwrap_or(t).to_string());
    Ok(Checked {
        args: kept,
        command,
        kinds,
        target,
        list,
    })
}

/// Whether two jobs walk a list in common: the same account, by name, and a
/// list each of them walks.
pub(crate) fn overlap(one: &Checked, other: &Checked) -> bool {
    let same_account = match (&one.target, &other.target) {
        (None, None) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    same_account && one.kinds.iter().any(|kind| other.kinds.contains(kind))
}

/// The global flags a job's processes are started with: the account, by its
/// id so no change of the active account can move the job onto another, and
/// what decides where its files are and what TLS trusts. **Never
/// `--stop-when-stdin-closes`**, which the process that starts a job may carry
/// and which would stop a supervisor whose standard input is the null device
/// at once, nor `--max-wait` or `--spend-alone`, which the caller adds once.
pub(crate) fn globals(cli: &Cli, account: Pk) -> std::io::Result<Vec<OsString>> {
    let mut args: Vec<OsString> = vec!["--account".into(), account.to_string().into()];
    if cli.no_keyring {
        args.push("--no-keyring".into());
    }
    if cli.strict_roots {
        args.push("--strict-roots".into());
        for root in &cli.tls_extra_root {
            args.push("--tls-extra-root".into());
            args.push(std::path::absolute(root)?.into());
        }
    }
    #[cfg(feature = "testing")]
    if let Some(root) = &cli.sandbox_root {
        args.push("--sandbox-root".into());
        args.push(std::path::absolute(root)?.into());
        if let Some(base) = &cli.ig_base_url {
            args.push("--ig-base-url".into());
            args.push(base.as_str().into());
        }
        if cli.through_the_browser {
            args.push("--through-the-browser".into());
        }
    }
    Ok(args)
}

/// [`globals`] and the job's `--max-wait`, **once**: what a supervisor is
/// started with ahead of `__job run <id>`. The value is the one the process
/// starting the job was given, and is never appended to one already there,
/// which clap refuses (a global flag given twice is an error).
pub(crate) fn supervisor(cli: &Cli, account: Pk) -> std::io::Result<Vec<OsString>> {
    let mut args = globals(cli, account)?;
    push_max_wait(&mut args, cli.max_wait);
    Ok(args)
}

/// What a job's walk is started with, by its supervisor, from the
/// supervisor's own command line: [`globals`], the three flags every job's
/// walk gets, the job's `--max-wait` once, and the command as checked.
pub(crate) fn walk(cli: &Cli, account: Pk, command: &[String]) -> std::io::Result<Vec<OsString>> {
    let mut args = globals(cli, account)?;
    args.extend(
        [
            "--spend-alone",
            "--stop-when-stdin-closes",
            "--progress-json",
        ]
        .map(OsString::from),
    );
    push_max_wait(&mut args, cli.max_wait);
    args.extend(command.iter().map(OsString::from));
    Ok(args)
}

/// `--max-wait`, in whole seconds as the MCP server passes it, when there is
/// one; zero stays zero.
fn push_max_wait(args: &mut Vec<OsString>, wait: Option<std::time::Duration>) {
    if let Some(wait) = wait {
        args.push("--max-wait".into());
        args.push(format!("{}s", wait.as_secs()).into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Vec<String> {
        text.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn start_refuses_anything_but_a_list_command() {
        for refused in [
            "follow someone -y --json",
            "unfollow someone -y",
            "status --json",
            "scan someone",
            "profile someone",
            "purge --yes",
            "followers --dry-run",
            "--account x followers",
            "",
        ] {
            assert!(check(&line(refused)).is_err(), "{refused}");
        }
        for allowed in [
            "followers",
            "following someone -y --format json --no-interactive",
            "unfollowers --refresh --limit 5 --max-pages 3",
            "fans @someone --offline",
            "friends",
            "mutuals",
        ] {
            assert!(check(&line(allowed)).is_ok(), "{allowed}");
        }
    }

    #[test]
    fn an_account_called_follow_is_a_list_argument() {
        let checked = check(&line("followers follow -y")).expect("a list of an account");
        assert_eq!(checked.command, "followers");
        assert_eq!(checked.target.as_deref(), Some("follow"));
        let checked = check(&line("fans @status")).expect("a list of an account");
        assert_eq!(checked.target.as_deref(), Some("status"));
        assert_eq!(checked.kinds, [ListKind::Followers, ListKind::Following]);
    }

    #[test]
    fn run_refuses_a_flag_outside_the_allowlist() {
        for refused in [
            "followers -o out.json",
            "followers --output=out.json",
            "followers --format csv",
            "followers --format=xlsx",
            "followers --same-day",
            "followers --exclude-list names.txt",
            "followers --max-wait 5s",
            "followers --account other",
            "followers --sandbox-root x",
            "followers --spend-alone",
            "followers --stop-when-stdin-closes",
            "followers --verbose",
            "followers -i",
            "followers --no-interactive=yes",
            "followers -- --refresh",
        ] {
            assert!(check(&line(refused)).is_err(), "{refused}");
        }
        // The two forms of a value are both kept as they were given.
        let checked = check(&line(
            "followers x -y --limit=3 --max-pages 2 --confirmed-via=chat --confirmed-for=client",
        ))
        .unwrap();
        assert_eq!(
            checked.args,
            line(
                "followers x -y --limit=3 --max-pages 2 --confirmed-via=chat --confirmed-for=client"
            )
        );
    }

    #[test]
    fn no_progress_is_taken_out_of_a_jobs_command() {
        let checked = check(&line(
            "followers --format json --no-interactive --no-progress",
        ))
        .unwrap();
        assert_eq!(
            checked.args,
            line("followers --format json --no-interactive")
        );
    }

    #[test]
    fn two_jobs_overlap_when_they_walk_a_list_of_the_same_account() {
        let job = |text: &str| check(&line(text)).unwrap();
        assert!(overlap(&job("followers"), &job("unfollowers")));
        assert!(!overlap(&job("followers"), &job("following")));
        assert!(overlap(&job("fans someone"), &job("following @Someone")));
        assert!(!overlap(&job("fans someone"), &job("fans")));
    }

    #[test]
    fn a_jobs_globals_name_the_account_by_id_and_never_stop_on_stdin() {
        let cli = Cli::try_parse_from([
            "snob",
            "--account",
            "someone",
            "--max-wait",
            "5s",
            "--stop-when-stdin-closes",
            "--spend-alone",
            "--no-keyring",
            "status",
        ])
        .unwrap();
        let globals = globals(&cli, Pk::new(42)).unwrap();
        assert_eq!(
            globals[..3],
            ["--account", "42", "--no-keyring"].map(OsString::from)
        );
        for never in ["--max-wait", "--stop-when-stdin-closes", "--spend-alone"] {
            assert!(!globals.iter().any(|arg| arg == never), "{never}");
        }
    }

    #[test]
    fn max_wait_is_replaced_not_repeated() {
        let cli = Cli::try_parse_from(["snob", "--max-wait", "90s", "__job", "run", "x"]).unwrap();
        let times = |args: &[OsString]| args.iter().filter(|arg| *arg == "--max-wait").count();
        let supervisor = supervisor(&cli, Pk::new(42)).unwrap();
        assert_eq!(times(&supervisor), 1);
        let at = supervisor
            .iter()
            .position(|arg| arg == "--max-wait")
            .unwrap();
        assert_eq!(supervisor[at + 1], "90s");

        let command = line("followers --limit 3");
        let walk = walk(&cli, Pk::new(42), &command).unwrap();
        assert_eq!(times(&walk), 1);
        assert!(walk.ends_with(&command.iter().map(OsString::from).collect::<Vec<_>>()));
        for flag in [
            "--spend-alone",
            "--stop-when-stdin-closes",
            "--progress-json",
        ] {
            assert_eq!(walk.iter().filter(|arg| *arg == flag).count(), 1, "{flag}");
        }
        // And with none given, none at all: a walk a person's job runs waits
        // as long as one they type.
        let cli = Cli::try_parse_from(["snob", "__job", "run", "x"]).unwrap();
        assert_eq!(times(&walk_of(&cli)), 0);
    }

    fn walk_of(cli: &Cli) -> Vec<OsString> {
        walk(cli, Pk::new(42), &line("followers")).unwrap()
    }
}
