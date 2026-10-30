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
//!
//! **A remote server is pointed at, never started** (`--remote <URL>`): the
//! snippet names the address a tunnel serves `snob mcp --listen` at, and how
//! the client sends the key, with no command, and so none of the server's
//! flags. **The key is never in it**, and never read to print it: the client
//! takes it from [`TOKEN`] in its own environment, and a client with no
//! variables is told to paste it where it keeps its connectors. `TOKEN` is
//! not the server's variable (`key::ENV`), which wins over the keyring in
//! every snob it reaches; a person who exported that one for a client would
//! hand the key to any server started in the same shell.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use url::{Host, Url};

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
    let within = super::longest_call(max_wait);
    if let Some(url) = &args.remote {
        return Ok(remote(args.client, url, &globals, within));
    }
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

/// The variable a client reads the key from, for a remote server. Not the
/// server's own (`key::ENV`): see the header. `jobs::NEVER_INHERITED` keeps
/// it, with the server's, from every child.
pub const TOKEN: &str = "SNOB_MCP_TOKEN";

/// The global flags that describe the server a client starts, and so mean
/// nothing to one that reaches a server already running.
const SERVERS_GLOBALS: &[&str] = &["--account", "--no-keyring", "--strict-roots"];

/// `--remote`'s value: where the client reaches the server. **`https`**, the
/// tunnel's address, or `http` on this machine's loopback only (the near end
/// of `ssh -L`), since anywhere else the key would cross a network in plain
/// text. Nothing secret in it: no user or password, no query or fragment,
/// and no key, which goes in a header and nowhere else. An address with no
/// path is given the one the server serves ([`super::HTTP_PATH`]).
pub fn remote_url(text: &str) -> Result<Url, String> {
    // Before anything that would repeat the address: a key in it is said
    // to be one, and not printed back.
    if text.contains(super::KEY_PREFIX) {
        return Err(format!(
            "the address holds a key; the key goes in {TOKEN} in the client's environment, \
             never in an address, which clients keep and show in plain text"
        ));
    }
    // Nor is an address that does not parse, or that carries a password
    // or a token, said back: it is checked for those first, and the
    // sentences after them may repeat it.
    let mut url = Url::parse(text).map_err(|e| format!("the address does not read as one: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "the address carries a user or password; the server takes nothing but the key, \
             sent from {TOKEN} as a header"
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(
            "the address has a query or a fragment; the server is reached at a plain address, \
             with the key sent as a header"
                .to_string(),
        );
    }
    if url.host().is_none() {
        return Err(format!("\"{text}\" names no host"));
    }
    match url.scheme() {
        "https" => {}
        "http" if on_loopback(&url) => {}
        "http" => {
            return Err(format!(
                "\"{text}\" is http on another machine: the key travels in every request, so \
                 the address is the tunnel's https one, or http on this machine's loopback for \
                 a tunnel such as ssh -L"
            ));
        }
        _ => return Err(format!("\"{text}\" is neither https nor http")),
    }
    if url.path() == "/" {
        url.set_path(super::HTTP_PATH);
    }
    Ok(url)
}

/// Whether `url` is this machine's own: `localhost`, or a loopback address.
fn on_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The warning for an address `client` cannot reach, when it is one: a
/// custom connector of Claude Desktop's is reached from Anthropic's side,
/// never from the person's machine, so an address on this machine's
/// loopback (where `ssh -L` puts a tunnel's near end) leads it nowhere.
fn unreachable_by(client: McpClient, url: &Url) -> Option<&'static str> {
    (client == McpClient::ClaudeDesktop && on_loopback(url)).then_some(
        "a custom connector is reached from Anthropic's side, never from this machine, so \
         this address leads it nowhere: give it the https address a tunnel serves the \
         server at (\"snob mcp --listen --public-url\")",
    )
}

/// [`remote_url`] for clap. Not a plain function as a value parser, whose
/// refusal clap prints after the value it was given: an address that holds
/// a key or a password is refused here without being repeated.
#[derive(Clone, Copy, Debug)]
pub struct RemoteUrl;

impl clap::builder::TypedValueParser for RemoteUrl {
    type Value = Url;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<Url, clap::Error> {
        let refused = |kind, why: &str| clap::Error::raw(kind, format!("--remote: {why}\n"));
        let text = value.to_str().ok_or_else(|| {
            refused(
                clap::error::ErrorKind::InvalidUtf8,
                "the address is not UTF-8",
            )
            .with_cmd(cmd)
        })?;
        remote_url(text)
            .map_err(|why| refused(clap::error::ErrorKind::ValueValidation, &why).with_cmd(cmd))
    }
}

/// `snob mcp config <client> --remote <URL>`: the snippet for a client that
/// reaches a server already serving, and what to do with it. Exit 0 always:
/// clap refused the server's flags (`cli::McpConfigArgs`), and the address
/// was checked as it was parsed ([`remote_url`]).
fn remote(
    client: McpClient,
    url: &Url,
    globals: &[std::ffi::OsString],
    within: Duration,
) -> ExitCode {
    let ignored: Vec<&str> = SERVERS_GLOBALS
        .iter()
        .copied()
        .filter(|flag| globals.iter().any(|given| given == *flag))
        .collect();
    if !ignored.is_empty() {
        ui::warn(&format!(
            "{} {} the server, not this configuration: give {} to the \"snob mcp --listen\" \
             the address leads to",
            ignored.join(", "),
            if ignored.len() == 1 {
                "describes"
            } else {
                "describe"
            },
            if ignored.len() == 1 { "it" } else { "them" },
        ));
    }
    if let Some(warning) = unreachable_by(client, url) {
        ui::warn(warning);
    }
    if !url.path().ends_with(super::HTTP_PATH) {
        ui::warn(&format!(
            "the server answers at {} only; this address is right only if the tunnel maps \
             it there",
            super::HTTP_PATH
        ));
    }
    let (snippet, whereto) = render_remote(client, url.as_str(), within);
    ui::say!("{snippet}");
    ui::info(whereto);
    ui::info(if client == McpClient::ClaudeDesktop {
        "In the form, under Request headers, the header is Authorization and its value is \
         \"Bearer \" followed by the key, which \"snob mcp key show --reveal\" prints on the \
         machine the server runs on."
    } else {
        // `TOKEN` and `key::ENV`, spelled out so that the sentence is one
        // constant.
        "Set SNOB_MCP_TOKEN to the key (\"snob mcp key show --reveal\" prints it on the \
         machine the server runs on) in the environment the client starts in, and only there: \
         it is the client's name for the key, and SNOB_MCP_KEY, the server's, would replace \
         the key of any snob started where it is set."
    });
    // The writes are the server's to offer (`--allow-writes` on its own
    // command line), so the advice is given whether or not it does.
    ui::info(
        "If the server was started with --allow-writes, keep snob_follow and snob_unfollow on \
         \"ask\" in the client's tool permissions: snob cannot tell whether the yes it is \
         handed came from you or from the agent.",
    );
    ExitCode::Ok
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
        // longest call is one that waits for the budget, a profile's with a
        // rest between sittings after it (`mcp::longest_call`).
        McpClient::Codex => (
            format!(
                "[mcp_servers.snob]\ncommand = {}\nargs = [{}]\n\
                 # A call may wait for your answer, or for the request budget and\n\
                 # a rest; a list that takes longer goes on as a job.\n\
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

/// The snippet for `client` to reach the server at `url`, and a sentence
/// saying where it goes. No command, and **no key**: each client is told to
/// read it from [`TOKEN`] in the way it reads a variable, and Claude Desktop,
/// which reads no file for a remote server, is given the address alone.
///
/// The spellings of a variable are the clients' own, as their documentation
/// was quoted to us and not checked against each client: Claude Code expands
/// `${VAR}`, Cursor and VS Code `${env:VAR}`, and Codex takes the variable's
/// name (`bearer_token_env_var`). The sentences say to check that where a
/// client answers 401.
fn render_remote(client: McpClient, url: &str, within: Duration) -> (String, &'static str) {
    let header = |variable: String| json!({ "Authorization": format!("Bearer {variable}") });
    let servers = |key: &str, server: Value| {
        serde_json::to_string_pretty(&json!({ key: { "snob": server } })).expect("plain JSON")
    };
    match client {
        McpClient::ClaudeCode => (
            servers(
                "mcpServers",
                json!({ "type": "http", "url": url, "headers": header(format!("${{{TOKEN}}}")) }),
            ),
            "Add it to .mcp.json in a project, or under \"mcpServers\" in ~/.claude.json for \
             every project: \"claude mcp add-json --scope user snob '<the inner object>'\" \
             does the second, and \"--scope project\" the first. Claude Code fills in \
             ${SNOB_MCP_TOKEN} from its environment when it connects.",
        ),
        // Claude Desktop and claude.ai reach a remote server as a custom
        // connector, added in their settings, not in a file, and reached
        // from Anthropic's side (claude.com/docs/connectors/custom/
        // add-unlisted, read 2026-10-30: the menu, the https address, and
        // request headers in beta for some organizations, with
        // Authentication set to "No sign-in").
        McpClient::ClaudeDesktop => (
            url.to_string(),
            "Claude Desktop reads no file for a remote server: add this address as a custom \
             connector under Customize > Connectors, as on claude.ai (on a Team or Enterprise \
             plan, an Owner adds it under Organization settings > Connectors). A connector is \
             reached from Anthropic's side, so the address is the https one a tunnel serves. \
             The server takes nothing but the key, as an Authorization header: choose \
             Authentication \"No sign-in\" and give the key under Request headers, which are \
             in beta and offered to some organizations only; snob offers no sign-in of its \
             own (OAuth).",
        ),
        McpClient::Cursor => (
            servers(
                "mcpServers",
                json!({ "url": url, "headers": header(format!("${{env:{TOKEN}}}")) }),
            ),
            "Merge it into ~/.cursor/mcp.json, or .cursor/mcp.json in a project. Cursor fills \
             in ${env:SNOB_MCP_TOKEN} from its environment; if the server answers 401, check \
             how its documentation spells a variable in a header.",
        ),
        McpClient::Vscode => (
            servers(
                "servers",
                json!({
                    "type": "http",
                    "url": url,
                    "headers": header(format!("${{env:{TOKEN}}}")),
                }),
            ),
            "Merge it into .vscode/mcp.json in a workspace, or the user mcp.json (\"MCP: Open \
             User Configuration\"). VS Code's key is \"servers\", and it fills in \
             ${env:SNOB_MCP_TOKEN} from its environment; if the server answers 401, check how \
             its documentation spells a variable in a header.",
        ),
        // As for a server Codex starts (`render`): the longest call is the
        // one this command line's `--max-wait` describes, which is the
        // server's own only when the same one was given to both.
        McpClient::Codex => (
            format!(
                "[mcp_servers.snob]\nurl = {}\nbearer_token_env_var = {}\n\
                 # A call may wait for your answer, or for the request budget and\n\
                 # a rest; a list that takes longer goes on as a job.\n\
                 tool_timeout_sec = {}",
                toml_string(url),
                toml_string(TOKEN),
                within.as_secs()
            ),
            "Add it to ~/.codex/config.toml. Codex sends the key from SNOB_MCP_TOKEN as \
             Authorization: Bearer. tool_timeout_sec is counted from the --max-wait given \
             here, so give the one the server was started with.",
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

    /// Five minutes for the budget by default and a profile's rest of up to
    /// fifteen (longer than the ten for the person's answer), the child's
    /// grace and a margin: twenty-one minutes.
    #[test]
    fn codex_is_given_a_tool_timeout_past_the_longest_call() {
        let (snippet, _) = render(McpClient::Codex, EXE, &args(), within());
        assert!(snippet.ends_with("\ntool_timeout_sec = 1260"), "{snippet}");
        let longer = super::super::longest_call(Duration::from_secs(3600));
        let (snippet, _) = render(McpClient::Codex, EXE, &args(), longer);
        assert!(snippet.ends_with("\ntool_timeout_sec = 4560"), "{snippet}");
        // Every other line is a table, a key or a comment.
        assert!(
            snippet
                .lines()
                .all(|line| line.starts_with('[') || line.starts_with('#') || line.contains(" = "))
        );
    }

    const REMOTE: &str = "https://snob.example.com/mcp";

    fn every_client() -> impl Iterator<Item = McpClient> {
        use clap::ValueEnum as _;
        McpClient::value_variants().iter().copied()
    }

    #[test]
    fn a_remote_codex_config_reads_the_token_from_the_environment() {
        let (snippet, whereto) = render_remote(McpClient::Codex, REMOTE, within());
        assert!(snippet.starts_with("[mcp_servers.snob]\n"), "{snippet}");
        assert!(
            snippet.contains("\nurl = \"https://snob.example.com/mcp\"\n"),
            "{snippet}"
        );
        assert!(
            snippet.contains("\nbearer_token_env_var = \"SNOB_MCP_TOKEN\"\n"),
            "{snippet}"
        );
        assert!(snippet.ends_with("\ntool_timeout_sec = 1260"), "{snippet}");
        assert!(!snippet.contains("command"), "{snippet}");
        assert!(!snippet.contains("args"), "{snippet}");
        assert!(
            snippet
                .lines()
                .all(|line| line.starts_with('[') || line.starts_with('#') || line.contains(" = "))
        );
        assert!(whereto.contains("SNOB_MCP_TOKEN"), "{whereto}");
    }

    #[test]
    fn a_remote_claude_code_config_sends_the_token_as_a_header() {
        let (snippet, whereto) = render_remote(McpClient::ClaudeCode, REMOTE, within());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        let server = &parsed["mcpServers"]["snob"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], REMOTE);
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${SNOB_MCP_TOKEN}"
        );
        assert_eq!(
            server.as_object().map(|server| server.len()),
            Some(3),
            "nothing to start: {snippet}"
        );
        assert!(whereto.contains("--scope user"), "{whereto}");
    }

    #[test]
    fn cursor_and_vs_code_read_the_token_as_they_spell_a_variable() {
        let (snippet, _) = render_remote(McpClient::Cursor, REMOTE, within());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        let server = &parsed["mcpServers"]["snob"];
        assert_eq!(server["url"], REMOTE);
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${env:SNOB_MCP_TOKEN}"
        );
        assert!(server.get("command").is_none(), "{snippet}");

        let (snippet, _) = render_remote(McpClient::Vscode, REMOTE, within());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        let server = &parsed["servers"]["snob"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], REMOTE);
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${env:SNOB_MCP_TOKEN}"
        );
        assert!(server.get("command").is_none(), "{snippet}");
    }

    #[test]
    fn claude_desktop_is_given_the_address_for_a_connector() {
        let (snippet, whereto) = render_remote(McpClient::ClaudeDesktop, REMOTE, within());
        assert_eq!(snippet, REMOTE);
        assert!(whereto.contains("custom connector"), "{whereto}");
        assert!(whereto.contains("Customize > Connectors"), "{whereto}");
        assert!(whereto.contains("\"No sign-in\""), "{whereto}");
        assert!(whereto.contains("in beta"), "{whereto}");
    }

    /// A connector is reached from Anthropic's side: an address on this
    /// machine is one it cannot reach, and is warned about; the other
    /// clients run where the tunnel's near end is.
    #[test]
    fn a_loopback_address_for_claude_desktop_is_warned_about() {
        for address in [
            "http://127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp",
            "http://[::1]:8750/mcp",
            "https://localhost/mcp",
        ] {
            let url = remote_url(address).expect(address);
            assert!(
                unreachable_by(McpClient::ClaudeDesktop, &url).is_some(),
                "{address}"
            );
            assert!(unreachable_by(McpClient::ClaudeCode, &url).is_none());
        }
        let public = remote_url(REMOTE).unwrap();
        assert!(unreachable_by(McpClient::ClaudeDesktop, &public).is_none());
    }

    /// Nothing printed for a remote server holds a key: it is never read,
    /// and an address that holds one is refused without being repeated.
    #[test]
    fn a_remote_config_never_contains_a_key() {
        for client in every_client() {
            let (snippet, whereto) = render_remote(client, REMOTE, within());
            for said in [&snippet[..], whereto] {
                assert!(
                    !said.contains(super::super::KEY_PREFIX),
                    "{client:?}: {said}"
                );
            }
        }
        let key = "snob_mcp_N2u3l9q0aYyK0w7mQxk1vR4cT8bZp5sJdE6fGhIoUjA";
        for address in [
            format!("https://snob.example.com/{key}/mcp"),
            format!("https://snob.example.com/mcp?key={key}"),
            format!("https://{key}:x@snob.example.com/mcp"),
        ] {
            let refused = remote_url(&address).expect_err(&address);
            assert!(!refused.contains(key), "the key was repeated: {refused}");
        }
    }

    /// The server's variable wins over the keyring wherever it is set, so
    /// no snippet has it set where a client runs.
    #[test]
    fn a_remote_config_never_names_the_servers_variable() {
        for client in every_client() {
            let (snippet, _) = render_remote(client, REMOTE, within());
            assert!(!snippet.contains("SNOB_MCP_KEY"), "{client:?}: {snippet}");
        }
    }

    #[test]
    fn a_remote_server_is_reached_over_https_or_on_loopback() {
        for taken in [
            "https://snob.example.com/mcp",
            "https://snob.example.com:8443/tunnel/mcp",
            "http://127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp",
            "http://[::1]:8750/mcp",
        ] {
            assert_eq!(remote_url(taken).map(String::from), Ok(taken.to_string()));
        }
        // An address with no path is given the server's.
        assert_eq!(
            remote_url("https://snob.example.com").map(String::from),
            Ok("https://snob.example.com/mcp".to_string())
        );
        assert_eq!(
            remote_url("http://127.0.0.1:8750/").map(String::from),
            Ok("http://127.0.0.1:8750/mcp".to_string())
        );
        for refused in [
            "http://snob.example.com/mcp",
            "http://192.168.1.20:8750/mcp",
            "ftp://snob.example.com/mcp",
            "https://user:secret@snob.example.com/mcp",
            "https://snob.example.com/mcp?token=1",
            "https://snob.example.com/mcp#here",
            "snob.example.com/mcp",
        ] {
            assert!(remote_url(refused).is_err(), "{refused}");
        }
        // What may be a secret is not said back, whatever else is wrong.
        for (address, secret) in [
            ("https://user:secret@snob.example.com/mcp", "secret"),
            ("ftp://user:secret@snob.example.com/mcp", "secret"),
            ("http://user:secret@192.168.1.20/mcp", "secret"),
            ("https://snob.example.com/mcp?token=hush", "hush"),
            ("http://192.168.1.20/mcp?token=hush", "hush"),
            ("secret", "secret"),
        ] {
            let refused = remote_url(address).expect_err(address);
            assert!(!refused.contains(secret), "{refused}");
        }
    }

    /// Two homes would drift: the key's and the server's modules are behind
    /// `mcp-http`, and what this refuses and points at is not.
    #[cfg(feature = "mcp-http")]
    #[test]
    fn the_key_prefix_and_the_path_are_the_servers() {
        assert_eq!(super::super::KEY_PREFIX, super::super::key::PREFIX);
        assert_eq!(super::super::HTTP_PATH, super::super::http::PATH);
    }
}
