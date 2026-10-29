# Snob CLI

An Instagram CLI: a client for the terminal, with extra utilities, media
downloads and output ready for AI agents.

`snob` tells you who does not follow you back, browses profiles, stories,
highlights, posts and reels, downloads their photos and videos, and watches an
account over time to report what changed. It signs in as you and sends its
requests from a Chrome, Edge, Brave or Chromium already on your machine, at a
human pace. It only ever changes two things, `follow` and `unfollow`, one
account at a time and after asking; it never marks a story as seen.

## Install

**Windows** ([Scoop](https://scoop.sh)):

```bash
scoop bucket add snob https://github.com/dennisgr7/snob-cli
scoop install snob
```

**macOS and Linux** ([Homebrew](https://brew.sh)):

```bash
brew tap dennisgr7/snob https://github.com/dennisgr7/snob-cli
brew install snob
```

**Debian and Ubuntu**: the `.deb` for your architecture is on the
[releases page](https://github.com/dennisgr7/snob-cli/releases).

```bash
sudo apt install ./snob-v<version>-x86_64-unknown-linux-musl.deb
```

**Without a package manager**:

```bash
curl -fsSL https://raw.githubusercontent.com/dennisgr7/snob-cli/main/packaging/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/dennisgr7/snob-cli/main/packaging/install.ps1 | iex
```

**From source** (Rust and a C compiler):

```bash
cargo install --locked --git https://github.com/dennisgr7/snob-cli snob-cli
```

Builds exist for Windows 11 and Linux on x86_64 and ARM64, and macOS on Apple
Silicon. Everything that talks to Instagram needs a Chromium-based browser
installed; no window is ever shown.

## Getting started

```bash
snob login      # sign in through a browser snob opens, or paste a sessionid
snob whoami
```

Over SSH, where no window can reach you, `snob login --remote` prints an `ssh`
command that opens a tunnel from your computer and a link to open there: you log
in on that page, in snob's browser seen from afar, and the session stays on the
machine snob runs on.

snob never sees your password when you log in in a window or paste a session,
and it never reads your own browser's cookies. With `--remote` what you type on
that page is passed through snob to its browser, and kept nowhere.

## Commands

```bash
snob unfollowers                 # you follow them, they do not follow you back
snob fans                        # they follow you, you do not follow them
snob friends                     # you follow each other
snob followers someone           # any list, yours or another account's
snob profile someone             # the profile page: counts, bio, mutuals, highlights
snob pfp someone                 # the profile picture at full size
snob stories someone -d all      # save every story an account has up
snob highlights someone 2 -d all # save everything in its second highlight
snob posts someone               # the posts on its grid
snob post <link> -d all          # one post by its link, every photo and video in it
snob reel <link> -d all          # the same for a reel
snob fetch <cdn-address>         # one file by its address, no account needed
snob follow someone
snob unfollow someone
snob watch                       # keep watching on a schedule, report changes
snob status                      # budget left, cooldown, stored lists; sends nothing
```

On a terminal most commands open a full-screen view you move through with the
arrow keys. `snob --help` and `snob <command> --help` list everything.

## Scripts, automation and AI agents

Every command can print instead of opening a view, in a stable form a program
can read:

- `--format json|ndjson|csv|xlsx|md|table` and `-o <file>` for anything that
  prints a list or a document; `--json` for status commands such as `whoami`
  and `status`, and for `follow` and `unfollow`, whose object says what
  happened in one word (`followed`, `requested`, `already_following`, ...).
- `snob status --budget` says what can still be sent today, for no request, and
  exits 5 while the account is cooling down. `--dry-run` on a list command says
  what its walks would cost against that, and sends nothing.
- `-o -` is standard output: a list as with no `-o`, and a single downloaded
  file, as in `snob reel <link> -d 1 -o - | ffmpeg -i - ...`.
- `--no-interactive` always prints. Down a pipe this is the default, and the
  default format is JSON.
- `-y` answers the one confirmation a command may ask, in advance.
- `--max-wait <duration>` stops a command, with exit 130, rather than let it
  wait longer than that for the request budget; a list stopped this way
  continues where it left off next time.
- Exit codes are stable: 0 worked, 1 failed, 2 bad command line, 3 log in
  again, 4 the account needs verifying, 5 rate limited or cooling down, 130
  stopped. When the output was going to be JSON, a failure is also one JSON
  object on the last line of standard error.
- `snob watch --webhook <url>` posts each report as JSON; `--json` writes them
  as lines instead.
- `snob mcp` is an MCP server for AI agents, on standard input and output:
  the status, the lists and profiles, and with `--allow-writes` follow and
  unfollow. Those, and walking another account's lists, run only after a yes
  for that one call: asked in the client when it can show a form, or put to
  you by the agent in the chat (`--confirm-in-chat none|lists|all` says what
  that may cover; `all` by default). snob cannot tell whether that yes was
  yours or the agent's, so keep the writes on "ask" in the client's tool
  permissions. Each call runs one snob command, under the same budget and
  cooldown as the terminal.
  `snob mcp config claude-code` (or `claude-desktop`, `codex`, `cursor`,
  `vscode`) prints what to add to that client's configuration. After
  upgrading snob, reconnect the server in the client: a server whose binary
  changed under it refuses every call.

## Staying a light client

There is no official API for most of this, so snob uses the same web API
instagram.com uses, signed in as you. That is outside Instagram's Terms of Use,
and the usual consequence is that Instagram asks the account to verify itself.
snob keeps to modest volumes, sends only the web app's own requests, and stops
at the first sign of push-back. Use it from the connection you normally browse
from.

To remove everything snob stored on the machine, sessions included, run
`snob purge` before uninstalling.

## License

MIT.
