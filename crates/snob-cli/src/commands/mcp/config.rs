//! `snob mcp config <client>`: what to add to a client's configuration so it
//! starts this server.
//!
//! **Printed, never written.** Each client keeps its servers in a file of its
//! own, in a format of its own, and some of those files hold the person's
//! other servers and their secrets. Merging into them is a write into another
//! program's settings that a person cannot review before it happens; printing
//! is one they can read, and paste, or not. The snippet goes to standard
//! output, so it can be redirected; where it goes is said on standard error.

use serde_json::{Value, json};

use crate::cli::{McpClient, McpConfigArgs};
use crate::exit::ExitCode;
use crate::ui;

/// `globals` are this command line's global flags (`main::mcp_globals`),
/// which go in front of `mcp`.
pub fn run(args: &McpConfigArgs, globals: Vec<std::ffi::OsString>) -> anyhow::Result<ExitCode> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy().into_owned();
    let mut server_args: Vec<String> = globals
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    server_args.push("mcp".into());
    if args.allow_writes {
        server_args.push("--allow-writes".into());
    }

    let (snippet, whereto) = render(args.client, &exe, &server_args);
    ui::say!("{snippet}");
    ui::info(whereto);
    Ok(ExitCode::Ok)
}

/// The snippet for `client`, and a sentence saying where it goes.
fn render(client: McpClient, exe: &str, args: &[String]) -> (String, &'static str) {
    let stdio = || json!({ "command": exe, "args": args });
    let servers = |key: &str, server: Value| {
        serde_json::to_string_pretty(&json!({ key: { "snob": server } })).expect("plain JSON")
    };
    match client {
        McpClient::ClaudeCode => (
            servers("mcpServers", stdio()),
            "Add it to .mcp.json in a project, or under \"mcpServers\" in ~/.claude.json for \
             every project; \"claude mcp add-json snob '<the inner object>'\" does the same.",
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
        McpClient::Codex => (
            format!(
                "[mcp_servers.snob]\ncommand = {}\nargs = [{}]",
                toml_string(exe),
                args.iter()
                    .map(|arg| toml_string(arg))
                    .collect::<Vec<_>>()
                    .join(", ")
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

    #[test]
    fn the_json_clients_get_the_path_escaped_as_json() {
        let (snippet, _) = render(McpClient::ClaudeCode, EXE, &args());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(parsed["mcpServers"]["snob"]["command"], EXE);
        assert_eq!(parsed["mcpServers"]["snob"]["args"][0], "mcp");
    }

    #[test]
    fn vs_code_is_told_under_its_own_key() {
        let (snippet, _) = render(McpClient::Vscode, EXE, &args());
        let parsed: Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(parsed["servers"]["snob"]["type"], "stdio");
    }

    #[test]
    fn codex_gets_toml_with_the_backslashes_escaped() {
        let (snippet, _) = render(McpClient::Codex, EXE, &args());
        assert!(snippet.contains(r#"command = "C:\\Program Files\\snob\\snob.exe""#));
        assert!(snippet.ends_with(r#"args = ["mcp"]"#), "{snippet}");
    }
}
