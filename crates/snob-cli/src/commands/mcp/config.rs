//! `snob mcp config <client>`: what to add to a client's configuration so it
//! starts this server.
//!
//! **Printed, never written.** Each client keeps its servers in a file of its
//! own, in a format of its own, and some of those files hold the person's
//! other servers and their secrets. Merging into them is a write into another
//! program's settings that a person cannot review before it happens; printing
//! is one they can read, and paste, or not. The snippet goes to standard
//! output, so it can be redirected; where it goes is said on standard error.
//!
//! **The path printed outlives an upgrade** wherever the installer keeps one
//! that does ([`stable_exe`]): a package manager runs the binary from a
//! directory named after its version, and a configuration pointing there
//! stops working the day that directory is cleaned up. A server already
//! running when that happens refuses its calls instead (`child::Launch`).

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::cli::{ConfirmInChat, McpClient, McpConfigArgs};
use crate::exit::ExitCode;
use crate::ui;

/// `globals` are this command line's global flags (`main::mcp_globals`),
/// which go in front of `mcp`; `max_wait` is the limit the server will hand
/// its children: the one given, or [`super::MAX_WAIT`].
pub fn run(
    args: &McpConfigArgs,
    globals: Vec<std::ffi::OsString>,
    max_wait: Duration,
) -> anyhow::Result<ExitCode> {
    let current = std::env::current_exe()?;
    let path_var = std::env::var_os("PATH");
    let exe = stable_exe(
        &current,
        |path| std::fs::canonicalize(path).ok(),
        path_var.as_deref(),
    );
    if names_a_version(&exe) {
        ui::warn(&format!(
            "{} is in a directory named after this version of snob, so a client started \
             from it stops working once an upgrade removes that directory; print this again \
             after an upgrade, or point the client at a path that survives one",
            exe.display()
        ));
    }
    let exe = exe.to_string_lossy().into_owned();
    let mut server_args: Vec<String> = globals
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    server_args.push("mcp".into());
    if args.allow_writes {
        server_args.push("--allow-writes".into());
    }
    if let Some(what) = args.confirm_in_chat {
        server_args.push("--confirm-in-chat".into());
        server_args.push(chat_flag(what));
    }
    if let Some(writes) = args.max_writes_per_day {
        server_args.push("--max-writes-per-day".into());
        server_args.push(writes.to_string());
    }

    let within = super::longest_call(max_wait);
    let (snippet, whereto) = render(args.client, &exe, &server_args, within);
    ui::say!("{snippet}");
    ui::info(whereto);
    if args.allow_writes {
        // The real gate on a write: the yes snob is handed may have been
        // typed by the agent itself (`mod.rs`, `consent`), and the client's
        // own prompt before a tool runs is the one an agent cannot answer.
        ui::info(
            "Keep snob_follow and snob_unfollow on \"ask\" in the client's tool permissions: \
             snob cannot tell whether the yes it is handed came from you or from the agent.",
        );
    }
    Ok(ExitCode::Ok)
}

/// The path to print for the running binary `current`: one that leads to the
/// same file today and to the upgraded one after an upgrade, when the
/// installer keeps such a path.
///
/// `lookup` is `std::fs::canonicalize` outside the tests: where a path really
/// leads, or `None` when it leads nowhere. **A candidate is taken only when it
/// leads to the running binary itself**, so a guess about a layout that turns
/// out wrong (another prefix, a link to another version, a shim that is a
/// program and not a link) is never printed: the next rule is tried, and in
/// the end `current` is printed as it is. In order:
///
/// 1. Homebrew and Linuxbrew run `<prefix>/Cellar/snob/<version>/bin/snob`
///    and link `<prefix>/bin/snob` and `<prefix>/opt/snob/bin/snob` to it;
///    both links move on an upgrade.
/// 2. Scoop runs `<root>/apps/snob/<version>/snob.exe` and keeps the junction
///    `<root>/apps/snob/current` on the newest version. Its shim on `PATH` is
///    a program of its own, so rule 3 would never find it.
/// 3. Anything else: the first `snob` on `PATH` that leads to this binary
///    (WinGet's link, `~/.local/bin`, `/usr/bin`). A relative entry on `PATH`
///    is skipped, since a client would read it from wherever it starts.
///
/// The rules match the path `current` leads to, not `current` itself: macOS
/// reports the path a program was started by, which may be the link.
pub fn stable_exe(
    current: &Path,
    lookup: impl Fn(&Path) -> Option<PathBuf>,
    path_var: Option<&OsStr>,
) -> PathBuf {
    let Some(real) = lookup(current) else {
        return current.to_path_buf();
    };
    let Some(file) = real.file_name() else {
        return current.to_path_buf();
    };
    let leads_here = |candidate: &Path| lookup(candidate).is_some_and(|there| there == real);

    let names: Vec<&OsStr> = real
        .components()
        .map(|component| component.as_os_str())
        .collect();
    let mut candidates: Vec<PathBuf> = Vec::new();
    // `<prefix>/Cellar/snob/<version>/bin/snob`, its last five names.
    if let [.., cellar, snob, _version, bin, _file] = names.as_slice()
        && *cellar == "Cellar"
        && *snob == "snob"
        && *bin == "bin"
        && let Some(prefix) = ancestor(&real, 5)
    {
        candidates.push(prefix.join("bin").join(file));
        candidates.push(prefix.join("opt").join("snob").join("bin").join(file));
    }
    // `<root>/apps/snob/<version>/snob.exe`, its last four names.
    if let [.., apps, snob, _version, _file] = names.as_slice()
        && *apps == "apps"
        && *snob == "snob"
        && let Some(app) = ancestor(&real, 2)
    {
        candidates.push(app.join("current").join(file));
    }
    if let Some(found) = candidates
        .into_iter()
        .map(without_verbatim)
        .find(|candidate| leads_here(candidate))
    {
        return found;
    }

    let typed = current.file_name().unwrap_or(file);
    path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(typed))
        .find(|candidate| leads_here(candidate))
        .unwrap_or_else(|| current.to_path_buf())
}

/// `--confirm-in-chat`'s value for `what`, as the command line spells it.
fn chat_flag(what: ConfirmInChat) -> String {
    use clap::ValueEnum as _;
    what.to_possible_value()
        .map(|value| value.get_name().to_string())
        .unwrap_or_default()
}

/// `path` with its last `levels` names taken off.
fn ancestor(path: &Path, levels: usize) -> Option<&Path> {
    (0..levels).try_fold(path, |path, _| path.parent())
}

/// A path without the `\\?\` that `canonicalize` puts in front of one on
/// Windows, when what follows is a drive. `C:\...` is what a person would
/// write in a configuration, and what every client can start. A path on a
/// share (`\\?\UNC\...`) keeps it: dropping it there takes more than a cut.
fn without_verbatim(path: PathBuf) -> PathBuf {
    let Some(rest) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) else {
        return path;
    };
    let bytes = rest.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':') {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// Whether a directory on the way to `path` is named after a version
/// (`0.7.0`, `0.7.0_1`, `snob-v0.7.0-x86_64-pc-windows-msvc`): three numbers
/// with dots between them, which is how snob's versions are written.
fn names_a_version(path: &Path) -> bool {
    let versioned = |name: &OsStr| {
        name.to_string_lossy()
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .any(|run| {
                let parts: Vec<&str> = run.trim_matches('.').split('.').collect();
                parts.len() >= 3 && parts.iter().all(|part| !part.is_empty())
            })
    };
    path.parent().is_some_and(|dirs| {
        dirs.components().any(|component| match component {
            Component::Normal(name) => versioned(name),
            _ => false,
        })
    })
}

/// The snippet for `client`, and a sentence saying where it goes. `within` is
/// how long one call may take ([`super::longest_call`]), for the client whose
/// own limit is shorter than that.
fn render(
    client: McpClient,
    exe: &str,
    args: &[String],
    within: Duration,
) -> (String, &'static str) {
    let stdio = || json!({ "command": exe, "args": args });
    let servers = |key: &str, server: Value| {
        serde_json::to_string_pretty(&json!({ key: { "snob": server } })).expect("plain JSON")
    };
    match client {
        McpClient::ClaudeCode => (
            servers("mcpServers", stdio()),
            "Add it to .mcp.json in a project, or under \"mcpServers\" in ~/.claude.json for \
             every project: \"claude mcp add-json --scope user snob '<the inner object>'\" \
             does the second, and \"--scope project\" the first.",
        ),
        McpClient::ClaudeDesktop => (
            servers("mcpServers", stdio()),
            "Merge it into claude_desktop_config.json (Settings, Developer, Edit Config) and \
             restart Claude Desktop.",
        ),
        McpClient::Cursor => (
            servers("mcpServers", stdio()),
            "Merge it into ~/.cursor/mcp.json, or .cursor/mcp.json in a project.",
        ),
        McpClient::Vscode => {
            let mut server = stdio();
            server["type"] = json!("stdio");
            (
                servers("servers", server),
                "Merge it into .vscode/mcp.json in a workspace, or the user mcp.json \
                 (\"MCP: Open User Configuration\"). VS Code's key is \"servers\".",
            )
        }
        // Codex gives up on a tool call after 60 seconds unless told
        // otherwise (`tool_timeout_sec`), which is less than the person may
        // take to answer a question alone. A list's call is bounded by its
        // wait for the job it starts, and the walk goes on past it, so the
        // longest call is one that waits for the budget (`mcp::longest_call`).
        McpClient::Codex => (
            format!(
                "[mcp_servers.snob]\ncommand = {}\nargs = [{}]\n\
                 # A call may wait for your answer, then for the request budget;\n\
                 # a list that takes longer goes on as a job.\n\
                 tool_timeout_sec = {}",
                toml_string(exe),
                args.iter()
                    .map(|arg| toml_string(arg))
                    .collect::<Vec<_>>()
                    .join(", "),
                within.as_secs()
            ),
            "Add it to ~/.codex/config.toml.",
        ),
    }
}

/// A TOML basic string: quoted, with the backslashes of a Windows path and any
/// quote escaped.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\snob\snob.exe";

    fn args() -> Vec<String> {
        vec!["mcp".into()]
    }

    fn within() -> Duration {
        super::super::longest_call(super::super::MAX_WAIT)
    }

    /// An absolute path on the platform the test runs on, written with
    /// forward slashes, which both read as separators.
    fn at(rest: &str) -> PathBuf {
        PathBuf::from(if cfg!(windows) { "C:/" } else { "/" }).join(rest)
    }

    /// A file system of links: each path and where it really leads. A path
    /// not listed leads nowhere.
    fn links(table: &[(PathBuf, PathBuf)]) -> impl Fn(&Path) -> Option<PathBuf> + '_ {
        move |path| {
            table
                .iter()
                .find(|(from, _)| from == path)
                .map(|(_, to)| to.clone())
        }
    }

    fn path_of(dirs: &[PathBuf]) -> std::ffi::OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn a_homebrew_cellar_path_is_printed_as_the_linked_one() {
        let real = at("opt/homebrew/Cellar/snob/0.7.0/bin/snob");
        let table = [
            (real.clone(), real.clone()),
            (at("opt/homebrew/bin/snob"), real.clone()),
            (at("opt/homebrew/opt/snob/bin/snob"), real.clone()),
        ];
        let printed = stable_exe(&real, links(&table), None);
        assert_eq!(printed, at("opt/homebrew/bin/snob"));
        assert!(names_a_version(&real));
        assert!(!names_a_version(&printed));

        // macOS may report the link it was started by: the same answer.
        let printed = stable_exe(&at("opt/homebrew/bin/snob"), links(&table), None);
        assert_eq!(printed, at("opt/homebrew/bin/snob"));

        // Without the link in bin, the one in opt.
        let printed = stable_exe(&real, links(&table[..1]), None);
        assert_eq!(
            printed, real,
            "nothing links to it, so it is printed as it is"
        );
        let table = [table[0].clone(), table[2].clone()];
        let printed = stable_exe(&real, links(&table), None);
        assert_eq!(printed, at("opt/homebrew/opt/snob/bin/snob"));
    }

    /// While an older version is still running, the links lead to the newer
    /// one, and printing them would describe a binary other than this one.
    #[test]
    fn a_link_to_another_version_is_not_printed() {
        let real = at("home/linuxbrew/.linuxbrew/Cellar/snob/0.7.0/bin/snob");
        let newer = at("home/linuxbrew/.linuxbrew/Cellar/snob/0.8.0/bin/snob");
        let table = [
            (real.clone(), real.clone()),
            (at("home/linuxbrew/.linuxbrew/bin/snob"), newer.clone()),
            (at("home/linuxbrew/.linuxbrew/opt/snob/bin/snob"), newer),
        ];
        assert_eq!(stable_exe(&real, links(&table), None), real);
    }

    #[test]
    fn a_scoop_version_directory_is_printed_as_current() {
        let real = at("Users/someone/scoop/apps/snob/0.7.0/snob.exe");
        let current = at("Users/someone/scoop/apps/snob/current/snob.exe");
        let shims = at("Users/someone/scoop/shims");
        let table = [
            (real.clone(), real.clone()),
            (current.clone(), real.clone()),
            // The shim is a program that starts the binary, not a link to it.
            (shims.join("snob.exe"), shims.join("snob.exe")),
        ];
        let path = path_of(std::slice::from_ref(&shims));
        assert_eq!(stable_exe(&real, links(&table), Some(&path)), current);
        // Started through the shim, the binary reports the junction's path.
        assert_eq!(stable_exe(&current, links(&table), Some(&path)), current);
    }

    #[test]
    fn the_first_snob_on_the_path_that_leads_here_is_printed() {
        let real = at(
            "Users/someone/AppData/Local/Microsoft/WinGet/Packages/dennisgr7.snob_x/\
                       snob-v0.7.0-x86_64-pc-windows-msvc/snob.exe",
        );
        let elsewhere = at("Tools");
        let winget = at("Users/someone/AppData/Local/Microsoft/WinGet/Links");
        let table = [
            (real.clone(), real.clone()),
            // A relative entry, which a client would read from its own
            // directory: skipped even though it leads here today.
            (PathBuf::from("bin").join("snob.exe"), real.clone()),
            (elsewhere.join("snob.exe"), elsewhere.join("snob.exe")),
            (winget.join("snob.exe"), real.clone()),
        ];
        let path = path_of(&[PathBuf::from("bin"), elsewhere, winget.clone()]);
        let printed = stable_exe(&real, links(&table), Some(&path));
        assert_eq!(printed, winget.join("snob.exe"));
        assert!(names_a_version(&real));
        assert!(!names_a_version(&printed));
    }

    #[test]
    fn a_path_nothing_links_to_is_printed_as_it_is() {
        let real = at("dev/snob-cli/target/release/snob");
        let other = at("usr/bin");
        let table = [
            (real.clone(), real.clone()),
            (other.join("snob"), other.join("snob")),
        ];
        let path = path_of(&[other]);
        assert_eq!(stable_exe(&real, links(&table), Some(&path)), real);
        assert!(!names_a_version(&real));
        // And one that leads nowhere at all, which no rule can judge.
        let gone = at("tmp/snob");
        assert_eq!(stable_exe(&gone, links(&table), Some(&path)), gone);
    }

    #[test]
    fn a_drive_path_loses_the_prefix_canonicalize_gives_it() {
        assert_eq!(
            without_verbatim(PathBuf::from(r"\\?\C:\scoop\apps\snob\current\snob.exe")),
            PathBuf::from(r"C:\scoop\apps\snob\current\snob.exe")
        );
        let share = PathBuf::from(r"\\?\UNC\server\share\snob.exe");
        assert_eq!(without_verbatim(share.clone()), share);
    }

    #[test]
    fn the_json_clients_get_the_path_escaped_as_json() {
        let (snippet, _) = render(McpClient::ClaudeCode, EXE, &args(), within());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(parsed["mcpServers"]["snob"]["command"], EXE);
        assert_eq!(parsed["mcpServers"]["snob"]["args"][0], "mcp");
    }

    #[test]
    fn claude_code_is_told_the_scope_to_add_it_with() {
        let (_, whereto) = render(McpClient::ClaudeCode, EXE, &args(), within());
        assert!(
            whereto.contains("claude mcp add-json --scope user snob"),
            "{whereto}"
        );
    }

    #[test]
    fn vs_code_is_told_under_its_own_key() {
        let (snippet, _) = render(McpClient::Vscode, EXE, &args(), within());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(parsed["servers"]["snob"]["type"], "stdio");
    }

    #[test]
    fn codex_gets_toml_with_the_backslashes_escaped() {
        let (snippet, _) = render(McpClient::Codex, EXE, &args(), within());
        assert!(snippet.contains(r#"command = "C:\\Program Files\\snob\\snob.exe""#));
        assert!(snippet.contains("\nargs = [\"mcp\"]\n"), "{snippet}");
    }

    /// Ten minutes for the person's answer, five for the budget by default,
    /// the child's grace and a margin: sixteen minutes.
    #[test]
    fn codex_is_given_a_tool_timeout_past_the_longest_call() {
        let (snippet, _) = render(McpClient::Codex, EXE, &args(), within());
        assert!(snippet.ends_with("\ntool_timeout_sec = 960"), "{snippet}");
        let longer = super::super::longest_call(Duration::from_secs(3600));
        let (snippet, _) = render(McpClient::Codex, EXE, &args(), longer);
        assert!(snippet.ends_with("\ntool_timeout_sec = 4260"), "{snippet}");
        // Every other line is a table, a key or a comment.
        assert!(
            snippet
                .lines()
                .all(|line| line.starts_with('[') || line.starts_with('#') || line.contains(" = "))
        );
    }
}
