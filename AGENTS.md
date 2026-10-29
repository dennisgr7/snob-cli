# AGENTS.md

Instructions for anyone, human or agent, changing this repository. `CLAUDE.md`
points here. Code comments cite this file by section name ("Known walls",
"Costs worth knowing", "Stories are handed to the system viewer", the write
regime, the rules below), so rename a section only together with the comments
that cite it (`grep -rn AGENTS.md crates`).

The code documents itself heavily: every module opens with a `//!` header that
says what it is for and which rule it keeps, and every dependency, feature,
workflow step and magic number has its reason written beside it. **Read the
header of a file before changing it.** This document is the map and the rules
that span files; it does not repeat what a header already says.

## What snob is, and the lines it does not cross

`snob` is an Instagram CLI (binary `snob`, crate `snob-cli`, version in
`[workspace.package]` of `Cargo.toml`). It walks followers and following lists
and crosses them (`unfollowers`, `fans`, `friends`, `scan`), shows profiles,
stories, highlights, posts and reels and downloads their media, imports
Instagram's own data export (`import dyi`, no network at all), and monitors
accounts over time (`watch`, with webhooks). It signs in as the user, through
Instagram's own **web** API (`www.instagram.com`, never the mobile API), and by
default sends every request from a Chromium-based browser installed on the
machine (Chrome, Edge, Brave, Chromium), headless, on a profile of snob's own.

Hard lines, each enforced by code or a test, never by convention alone:

- **Two writes, ever: follow and unfollow.** No block, remove-follower, like,
  comment, message, share. `snob_ig::graphql::Mutation` has exactly two
  variants and its matches have no wildcard arm, so a third write is a compile
  error; a test in `graphql.rs` pins the operation names. `IgClient::post`
  (in `crates/snob-ig/src/client/write.rs`, the only file that writes) takes a
  `Mutation`, never a path.
- **Nothing ever marks anything as seen**: stories, reels, posts, the activity
  feed, threads, search history, time spent, view counts.
  `crates/snob-core/tests/no_seen.rs` fails on any known spelling of such a
  call in any `.rs` file (it is the only file allowed to name them), and
  `snob_ig::allowlist` refuses every call it does not name. Reading a reel
  does not register a view; sending the seen call would.
- **The user's own browser is never touched.** No reading of its cookie store
  (on Windows that would mean defeating App-Bound Encryption). snob logs in
  in a window it opens on its own profile, or takes a pasted `sessionid`, or
  relays its own headless browser to the user's (`login --remote`). The
  password is never seen by snob except as keystrokes relayed over `--remote`,
  which are passed to its browser and kept nowhere.
- **A human pace, a hard stop on the first push-back.** See "Talking to
  Instagram" below. Slower is always acceptable; faster has to be argued for
  in the code, next to the number.
- **Consent before enumerating someone else's lists**, asked before the
  account is even resolved (resolving is already a request).

## Workspace map

Rust 2024 edition, resolver 3. Dependency direction is one-way:
`snob-cli -> snob-store, snob-ig -> snob-core`; `snob-ig` never depends on
`snob-store`.

| Crate | What lives there | What must not |
|---|---|---|
| `crates/snob-core` | Domain: `Pk`, models (the JSON contract), set arithmetic, `watch` diff, schedule (cron and `--on/--at/--every`), webhook signing (HMAC-SHA256), `Secret`, `Session`, the `RateBudget` trait and cooldown policy, `clock` | **No I/O.** No file, database, network or directory; `clock` is the single exception |
| `crates/snob-ig` | Instagram web client: `client/` (`read.rs`, `write.rs`, `transport.rs`, `headers.rs`, `cdn.rs`, `media.rs`, `page.rs`, `ask.rs`, `posts.rs`), `web.rs` (calls built as the web app builds them), `allowlist.rs`, `pager.rs` (list walks), `pace.rs`, `graphql.rs`, `error.rs` (classifying answers), `http.rs` (the only place `reqwest` is configured; re-exported) | No SQLite, no keyring, no browser driving (it defines the page request shape only) |
| `crates/snob-store` | SQLite (`store/`, migrations in `store/sql/`), platform paths, keyring/file secret store, account registry (`accounts.toml`), the monitor's `watch.toml`, layout migration | Domain logic that belongs in `snob-core` |
| `crates/snob-cli` | `cli.rs` (clap), `main.rs`, `app.rs` (wires everything), `commands/`, `engine/` (rules: cache, consent, cooldown, walks, estimates, monitor), `output/`, `report.rs` (shared sentences), `ui/` (ratatui views), `headless/` + `cdp/` + `pipe.rs` (the browser), `owner/` (one background process that owns the browsers), `detach.rs` (starting a process that outlives its command, with nothing of the command's), `relay/` (`login --remote`), `power/`, `watch/webhook.rs`, `commands/mcp/` (the MCP server, feature `mcp`) | |

Layering rules inside `snob-cli`:

- **Commands take an `App`** and orchestrate; they never build a client or
  open a database themselves. The exceptions say so in their header
  (`status`, `--dry-run`, `fetch`).
- **`engine` returns data and where it came from; it never decides how
  anything looks.** Wording and exit codes for those outcomes live in
  `commands` and `report.rs`. `engine::list` is the one place every list comes
  out of, in a fixed order: cooldown, consent, counter poll, freshness, walk.
  It knows nothing about delivery (`WatchConfig` stays out of it).
- In `commands/watch/`, `say.rs` is prose (rewording is free) and `wire.rs` is
  protocol (`SENT_SCHEMA = 1`, `PRINTED_SCHEMA = 2`; what is signed, stored
  and deduplicated on `run_id`). An edit to `wire.rs` is an integration change.

## Build, lint, test: what CI runs

The toolchain is pinned in `rust-toolchain.toml` (`1.97.1`, with rustfmt and
clippy); `rust-version = "1.95"` is the floor for `cargo install --git`. Bump
the toolchain together with `tools/ci/Dockerfile` (`FROM rust:1.97.1-...`).
Every command that resolves dependencies takes `--locked`.

`.github/workflows/ci.yml`, job **lint** (Ubuntu):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy -p snob-cli --all-targets --no-default-features --locked -- -D warnings
```

plus a check that `bash packaging/render.sh <version> SHA256SUMS` reproduces
the committed `Formula/`, `bucket/` and `packaging/winget/` (skipped while the
version is unreleased). Never hand-edit those files.

Job **test**, on `ubuntu-latest`, `ubuntu-24.04-arm`, `windows-latest`,
`windows-11-arm`, `macos-latest`:

```bash
# Windows and macOS: clippy again (Linux never compiles cfg(windows)/macOS code)
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
SNOB_TEST_REQUIRE_BROWSER=1 cargo test --workspace --locked
SNOB_TEST_REQUIRE_BROWSER=1 cargo test -p snob-cli --features testing --locked -- --test-threads=1

# Linux: against musl, inside a session bus with gnome-keyring unlocked
dbus-run-session -- bash -c '
  echo "" | gnome-keyring-daemon --unlock --components=secrets
  cargo test --workspace --locked --target "$(uname -m)-unknown-linux-musl"
  cargo test -p snob-cli --features testing --locked --target "$(uname -m)-unknown-linux-musl"'
```

Job **build** (pushes only, not pull requests): `cargo build --release --locked
--target <triple> -p snob-cli` for the five release targets, runs `--version`,
checks the musl binaries are static, and builds the `.deb` with
`cargo-deb@3.7.0`.

`.github/workflows/supply-chain.yml` (also weekly): `cargo deny check
advisories sources licenses`. Not a bare `cargo deny check`: `bans` is off on
purpose and fails. Every `ignore` entry in `deny.toml` needs a reason and the
condition under which it goes; a new release target must be added to
`[graph] targets`.

Locally:

- On the maintainer's Windows machine, the CI lint and test commands above run
  as written (with `SNOB_TEST_REQUIRE_BROWSER` optional). Remember they do not
  compile `cfg(unix)`/Linux code.
- `bash tools/ci/run.sh` runs the Linux job (musl, real Secret Service keyring)
  in Docker; `tools/ci/ci.sh` is what the container runs and must stay in step
  with `ci.yml`.
- **Never export `RUSTFLAGS`**: it replaces `.cargo/config.toml` rather than
  merging, silently dropping `+crt-static` and `/Brepro` on Windows (the
  binaries would then need the VC++ redistributable). Add flags to a table in
  `.cargo/config.toml` or to a second config file, as `release.yml` does.

## Tests

- Unit tests sit in `#[cfg(test)] mod tests` at the bottom of each file;
  integration tests in `crates/<crate>/tests/`, with shared fixtures in
  `tests/common/`. Test names are sentences stating the property
  (`only_a_walk_against_a_test_server_skips_the_waits`).
- **No test reaches Instagram.** HTTP is mocked with `wiremock`, or with the
  fake Instagram in `crates/snob-cli/tests/common/ig.rs` (a `World` served in
  the web app's shapes, whose Relay endpoints are an independent oracle that
  answers 400 to a malformed form). `IgClient::is_live` decides by address, so
  every wait and pace is off against a local server and on against
  instagram.com, and no caller can choose otherwise.
- **The `testing` feature** (`snob-cli/testing` -> `snob-ig/testing`) adds the
  hidden seam: `--sandbox-root`, `--ig-base-url` (requires `--sandbox-root`),
  `--through-the-browser` (requires `--ig-base-url`), `point_every_client_at`,
  `SANDBOX_BASE`, `with_page`. A release binary contains none of it, and
  `crates/snob-core/tests/sandbox.rs` fails if any of those names appears
  without `#[cfg(feature = "testing")]` directly above it. `tests/sandbox.rs`,
  `tests/headless.rs` and `tests/web.rs` in `snob-cli` need the feature;
  `--workspace` alone runs none of them. Do not narrow the CI line with
  `--test`: it would skip the bin's own unit tests.
- **Tests that need a browser** (`browser_pipe.rs`, `headless.rs`, some
  `owner`/`headless` unit tests) skip when none will start, and fail instead
  when `SNOB_TEST_REQUIRE_BROWSER=1`. Run them with `--test-threads=1`.
  Chromium refuses to run as root.
- **No test may touch the real keyring** (service `snob-ig`). Every
  `SecretStore::new(` in test code is followed by `.with_service(` in the same
  statement; `crates/snob-core/tests/keyring.rs` reads the source to enforce
  it. Binary-level tests use `--sandbox-root`, which forces the file backend
  and a keyring namespace of its own.
- **Source-reading guards** in `crates/snob-core/tests/` (shared walk in
  `common/mod.rs`, over tracked and untracked-but-not-ignored files):
  `no_seen.rs`, `keyring.rs`, `sandbox.rs`, and `language.rs`, which scans
  `rs sql md toml yml yaml sh ps1 rb json` (this file included) for Spanish
  (escape: `i18n-allow`), British spellings (escape: `spelling-allow`), runs of
  spaces inside a sentence in `.rs` (escape: `layout-allow`), and a
  `snob ...` command to be typed back that contains an at sign (no escape).
  `.agents/` (vendored skills) and `pnpm-lock.yaml` are skipped.
- Hand-run only: `cargo test -p snob-cli --features testing --test headless
  serve_the_fake_world -- --ignored --nocapture` serves the fake Instagram for
  `SNOB_TEST_SERVE_SECS`; `tools/parity/tui.sh` diffs the full-screen views of
  two `testing` builds frame by frame in tmux.
- Migrations: the `validate()` test runs the whole chain; it is mandatory.

## Talking to Instagram

All numbers below are in code with their source next to them; change one only
with a documented reason in the same place.

- **Transport.** By default every request leaves from a tab on instagram.com
  in a headless installed browser, sent with `fetch()`, so handshake, HTTP/2,
  cookie jar, client hints and `Referer` are the browser's own
  (`headless/`, `snob_ig::client::page`). `SNOB_NO_BROWSER=1` sends with
  `reqwest` instead, and Instagram can tell. One browser per account, run by
  the **owner** process (`owner/`, the hidden `__browser-owner` command), closed
  `headless::IDLE` (5 min) after its last request; `SNOB_NO_OWNER=1` makes a
  command run its own. Nothing that spends no network starts a browser.
- **Allowlist.** Every call snob may send is a value in `snob_ig::allowlist`
  (call sites pass those values, never a path or a name). The tab's guard
  (`headless::guard`) pauses every API call the page makes, the app's own
  included: GETs go on, a POST only if `allowlist::refused` lets it.
  `doc_id`s are discovered from the page's JavaScript, never hard-coded.
- **Headers.** Every value is the page's, or computed from the User-Agent of
  the browser actually installed; what cannot be computed is left out. An
  absent header is honest, an invented one is not, and no header is a
  disguise (`client_hints.rs`, `headers.rs`). TLS handshake, HTTP/2 settings
  and header order are never imitated.
- **Pace** (`snob_ig::pace`): web app's page size (12), 1 to 3 s between pages
  of an action (`STEP_MS`), 1.5 to 4 s dwell before a list (`DWELL_MS`), a
  5 to 15 min rest after 40 pages (`PAGES_PER_SITTING`, `SITTING_PAUSE_MS`),
  `HARD_PAGE_CAP = 2000`. Under it, the persistent GCRA buckets of
  `snob-store/src/store/rate_budget.rs`: pace (3.83 s emission, burst 20),
  daily (about 2,000 requests), and writes (burst of 3, then one every 15 min).
  Lists read at most 2,000 accounts in any 24 h, 1,000 for a week after a
  push-back (`budget::accounts_per_day`); `--same-day` reads past it on
  purpose. The budget outlives the process. **The sitting and the rest it
  owes are kept in the account's database and outlive the process** (table
  `sitting`, `snob_core::budget::Sitting`): the page that fills a sitting
  draws its rest and writes down when it ends, whether or not its walk goes
  on, and the next page any walk of that account reads (the pager, a
  profile's mutual followers), in this process or another, waits for it
  first. So a walk ending on its cap, a run stopped during its rest and a
  list read a page at a time all still rest after 40 pages; a sitting not
  full that is left alone for the longest rest (15 min) ends. `--max-wait`
  refuses a wait for the budget (asked with `read_wait`/`write_wait` before
  the charge, so the refusal costs nothing) and never cuts a sitting's rest,
  which would only be owed again by the next run.
- **Push-back is a hard stop.** The first refusal stops the run and starts a
  cooldown: 2 h for a throttle, 12 h for an action block, 30 min for a
  challenge (`snob_core::budget`, pinned by
  `the_cooldowns_are_the_documented_ones`). A push-back on one account brakes
  every account on the machine for an hour (`shared.db`, `common_brake`). The
  app's own calls in the tab are listened to for push-back too
  (`headless::listen`). Never write a retry loop against Instagram (the
  project this was inspired by did `catch { continue }`). During a cooldown
  nothing is sent, not even the counter poll; stored lists are served with a
  dated warning. `SNOB_IGNORE_COOLDOWN` exists only so a bug of ours cannot
  lock anyone out; it stays out of the help.
- **Cache.** A list's counter is polled before any walk (one request instead
  of a walk when nothing moved, `engine::freshness`). Each walked page commits
  in its own transaction, so stopping is saving.
- **The write regime** (`commands/follow.rs`, `client/write.rs`): one account
  per invocation and no flag that batches; the question is asked before the
  request (`-y` answers it in advance); the write pays the write bucket on
  top of the read buckets; it needs a CSRF token and refuses without one; it
  sends `Origin`; it follows no redirect; and once in flight it is **not**
  abandoned on Ctrl+C (the only exception to "Ctrl+C cancels at once"; every
  read races the cancel token).
- **Consent.** Walking another account's lists asks first, before resolving
  it; no terminal and no `-y` is a refusal with exit 130 and nothing spent.
  `watch once` has no `-y`: the monitor takes consent from its configuration.
  `stories`, `highlights` and `profile` do not ask: consent covers walking
  somebody's followers or following, and the only list `profile` reads is the
  viewer's own following.
- **Identity is the numeric `pk`, never the username.** `snob_core::Pk` is a
  newtype with no `Deref`, no arithmetic and no `ToSql`/`FromSql`; SQLite goes
  through `snob_store::store::pk_to_sql`/`pk_from_sql`. Crossings are by pk.
  Their arithmetic is tested: unfollowers + friends = following, fans +
  friends = followers.

## Secrets, network and local data

- **Session storage**: system keyring (Credential Manager with
  `CRED_PERSIST_LOCAL_MACHINE`, Keychain, Secret Service), or with
  `--no-keyring` a `0600` file in a `0700` directory (DPAPI-sealed on
  Windows). One session per account. Credential fields are `Secret`: `Debug`
  prints `<hidden>`, zeroized on drop, read only through `expose()` (keep it
  greppable). Never log or print a token.
- **No cookie jar in `reqwest`**: the `cookies` feature is off workspace-wide
  so `.cookie_store(true)` does not compile; the session travels as an
  explicit header on the one client meant to carry it. `reqwest` is
  configured only in `snob_ig::http` and re-exported from there.
- **Clients that can never carry the session**: the CDN client
  (`client/cdn.rs`: HTTPS, CDN hosts only, never instagram.com) and the
  webhook client (`watch/webhook.rs`), both built by `http::plain`, which has
  no argument for a credential. The webhook follows no redirect, says
  `User-Agent: snob`, refuses `http://` outside private networks, signs with
  `X-Snob-Signature: sha256=<hex>`, and its token and key live in the
  keyring, never in `watch.toml`. Retries are allowed there (it is the user's
  server), never against Instagram.
- **API redirects**: the API client's policy is `none()`; the transport
  follows redirects itself, pays for every hop, and never leaves
  instagram.com.
- **TLS**: rustls (with `prefer-post-quantum`) on four targets, native-tls
  (schannel) on Windows ARM64 only, because neither rustls provider builds
  there without LLVM. HTTP/2 is required (HTTP/1.1 draws 429s). Default trust
  is the platform store; `--strict-roots` narrows to Mozilla's roots (refused
  on Windows ARM64, where it cannot work) and `--tls-extra-root` adds PEM
  files to that. Neither applies to the webhook. No C crypto library is
  linked (RustCrypto `hmac`/`sha2` for signing).
- **The browser's DevTools protocol is on a pipe** (`--remote-debugging-pipe`,
  `pipe.rs`), never a port: a port hands the session to any local process. The
  owner's socket is reachable only by the same user (Unix: `0600` socket,
  `peer_cred` checks; Windows: named pipe with a DACL for this user, owner
  verified on both ends). The relay page for `--remote` gets JPEG frames and
  posts input only, behind a one-visit token and a key in the URL fragment;
  the protocol never crosses the tunnel. It listens on a `0600` Unix socket in
  the data directory, and falls back to a TCP port on `127.0.0.1` only on
  Windows or where the socket path cannot be forwarded by `ssh -L`
  (`relay::can_be_forwarded`).
- **Rotated cookies are written back** from the browser to the store when a
  command leaves it (`headless/write_back.rs`); a login is authoritative.
- **Local data is per user, never per directory** (`snob-store/src/paths.rs`,
  app name `snob-ig`): on Windows the Local, not Roaming, directory (WAL on a
  synced folder corrupts SQLite), created with a user-only DACL and
  `FILE_ATTRIBUTE_NOT_CONTENT_INDEXED`. Per account under `accounts/<pk>/`;
  `shared.db` for what accounts share; the account's database also keeps
  what agents were told yes to through `snob mcp` (`mcp_acts`, a week of
  rows, the last day shown by `snob status`); `accounts.toml` changed only
  through `Registry::update` (file lock). `watch.toml` and `accounts.toml`
  are read with serde and written from a hand-written template, tested to
  round-trip. `snob status` and `--dry-run` write nothing. `snob purge`
  removes everything but the binary.
- **Migrations** (`snob-store/src/store/migrations.rs`): a published migration
  is never edited; add the next numbered file in `store/sql/` (and
  `store/sql/shared/` for `shared.db`). No `PRAGMA foreign_keys` inside a
  migration; rebuild a table by create, copy, drop, rename
  (`SQLITE_DBCONFIG_DEFENSIVE` forbids `writable_schema`). Counts are not
  `u64` columns (`fallible_uint` is off on purpose).

## The MCP server (`snob mcp`)

Feature `mcp` (on by default; `rmcp` pinned exactly in the workspace
`Cargo.toml`). A stdio server an agent's client starts; `snob mcp config
<client>` prints the snippet for that client's configuration and never
writes it.

- **Every tool call is a child `snob`** with the arguments a person would
  type (`commands/mcp/tools.rs` builds them, `child.rs` runs them), never a
  function call into the commands: their confirmations read standard input,
  which here carries the protocol, the cancellation token is per process, and
  a stray line on standard output would corrupt a frame. So every rule above
  (the budget, the cooldown, consent, the write regime) holds for an agent
  through the same code path it holds for a person. Never let the server
  process itself build a client or open the store.
- Children get the server's global flags, `--max-wait` (5 min by default) and
  `--stop-when-stdin-closes`; a canceled call closes the child's standard
  input and kills it only after a grace period, so a walk saves its page and
  gives back its claim. `SNOB_IGNORE_COOLDOWN` is removed from a child's
  environment.
- **Reads only, unless `--allow-writes`**: status, lists, profiles. One tool
  list for every client, from the server's flags alone. Walking another
  account's lists, and every follow or unfollow, needs a yes for that one
  call, asked in this order (`mod.rs`, `Server::consent`): a form the client
  shows (elicitation, either revision); else, for a client that declared no
  form or answers `elicitation/create` with method-not-found, the chat: the
  call answers, not as an error, with `status: "confirmation_required"`, the
  question and a `confirmation` token, which the agent puts to the person
  word for word and hands back on the same call only on a yes. A decline, a
  dismissal or a timeout in a form is a no, never a reason to ask in the
  chat. `--confirm-in-chat none|lists|all` (`all` when not given on stdio)
  says what the chat may cover; past it the call is `cannot_confirm`. Not
  offered, ever: `login`, `logout`, `purge`, the monitor, `--same-day`,
  downloads (outside the request budget), any flag that writes a file.
- **The yes comes from the client, not provably from the person.** The
  client declares what it can show, and an agent can type a yes in the chat
  itself (or be steered to by a bio it read). The tickets keep a yes to the
  call it was asked about, once; the real gate on a write is the client's
  own permission prompt, so keep `snob_follow` and `snob_unfollow` on "ask"
  (`snob mcp config --allow-writes` says so). The question is composed of
  snob's words and the validated username only (`tools::Question`), never a
  display name, a bio or any other string another person wrote, since it is
  read out to the person in snob's voice.
- **A yes counts once, for the command it was given for** (`seal.rs`). On
  the stateless revision the question goes out with a ticket: a random
  nonce, a deadline ten minutes on in seconds of the process's monotonic
  clock, and an HMAC under a key drawn at start over its kind, the nonce,
  the deadline and the command line (`tools::Plan::binding`, the tool and
  `Plan.args`, which hold nothing about how a call is delivered). Redeeming
  puts the nonce in a set of spent ones, kept until it would have expired
  and at most 4096 (full is refused, never forgotten): the same answered
  call sent twice runs once and then answers `confirmation_used`; a ticket
  for another command, kind or server spends nothing. A call back with the
  ticket and no answer retires it and is asked again. A chat token is a
  ticket of the other kind, carried in the `confirmation` argument, which
  stays out of `Plan.args`; brought to a call that asks nothing it is
  `invalid_arguments`, and one for another call or past its time is no yes
  (the call is asked again, with `previous` saying why). Where a transport
  has sessions, the chat ticket is bound to the session too; stdio has none.
  No ticket outlives the process.
- **Signing in is the person's.** The server's instructions say so, and
  every answer that comes of having no session carries a hint that the
  person runs `snob login` in their own terminal, in place of the command's
  own (`answer::SIGN_IN`). `snob_status` with no session is an answer, not a
  failure: exit 3 is what it was asked, as exit 5 is in a cooldown.
- Arguments are checked, not passed through: an `account` must be a possible
  username, so it can never be read as a flag, and an unknown argument is
  refused rather than ignored. A tool that is not listed (the writes without
  `--allow-writes` included) is a JSON-RPC error, -32602, as the protocol
  asks, not an `isError` result.
- Everything a child prints is passed through `printable` string by string
  before it reaches the client; tool descriptions tell the model that names
  and bios are data, never instructions. What a child printed is read
  without assuming its shape (`answer.rs`): under `panic = "abort"` one
  indexing panic is the server gone, with every call in flight.
- **A few calls at a time, and one that spends** (`commands/mcp/limits.rs`):
  at most 4 calls at once, 1 of them a call that may reach Instagram
  (`snob_list` that polls or walks, `snob_profile`, the writes; not
  `snob_status`, a stored list or a dry run, `tools::Spends`), 1 write, and
  60 calls a minute, which also keeps the tickets alive under `seal.rs`'s
  4096. Taken before anything is asked, so nobody says yes to a call that is
  then refused; past a limit the call answers `busy` at once and is never
  queued; held until the child has exited. Across processes, every child
  that may reach Instagram is run with the hidden `--spend-alone`
  (`tools::Plan::command_line`, outside the binding) and takes
  `accounts/<pk>/spend.lock` in `App::open`, before any request; held
  elsewhere, it exits 1 with `reason: "busy"` in its JSON failure, which the
  server hands the agent as code `busy`. Why: the persistent budget bounds
  the average across processes, but `STEP_MS` and `DWELL_MS` are waited per
  process, so two walks of one account interleave their requests and drain
  the burst at once. A command a person types takes no lock, for now.
- **A record of every yes, and a cap on an agent's writes**
  (`snob_store::store::mcp_acts`). Since nobody can prove who said yes, the
  person sees what was done in their name and the volume stays human. A call
  run on a yes gets the hidden `--confirmed-via form|elicitation|chat` and
  `--confirmed-for=<client>` (the client's `name version`, from the request's
  `_meta` or the handshake, through `printable`, cut to `[A-Za-z0-9 ._/-]`
  and 64 characters; `tools::client_label`), and a write also
  `--mcp-write-cap=<N>`, all after the binding is taken
  (`tools::Plan::command_line`). The child records the act in the account's
  `mcp_acts` table before its first request, in a transaction of its own
  (`commands::common::record_agent_act`); a follow or unfollow is refused
  with nothing sent and nothing recorded when the writes recorded in the
  last 24 hours reach the cap: exit 1, `reason: "mcp_write_cap"`, a hint
  with when the next frees. Attempts count (one Instagram refused, or that
  found nothing to do, still did). Only follow and unfollow are writes, only
  acts recorded this way are counted, on the account whichever server made
  them; a command the person types records nothing. `--max-writes-per-day`
  (10 by default, `mcp_acts::WRITES_A_DAY`) is bounded by what the write
  bucket allows in a day anyway (99, `rate_budget::WRITES_IN_A_DAY`).
  `snob status` (section `--agents`, `agents` in `--json`) lists the last
  day's acts: when, what, how the yes was given and through which client.
- **A long list comes back a page at a time** (`answer::page`): at most 200
  accounts (`answer::PAGE`) and 30,000 bytes of compact JSON
  (`answer::PAGE_BYTES`, as a page of accounts with a picture's address is
  some 65), since the result goes into the model's context twice (structured
  and as text) and clients cut long ones (Claude Code at 25,000 tokens, as
  quoted). `{count, offset, items, next_offset}`, `count` the whole list's;
  a failure's `partial` is one page with no `next_offset`, since what a walk
  stopped short read is not stored as a whole list. The client's `offset` is
  sliced with `get`, never indexed. **A page past the first never walks**:
  `offset` stays out of `Plan.args` (and the binding), and the call is
  planned as the same list read with `--offline`, which asks nothing and
  spends nothing (`tools::Page`); `offset` with `mode: "refresh"` or
  `dry_run` is `invalid_arguments`. It is handed back from the whole list
  the server holds from the call that read it (`commands/mcp/pages.rs`: in
  memory, 4 lists, 30 min, 16 MiB), since the store would not always serve
  the same list: a walk cut by `max_pages` is not a whole one, and a
  crossing checked against Instagram on the first page is refused offline
  when its lists were walked far apart. Not held, the page is read from the
  store; for a call given `max_pages`, it is refused as `page_unavailable`.
- A heartbeat (`notifications/progress`) is sent from the call's own task
  and awaited, never spawned, so none can reach the client after the result.
- **Upgrades.** `snob mcp config` prints a path that outlives one
  (`config::stable_exe`: Homebrew's link, Scoop's `current`, or the first
  `snob` on `PATH`), each candidate taken only if it leads to the running
  binary, and warns when what it prints is in a directory named after a
  version. A running server whose binary was moved, removed or replaced
  (size and modification time, `child::Launch::upgraded`) refuses every
  call with `upgraded` and starts nothing; the client restarts it.
- Codex's snippet carries `tool_timeout_sec` from `mcp::longest_call`: the
  person's answer (`ANSWER_WITHIN`), `--max-wait` (`mcp::MAX_WAIT` when not
  given), the child's grace and a margin. It does not bound a list walked
  inline, whose pace `--max-wait` does not cover.

## Output contract

- **Standard output carries data only**; progress bars, prompts, warnings and
  every full-screen view are drawn on standard error. `snob stories someone
  -i > out.txt` leaves the file empty.
- `--format json|ndjson|csv|xlsx|md|table` and `-o <file>` for lists and
  documents (narrower enums where a format makes no sense, e.g.
  `StoryFormat`); `--json` for status commands; `-o -` is stdout. Not a
  terminal means JSON by default and no interactive view; `--no-interactive`
  forces printing.
- When the result would have been JSON, a failure is also one JSON object on
  the last line of standard error (`main::wording_for`); its `code` is the
  exit's token, and `reason` a finer one when there is one
  (`ExitError::with_reason`, e.g. `busy`, `mcp_write_cap`).
- **Exit codes are stable** (`exit.rs`, `snob_core::watch::RunOutcome`, the
  `watch_runs.outcome` column and the `--json` `outcome` field share one
  vocabulary): 0 worked (a list cut short by `--limit`/`--max-pages` is still
  0), 1 failed or result refused as incomplete, 2 bad command line (clap), 3 no
  or dead session, 4 account needs verifying, 5 throttled or in cooldown, 130
  stopped (Ctrl+C, a confirmation not given, a wait past `--max-wait`, or
  standard input closing under the hidden `--stop-when-stdin-closes`, which
  is how a parent on Windows stops a child in order). `snob status` exits 5 in
  cooldown, whatever was asked. Errors that know their code carry it as
  `ExitError` (with an optional `with_hint`, printed on its own `hint:` line).
- The JSON shapes are the types in `snob-core` (`model.rs`), deliberately
  separate from the wire models in `snob-ig`, which follow Instagram. A JSON
  shape change is a contract change.
- **Every string from another person goes through `snob_core::model::printable`**
  before it reaches a terminal or a JSON list (`output::readable`; a JSON
  reader may be a model, and `serde_json` writes `Cf` as it is): all of `Cc`
  and all of `Cf` removed, plus the invisible letters such as Hangul fillers;
  whitespace controls become a space.
- **No `@` in a command handed over to be typed back** (PowerShell's splatting
  eats an unquoted `@name`, and the command then answers about the user's own
  account). Names are accepted with or without it; the help explains quoting.

## Code conventions

- English everywhere, **US spelling**; the language guard enforces both.
  Comments say why, in full sentences, and carry where a number came from
  (a capture and its date, a measurement, a paper). Bold marks the claim a
  reader must not miss. Prefer one home per rule over copies that drift, and
  say in a comment where the other half of a rule lives.
- Errors: `thiserror` enums in the libraries (`IgError`, `PathError`, ...),
  `anyhow` in `snob-cli` with `ExitError` for anything with a specific exit
  code; `exit::exit_code_for` digs the code out of the chain. Instagram's
  answers are classified in `snob_ig::error` (status codes lie; the body says
  why), and everything in `snob_ig::model` but `pk` is optional.
- Lints (`[workspace.lints]`): `undocumented_unsafe_blocks` (every `unsafe`
  block has a `// SAFETY:` comment; there is no `unsafe fn`) and
  `return_self_not_must_use`. Lints are added one at a time, each for a
  defect this codebase had; no `pedantic`/`nursery` groups.
- Dependencies: every entry and feature in `Cargo.toml` is commented with why
  it is there and why its features are what they are; keep it that way.
  Features are narrowed to what is used (no `#[tracing::instrument]`, so no
  `attributes`; no `env-filter`; `SNOB_LOG` is `target=level` pairs). Avoid
  `ring`/`aws-lc-rs`/OpenSSL (Windows ARM64 cannot build the first two).
- Release profile: `opt-level = "s"`, fat LTO, one codegen unit, stripped,
  **`panic = "abort"`**: no destructor runs on a panic or `exit()`, so durable
  state is committed as it is made, and code fed by a browser (the CDP
  dispatcher) never panics.
- The runtime is `tokio` multi-thread with exactly two workers (`main.rs`):
  one worker would let a blocked task starve the Ctrl+C handler.
- Terminal UI: `ratatui` for drawing, keys read through
  `ui/browser/input.rs` (crossterm, bracketed paste) and never
  `console::Term::read_key`; `ui::tui::Tui` is the one claim on the terminal
  and restores it on drop; the 16 ANSI colors only, so the user's theme
  decides. Full-screen views use the alternate screen and print receipts
  after leaving it; the menu draws inline. From the profile card, never call
  `ui::stories::browse` or `ui::highlights::browse` (a second `Tui` and a
  second scratch directory on the same path).
- **Stories are handed to the system viewer.** Media is never rendered in the
  terminal: a view writes the file into the session's scratch directory
  (`ui/browser/scratch.rs`, swept when abandoned) and opens it with `opener`
  in whatever the user already uses for pictures and videos.
- `stories` and `highlights` are separate commands because a highlight has a
  tray level and an items level (`snob highlights someone 2`); `posts` has the
  same two levels. Shared flags live in `cli::MediaActionArgs`.
- The monitor: a run compares against what was last **reported**
  (`watch_marks`), not against the newest capture; a report is queued before
  its mark moves; the delivery queue is drained once per run, after every
  account (`commands/watch/run.rs`); what a run reports as spent is what the
  pacer was charged, measured around the tick. Schedules: `--every` is a floor
  from the previous run and `--on/--at` or `--cron` a calendar; the next run
  is on the calendar and past the floor, both (`--every 2w --on mon` is legal
  and is what cron cannot say). Cron day-of-month and day-of-week combine with
  OR when both are restricted.

## Platforms

Releases: `x86_64-pc-windows-msvc` (built for `x86-64-v2`; Windows 11 only),
`aarch64-pc-windows-msvc`, `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl` (static, no glibc requirement),
`aarch64-apple-darwin`. No Intel macOS build. The five triples appear in
`ci.yml`, `release.yml`, `deny.toml` and `packaging/render.sh`; a new target
touches all four.

Platform-specific code, all of it reached only by the CI job on that OS:

- Windows (`windows-sys`): launching the browser with an explicit handle list
  and a job object (`pipe.rs`), starting a process detached with its own
  handle list and out of the command's job where the job allows it
  (`detach.rs`), the owner's named pipe and its DACL
  (`owner/socket.rs`, `snob-store/src/windows_user.rs`), the hidden window that
  hears the end of a session (`owner/end_of_session.rs`), power requests,
  EcoQoS, the data directory's ACL, the keyring's local persistence, the
  language preference, `QueryUnbiasedInterruptTime` for sleep detection
  (`snob_ig::awake`). Static CRT and `/Brepro` via `.cargo/config.toml`.
- Unix (`libc`): `dup2` of the CDP pipe ends onto fds 3 and 4, `setsid`
  for a detached start (`detach.rs`), the owner's socket with `peer_cred`.
  Linux adds a logind `idle` inhibitor (`zbus`) and `SCHED_BATCH`/`uclamp_max`;
  macOS uses `caffeinate -i -w <pid>` and leaves the browser's scheduling
  alone.
- TLS backend and `--strict-roots` differ on Windows ARM64 (see above).

## Known walls

What Instagram does that snob cannot fix, only report:

- **Truncated lists.** Instagram can stop serving pages well before a list's
  declared size; a walk that falls short of a declared size past
  `pager::TRUNCATION_THRESHOLD` (10,000, a guess from unverified reports) ends
  `StopReason::Truncated`, which is not a success and cannot be a baseline.
  On some accounts `following` hits it on every walk while `followers`
  completes, so `watch status` treats a list never read as unhealthy.
- **Profiles that answer 400** (some business accounts): the id comes from
  search, which carries no counters, so truncation cannot be detected and the
  cache cannot judge freshness (`report::counters_unknowable`).
- **Private accounts the viewer does not follow**: refused before any walk.

## Costs worth knowing

A walk is not compute: about 0.69 s of client CPU for a 12,000-follower walk
against roughly fifteen minutes of deliberate pacing. Optimize for requests
sent and binary size, not speed. Measured, and recorded beside the setting:

- `opt-level = "s"` takes 23.7% off the Windows ARM64 binary for at most
  +7.7% on the CPU-bound exports (`Cargo.toml`, `[profile.release]`).
- Static CRT on Windows: +126,976 bytes per binary, 20 imported DLLs down to
  11 (`.cargo/config.toml`).
- The `xlsx` feature (`rust_xlsxwriter` and `zopfli`): about 498 KiB and six
  crates for one of six formats; on by default, removable with
  `--no-default-features` (also drops reading a zipped export).
- Dropping the Windows keyring's `search` feature removed `regex` and
  `aho-corasick`: 863,744 bytes, 11.4% of the Windows binaries.

## Open questions

Assumptions the code marks as not yet confirmed against the real app; settle
them with a capture (`tools/capture/record.js`, which writes redacted
recordings to the gitignored `tools/capture/out/`; `summarize.js` reads them)
before relying on them further: the paths and thresholds the listener counts as
push-back (`headless/listen.rs`), the prefix of `CLAIM_HANDED_OUT`
(`headless/guard.rs`), whether Instagram varies on `Sec-Fetch-*`
(`client/headers.rs`), and `TRUNCATION_THRESHOLD`.

## Website (`web/`)

A static Astro site (`output: 'static'`, Tailwind 4) served by Cloudflare
Workers from `dist/` (`wrangler.jsonc`, custom domain `snob.dennisgr7.dev`).
Workers Builds deploys `main` and uploads a preview for every other branch.
Node `24.21.0` (`web/.node-version`), pnpm `12.9.1` via Corepack; exact
versions only, a 24 h release quarantine and an install-script allowlist in
`web/pnpm-workspace.yaml`. CI (`.github/workflows/web.yml`, only on changes
under `web/`):

```bash
cd web
pnpm install --frozen-lockfile
pnpm exec playwright install --with-deps chromium
pnpm check && pnpm build && pnpm test   # Playwright against `wrangler dev` serving dist/
```

- `web/DESIGN.md` is the reasoning behind the look; a change to the look
  starts there. `/design-system` shows every token live.
- Every word on the page is in `web/src/lib/content.ts`, shared by the HTML,
  `llms.txt` and the Markdown twin (`index.md`); it may promise nothing the
  README and `--help` do not. The version is read from `Cargo.toml` at build
  time (`src/lib/version.ts`); never copy it.
- Three small scripts, hashed into the CSP; no `is:inline` code, no `style`
  attributes; everything works without JavaScript.
- `pnpm brand` regenerates the committed images in `public/` (after
  `pnpm build`).

## Release and packaging

1. Changes land on `dev` through pull requests; Dependabot also targets `dev`
   (weekly; cargo updates are lockfile-only).
2. A release commit sets `[workspace.package] version` (subject
   `Snob CLI X.Y.Z`); `dev` goes to `main` through a pull request, which
   `claude-code-review.yml` reviews.
3. `git tag vX.Y.Z` on a commit of `main` and push it. `release.yml` refuses a
   tag that is not an ancestor of `origin/main` or disagrees with
   `Cargo.toml`; builds the five targets with no cache and with
   `--remap-path-prefix`, reproducibly (`.zip` on Windows, `.tar.gz` and `.deb`
   elsewhere, times from the tagged commit); publishes `SHA256SUMS` and a
   build-provenance attestation; then runs `packaging/render.sh`, commits
   "Point the formula and the manifest at vX.Y.Z" to `main` and cherry-picks
   it onto `dev`.
4. `Formula/snob.rb` (Homebrew tap) and `bucket/snob.json` (Scoop bucket) are
   read straight from this repository; `packaging/winget/` is generated for a
   manual pull request to `microsoft/winget-pkgs`. `packaging/install.sh`
   (POSIX sh) and `install.ps1` install the latest release
   (`SNOB_VERSION`, `SNOB_INSTALL_DIR`).

The no-writes claim is stated in four places that must move together: the
`.deb` `extended-description` in `crates/snob-cli/Cargo.toml`, the winget
locale manifest that `render.sh` renders from it, `crates/snob-ig/src/lib.rs`,
and `README.md`.

Workflows pin every action to a commit SHA with the version in a comment;
Dependabot moves them. Every workflow starts from `permissions: contents:
read`. `claude.yml` (`@claude`) and `claude-code-review.yml` only take effect
once identical on `main`.

## Git

- Branches: `dev` is where work goes; `main` only takes what `dev` has, via
  the release pull request. Topic branches are named by kind and subject
  (`feat/ssh-remote-login`, `fix/story-browser`, `deps/rustls-0.23.46`) and
  merged into `dev` by pull request.
- Commit subjects are English sentences in the imperative, saying what the
  change does for the person using snob, with no type prefix and no final
  period: "Bind the tunnel's near end to loopback by name", "Take tokio 1.53.3
  and rustls-webpki 0.103.16 for two advisories". The body, wrapped at about
  72 columns, says why: the failure as it happened, what changes, what was
  measured. Dependency bumps name the advisories they answer.
- No `Co-Authored-By`, "Generated with" or other tool trailers, and never push
  unless asked.
- Never commit what snob produces: exports (`*.json`, `*.csv`, `*.xlsx`, ...
  at the root are ignored), data exports, `tools/capture/out/`, or anything
  under `/docs/` (scratch notes, ignored). A new root-level `.md` file is
  ignored unless `.gitignore` lists it.
