# Snob CLI

An Instagram client for the terminal, with extra utilities, media downloads and
output ready for AI agents.

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

Builds exist for Windows and Linux on x86_64 and ARM64, and macOS on Apple
Silicon. Everything that talks to Instagram needs a Chromium-based browser
installed; no window is ever shown.

## Getting started

```bash
snob login      # sign in through a browser snob opens, or paste a sessionid
snob whoami
```

Your password never goes through snob, and it never reads your own browser's
cookies.

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
  and `status`.
- `snob status --budget` says what can still be sent today, for no request, and
  exits 5 while the account is cooling down.
- `--no-interactive` always prints. Down a pipe this is the default, and the
  default format is JSON.
- `-y` answers the one confirmation a command may ask, in advance.
- Exit codes are stable: 0 worked, 1 failed, 2 bad command line, 3 log in
  again, 4 the account needs verifying, 5 rate limited or cooling down, 130
  stopped. When the output was going to be JSON, a failure is also one JSON
  object on the last line of standard error.
- `snob watch --webhook <url>` posts each report as JSON; `--json` writes them
  as lines instead.

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
