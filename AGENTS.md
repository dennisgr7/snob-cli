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
| `crates/snob-core` | Domain: `Pk`, models (the JSON contract), set arithmetic, `watch` diff, schedule (cron and `--on/--at/--every`), webhook signing (HMAC-SHA256) and the one constant-time comparison (`watch::sign::same`), `Secret`, `Session`, the `RateBudget` trait and cooldown policy, `clock` | **No I/O.** No file, database, network or directory; `clock` is the single exception |
| `crates/snob-ig` | Instagram web client: `client/` (`read.rs`, `write.rs`, `transport.rs`, `headers.rs`, `cdn.rs`, `media.rs`, `page.rs`, `ask.rs`, `posts.rs`), `web.rs` (calls built as the web app builds them), `allowlist.rs`, `pager.rs` (list walks), `pace.rs`, `graphql.rs`, `error.rs` (classifying answers), `http.rs` (the only place `reqwest` is configured; re-exported) | No SQLite, no keyring, no browser driving (it defines the page request shape only) |
| `crates/snob-store` | SQLite (`store/`, migrations in `store/sql/`), platform paths, keyring/file secret store, account registry (`accounts.toml`), the monitor's `watch.toml`, layout migration | Domain logic that belongs in `snob-core` |
| `crates/snob-cli` | `cli.rs` (clap), `main.rs`, `app.rs` (wires everything), `commands/`, `engine/` (rules: cache, consent, cooldown, walks, estimates, monitor), `output/`, `report.rs` (shared sentences), `ui/` (ratatui views), `headless/` + `cdp/` + `pipe.rs` (the browser), `owner/` (one background process that owns the browsers), `detach.rs` (starting a process that outlives its command, with nothing of the command's), `jobs/` (a list walk run as a job that outlives the call that started it: the hidden `__job start|run|show|list|cancel`, and `snob jobs`), `relay/` (`login --remote`), `power/`, `watch/webhook.rs`, `commands/mcp/` (the MCP server, feature `mcp`; over HTTP, `mcp-http`) | |

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
- **Jobs in tests** (`tests/sandbox.rs`): a walk still going when it is asked
  about is a followers mock with `set_delay` at a higher priority
  (`fake_instagram_slow`), answered at once after its first `n` times.
  **Every job test stops its jobs and waits for their locks before its
  `TempDir` drops** (`Jobs`, on drop, a panic included): a job is a detached
  process, and one left running writes into a directory being removed, and on
  Windows keeps it from being removed.
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
- **The MCP key** (`snob mcp key`, `commands/mcp/key.rs`): one key for
  `snob mcp` served over the network, `snob_mcp_` plus 43 base64url
  characters of 256 random bits; any other text is refused, from the
  keyring and from `SNOB_MCP_KEY` alike. **Keyring only** (`Kind::McpKey`,
  like the monitor's secrets; `--no-keyring` does not change it): on a
  machine without one, `SNOB_MCP_KEY` in the server's environment is the
  way, and it wins over the keyring wherever it is set, which `show` and
  `revoke` warn about; `new --env` makes one for it, printed and kept
  nowhere, the keyring not even read. A client reads its copy from `SNOB_MCP_TOKEN`, never
  the server's name. The key is printed only to a terminal unless `--print`
  (an agent with a shell would keep it in its transcript), and is named
  everywhere else by a 12-digit fingerprint. Neither variable reaches a
  child of a tool call or a job (`jobs::NEVER_INHERITED`). **`snob mcp
  --listen` does not start without it**: a TCP port on loopback is open to
  every local user, unlike the owner's pipe, and the key is what tells the
  person's client from anything else that connects (on Unix, `--listen
  socket` serves where nobody else can connect, and the key is required
  there too, since a tunnel lets in whoever reaches its far end). `snob
  mcp key` is behind the `mcp-http` feature; `Kind` has no feature gate, so every build
  purges the key; no test writes it to the
  keyring outside `SecretStore::with_service`, and the sandbox tests reach
  it through `SNOB_MCP_KEY` only.
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
  rows, the last day shown by `snob status`); its jobs under
  `accounts/<pk>/jobs/<id>/` (`job.json`, `state.json`, the walk's output and
  log, `0600` on Unix; a finished job's result kept a day, the job a week, at
  most 50, swept by a start and by `snob jobs` and the `__job` reads when
  the jobs lock is free, and a result past its day is not handed back,
  swept or not), which `snob logout` stops and `snob purge` stops and
  removes, both before the owner of the browsers is released, so no walk
  starts it again into what is going, and **both holding the account's jobs
  lock until its session is gone** (an account that never ran a job
  included), since a start checks the session again under that lock and
  makes nothing without one; `accounts.toml` changed only
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
`Cargo.toml`). A stdio server an agent's client starts, or with `--listen`
(feature `mcp-http`, on by default) the same server over HTTP on loopback,
behind the MCP key; `snob mcp config <client>` prints the snippet for that
client's configuration and never writes it (with `--remote <url>`, one that
reaches a server already listening: no command, no key, see "Over HTTP").

- **Every tool call is a child `snob`** with the arguments a person would
  type (`commands/mcp/tools.rs` builds them, `child.rs` runs them), never a
  function call into the commands: their confirmations read standard input,
  which here carries the protocol, the cancellation token is per process, and
  a stray line on standard output would corrupt a frame. So every rule above
  (the budget, the cooldown, consent, the write regime) holds for an agent
  through the same code path it holds for a person. **The server process
  itself never builds an Instagram client or opens a database**; the one
  thing it reads is the MCP key, to serve over HTTP.
- Children get the server's global flags, `--max-wait` (`mcp::Waits`: the
  server's own when given, else 5 min, and an hour, `mcp::JOB_MAX_WAIT`, for
  a job's walk, which nobody waits on) and `--stop-when-stdin-closes`; the
  server's globals carry no `--max-wait` of their own, since clap refuses it
  twice. A canceled call closes the child's standard
  input and kills it only after a grace period, so a walk saves its page and
  gives back its claim. **A child runs in a process group of its own**
  (`child::Launch`), so closing its input is the only stop it gets: the
  Ctrl+C that stops the server would otherwise reach it too, and the input
  closing after it would be a second stop, which exits at once.
  `SNOB_IGNORE_COOLDOWN` is removed from a child's environment.
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
  chat. `--confirm-in-chat none|lists|all` (`all` when not given on stdio,
  `lists` with `--listen`) says what the chat may cover; past it the call is `cannot_confirm`. Not
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
  (the call is asked again, with `previous` saying why). Where a request
  carries a session, the chat ticket is bound to it too (over HTTP, the
  `Mcp-Session-Id` of the 2025-11-25 revision, read from the parts rmcp
  hands the handler). **A stateless call over HTTP carries none, as stdio
  does**, so there the ticket is bound to the command alone, and any
  conversation that holds the key may redeem it, once, before it expires.
  No ticket outlives the process.
- **Signing in is the person's.** The server's instructions say so, and
  every answer that comes of having no session carries a hint that the
  person runs `snob login` in their own terminal, in place of the command's
  own (`answer::SIGN_IN`). `snob_status` with no session is an answer, not a
  failure: exit 3 is what it was asked, as exit 5 is in a cooldown. **Every
  hint that is advice to the person at a terminal is replaced the same
  way**, by the failure's `reason` (`answer::agent_hint`: `too_many_jobs`,
  `no_such_job`, `job_stopping`, `nothing_stored`, `stored_moments`,
  `budget_wait`, `busy`, `mcp_write_cap`, `walked_elsewhere`), with the
  tools that do it instead,
  and a push-back the call met itself, whose cooldown the failure does not
  date, by `answer::PUSHED_BACK` (ask `snob_status` when it lifts); so
  every refusal a cooldown already held is behind carries
  `cooldown_until`, a crossing's of two stored lists too far apart
  included (`engine::cooldown::check_same_moment_dated`); a failure's
  message is its sentences, the records of the log and the progress lines
  left out (`jobs::show::sentences`).
- **`snob_profile` stays inline, not a job**: its walk of the mutual
  followers is at most `MUTUAL_PAGE_CAP` (10) pages, which count toward the
  account's sitting as a list's do, so it may wait out one rest between
  sittings, up to 15 minutes, before or between those pages (one an
  earlier walk left owed, or one its own pages draw; a rest paid first
  leaves a sitting ten pages cannot fill), which its description says and
  `mcp::longest_call` counts.
  `openWorldHint` is per tool (`tools::Spec::open_world`): false for the
  tools whose every plan spends nothing (`snob_status` and the three job
  tools).
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
  queued; held until the child has exited. **Every refusal that costs
  nothing to know is asked before anything is asked too**
  (`Server::refused_ahead`), through a child that sends and writes nothing,
  with the refusal the call's own child would give: a walk run as a job
  through `__job start --check` (the account's jobs, since a job's walk
  holds no permit once its call answered: `too_many_jobs`, `job_stopping`,
  `walked_elsewhere`; and in a cooldown a refresh, or a list with nothing
  stored to serve), and a write through `follow --check-ahead` (a job
  walking or `spend.lock` held, `busy`; no CSRF token; the cooldown, with
  `cooldown_until`; `mcp_write_cap`; a wait for the write bucket past
  `--max-wait`, `budget_wait`). In a cooldown a stored list is still asked
  about: the command carries `-y`, which a question skipped would hand a
  walk with no yes once the cooldown lifts. What can still refuse after the
  yes: what changes between the check and the yes (another process taking
  `spend.lock`, a cooldown starting, the budget or the cap taken by another
  call), what only a request tells (a write already so, a name that is
  nobody's), and a crossing whose stored lists a cooldown serves were walked
  too far apart. Across
  processes, every child that may reach Instagram is run with the hidden
  `--spend-alone` (`tools::Plan::command_line`, outside the binding) and takes
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
  and 64 characters; `commands::common::client_label`), and a write also
  `--mcp-write-cap=<N>`, all after the binding is taken
  (`tools::Plan::command_line`). The child records the act in the account's
  `mcp_acts` table before its first request, in a transaction of its own
  (`commands::common::record_agent_act`); a follow or unfollow is refused
  with nothing sent and nothing recorded when the writes recorded in the
  last 24 hours reach the cap: exit 1, `reason: "mcp_write_cap"`, and
  `retry_at`, when the next frees. Attempts count (one Instagram refused, or
  that found nothing to do, still did). Only follow and unfollow are
  writes, only acts recorded this way are counted, on the account whichever
  server made them; a command the person types records nothing, and nor
  does a walk in a cooldown, which reads nothing. `--max-writes-per-day`
  (10 by default, `mcp_acts::WRITES_A_DAY`) is bounded by what the write
  bucket allows in a day anyway (99, `rate_budget::WRITES_IN_A_DAY`).
  `snob status` (section `--agents`, `agents` in `--json`) lists the last
  day's acts: when, what, how the yes was given and through which client.
- **A long list comes back a page at a time** (`answer::page`): at most 200
  accounts (`answer::PAGE`) and 30,000 bytes of compact JSON
  (`answer::PAGE_BYTES`), since the result goes into the model's context
  twice (structured and as text) and clients cut long ones (Claude Code at
  25,000 tokens, as quoted). **An account goes without its picture's
  address** (`answer::LEFT_OUT`, `pfp_url`): a signed URL of 300 to 500
  bytes that answers nothing an agent is asked, and with it a page held
  some 65 accounts. `{total, count, offset, items, next_offset}`: `total`
  the list's, `count` what the call hands back across its pages, at most
  `limit`. **The server applies `limit`** (`tools::Page::limit`), never the
  command's `--limit`, so a call given one still says the whole list's
  `total`, and one it cut to a single page says so in a sentence
  (`answer::limited`); the limit stays out of `Plan.args`, so a yes is to
  the walk whatever it trims, and every limit pages from the same list held.
  A failure's `partial` is one page with no `next_offset`, since what a walk
  stopped short read is not stored as a whole list. **A list is never
  handed back as more than it is**: where it came from is the walk's
  `served` progress line (`Progress::served`, read from a job's log, or
  from a stored read's own standard error, which `mode: "stored"` and a
  later page get `--progress-json` for), with its date, a sentence beside
  it when it was not read now (`answer::not_read_now`), and `complete:
  false` with what was read of how many and a sentence
  (`answer::incomplete`; for a crossing, that results are missing and the
  ones given are right) when a walk cut short by `max_pages` exited 0. The
  client's `offset` is sliced with `get`, never indexed. **A page past the
  first never walks**: `offset` stays out of `Plan.args` (and the binding),
  and the call is planned as the same list read with `--offline`, which
  asks nothing and spends nothing (`tools::Page`), whatever its mode;
  `offset` with `dry_run` is `invalid_arguments`. It is handed back from the
  whole list the server holds from the call that read it
  (`commands/mcp/pages.rs`: in memory, 4 lists, 30 min, 16 MiB), under the
  call's binding without `--refresh`, so a refresh and a later page of it
  meet, and with what the first page said of the whole list (`served`,
  `cooldown_until`, `complete`), since the store would not always serve
  the same list: a walk cut by `max_pages` is not a whole one, and a
  crossing checked against Instagram on the first page is refused offline
  when its lists were walked far apart. Not held, the page is read from the
  store; for a call given `max_pages`, it is refused as `page_unavailable`.
- **A walk can outlive the call that started it, as a job** (`jobs/`, in
  every build: `std` processes and threads, no `mcp` feature). `snob __job
  start` checks the walk, makes the job under the account's `jobs/.lock`
  (one running per account: the same command again is `reused`, the
  `--confirmed-*` aside, another is `too_many_jobs`, and one asked to stop
  that has not yet is `job_stopping`, never reused; a list a lost job may
  still hold is `walked_elsewhere`, with until when, `retry_at` for a
  program, and one that never started its walk, with no walk's pid in its
  state, holds none, `jobs::Seen::claim_frees_at`), starts its supervisor
  detached (`detach.rs`) and answers with **one** compact JSON document:
  the record, and the walk's answer under
  `finished` when it ended within `--wait`. Asked to stop before it answered
  (its standard input closing), it stops the job it made, since nobody would
  ever be told the id. The supervisor, `__job run <id>`, holds the job's
  `lock` all its life (a held lock is a running job, `jobs::derive`; asking
  writes nothing, `paths::lock_is_held`), **runs itself** (`current_exe`,
  never a path from disk) with its own global flags (`argv::globals`), and
  **checks the stored command line again** (`argv::check`: the five list
  commands, judged by clap, and an allowlist of flags; anything else ends
  `failed`, `refused_argv`, nothing run). Its walk gets `--spend-alone`,
  `--stop-when-stdin-closes` on a pipe it holds, and `--progress-json`, which
  wins over `--no-progress` and is what `show` reads progress and the time
  left from. Stopping is a `stop` file, read every second; the supervisor
  closes the walk's input and kills it through its own handle only after
  the grace. No pid read from a file is ever signaled. `show`, `list` and
  `cancel` look only in the jobs of the account the run acts as: another
  account's id is `no_such_job`. A Windows job object that refuses breakaway
  takes the job with its client: `detached: "in_client_job"`. On Windows
  the detached supervisor hears no end of session, so a job running at
  sign-out or shutdown is read as `lost`; and a supervisor that comes to a
  job every reader already takes as lost leaves it so (`started_late`)
  rather than run it.
- **To an agent, a list that may be walked is a job** (`tools::Run::Job`):
  `snob_list` in mode `auto` or `refresh` (not `stored`, a dry run or a later
  page; the writes never) runs `__job start --wait=<wait_seconds>s --tool
  --arguments --started-by -- <the command and its --confirmed-*>`, with no
  `--spend-alone` (the supervisor gives the walk that). `wait_seconds` (20 by
  default, at most 50, `tools::MOST_WAIT_SECONDS`, so a call answers inside a
  minute) stays out of `Plan.args`, the binding and what the job records
  (`tools::recorded`). Ended within it, the call answers as an inline walk
  would, with `job_id` beside it and its pages held as any list's; otherwise
  with `{status: "running", done: false, job_id, progress, eta_seconds,
  detached, poll_with: "snob_job"}` and a sentence (`commands/mcp/job.rs`).
  `snob_job` (`__job show <id> --wait`), `snob_jobs` (`__job list`) and
  `snob_job_cancel` (`__job cancel <id>`) are offered always, spend nothing,
  and take a `job_id` only as `JobId::parse` reads it. A finished job answers
  through `answer::from` with the plan rebuilt from the tool and arguments it
  recorded, its pages read again from the job's `out.json` (`offset`); a
  result swept a day later is `result_gone`, a lost job `job_lost`. **All job
  I/O goes through those `__job` children**, so the server still opens no
  store and reads no job's file.
- **A client that takes MCP Tasks gets a task for that job**
  (`commands/mcp/tasks.rs`; SEP-2663 as rmcp 3.8.0 has it: `tasks/get`,
  `tasks/update`, `tasks/cancel`, and no `tasks/result` or `tasks/list`, so
  jobs are listed by `snob_jobs`). The server declares the extension. A
  `snob_list` run as a job, for a client that declared it (at `initialize`,
  or in a stateless request's `_meta`), waits at most `tasks::WAIT_SECONDS`
  (5) whatever `wait_seconds` says, and answers with a task whose id is the
  job's. `tasks/get` and `tasks/cancel` are the children `snob_job` (with no
  wait) and `snob_job_cancel` run, under the same limits and the upgrade
  check; an id that is not one of the account's jobs is `invalid_params`
  "unknown task". Queued or running is `working`, with `pollIntervalMs` a
  tenth of the least time left (5 to 60 s); a walk that ended is
  `completed` with the result `snob_job` would give, an error result
  included; a canceled job is the extension's canceled status, and a lost
  one `failed` with `job_lost`. `ttlMs` is the week a job is kept. **Nothing
  is asked inside a task**: the yes is settled before the job starts, so
  `tasks/update` only checks the id. Not rmcp's `TaskManager`, which keeps
  tasks in memory and aborts one after five minutes.
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
  longer of a call that asks (the person's answer, `ANSWER_WITHIN`, then
  the longer of `--max-wait`, `mcp::MAX_WAIT` when not given, and a list's
  wait for its job, `MOST_WAIT_SECONDS` plus
  `jobs::start::SUPERVISOR_WITHIN`) and a profile (`--max-wait` and the
  longest rest between sittings), then the child's grace and a margin. A
  list's walk is bounded by it however long it is: the call hands back the
  job and the walk goes on.
- **Over HTTP, for a tunnel of your own** (`snob mcp --listen <addr>`,
  `commands/mcp/http.rs`). **Loopback only**: any other address is refused
  with exit 2, and TLS is the tunnel's job (`--public-url`, `https` only,
  adds the tunnel's host and origin to those taken). **On Unix, `--listen
  socket`**
  (`http/socket.rs`) serves on `mcp.sock` in the private data directory,
  `0600`, each connection checked for this user or root (`peer_cred`): the
  one way to close the gap below, since nobody else can connect to it or
  take its name while snob is not running. One server holds the name at a
  time (`mcp.sock.lock`, held for its life); holding it, a socket left at
  the name by a killed server is replaced, and anything else there is left
  alone and refused. Refused on Windows with exit 2. **The key is
  mandatory** (no key, exit 1 and a hint), because loopback TCP is
  reachable by every local user, unlike the owner's pipe; startup says its
  source and fingerprint, never the key, and with `--allow-writes` warns
  that whoever holds it can call the writes (and confirm them in the chat
  under `--confirm-in-chat all`). **The reverse does not hold: the server
  does not prove itself to its clients.** A client or tunnel sends the key
  to whatever answers on the port, so another local user who binds it while
  snob is not listening is handed the key; `--listen` is for a machine where
  nobody else can run code, and startup says to replace the key if the
  port could have been taken (revoking the keyring's, or replacing
  `SNOB_MCP_KEY`, by where the key came from). Bound to `127.0.0.1` or
  `[::1]`, it also holds the same port of the other family (`http::twin`),
  since `localhost` may resolve to either, and, on a port the person
  named, refuses to start, exit 1, when something else holds it (its
  family's wildcard held with exclusive use included) or it cannot be held
  for any reason but the family being absent; on a port the system picked
  (`:0`) it goes on with one family. Each request, in order: the path (`/mcp`
  only, `/.well-known/*` included is 404); the key (`Authorization:
  Bearer`, the scheme in any case), checked **before any of the body is
  read** by comparing HMACs of both sides under a key drawn at start, and
  refused with 401, `WWW-Authenticate: Bearer realm="snob"` and
  `Connection: close`; the body, read by snob, at most 256 KiB (413) within
  30 s (408); at most 8 sessions, a new one past them closing the one left
  idle longest with no call running, since a client gone without a `DELETE`
  keeps its session for the idle time (503 for an `initialize` only when
  every one has a call running; a stateless call opens none and passes);
  then rmcp's own
  checks and the same handler. **The `Host` and `Origin` checks guard
  browsers (DNS rebinding) only; against any other caller the key is the
  only control.** hyper serves HTTP/1.1 with a header (and idle) timeout of
  10 s and a 16 KiB buffer; a connection's first request must arrive within
  2 s. At most 32 connections, **counted against those that have shown the
  key**: past it, the oldest connection that has not is closed for the new
  one, so idle sockets held by another local process lock no client with
  the key out (`http::Open`). `Host` takes the bound address too, so any of
  127/8 works. Sessions idle for 30 min close. The limits, consent, jobs and
  tasks are the stdio server's, shared by every session; a chat token from
  a stateless call is bound to its command only (see "A yes counts once").
  Ctrl+C (or the system, or `--stop-when-stdin-closes`) stops accepting,
  stops every call's child as a canceled call's is stopped, answers a call
  still waiting on a form as not confirmed (`Server::in_a_form` races the
  question against `Server::stopped`), waits for the children
  (`child::GRACE` and 5 s), **then for their answers to be written** (rmcp
  drops an answer it has not handed over once its token is canceled), and
  only then ends the streams and exits 130. **What the tunnel sees**: TLS
  ends at its edge, so its provider sees the key and every answer, other
  people's names and lists included, in plain text, and a connector's vendor
  stores the key; put an access layer in front where the tunnel offers one
  (Cloudflare Access service tokens, for example). **`snob mcp config
  <client> --remote <url>`** (`config.rs`, `render_remote`) prints a
  client's snippet for such a server: the address and how the client sends
  the key, never a command and never the key, which it does not read. The
  client takes it from `SNOB_MCP_TOKEN` in its own environment
  (`config::TOKEN`; each client's spelling of a variable, Codex's
  `bearer_token_env_var`), never from `SNOB_MCP_KEY`, which would replace
  the keyring's key in every snob started where it is set; Claude Desktop
  gets the address alone, for a connector, which Anthropic's side reaches,
  so a loopback address is warned about. The address is `https`, or `http`
  on loopback only (`ssh -L`), and holds no user, password, query, fragment
  or key; clap is handed a parser of its own (`config::RemoteUrl`) so that a
  refused address is not printed back. The server's flags (`--allow-writes`,
  `--confirm-in-chat`, `--max-writes-per-day`) are refused with it, since
  they describe a process this does not start.

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
  (`ExitError::with_reason`, e.g. `busy`, `mcp_write_cap`,
  `walked_elsewhere`, `nothing_stored`, `stored_moments` for a crossing of
  two stored lists too far apart under `--offline`, `budget_wait` for a
  wait past `--max-wait`, and a job's `too_many_jobs`, `job_stopping`,
  `no_such_job`, `refused_argv`).
  `cooldown_until` is when a cooldown that refused the run lifts
  (`ExitError::with_cooldown_until`), and `retry_at` when a refusal that is
  no cooldown stops applying (`ExitError::with_retry_at`: `mcp_write_cap`,
  and `walked_elsewhere`, when the lost job's claim on the list frees),
  both in seconds since the epoch, since the sentence says them in the local
  time of the machine that wrote it.
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
  and all of `Cf` removed, plus the invisible letters such as Hangul fillers
  and the marks that draw nothing (the whole of `Default_Ignorable_Code_Point`:
  the combining grapheme joiner, the Mongolian and supplementary variation
  selectors, U+FE00 to U+FE0D, and U+FE0E/U+FE0F anywhere but alone right
  after a symbol in the ranges where Unicode puts the bases of presentation
  sequences (a positive list, `takes_a_presentation`), or after a keycap
  base with U+20E3 next; never after a letter, a digit, a combining mark
  of any script or punctuation that is no emoji: one selector per letter or
  mark hides four bits each, a run a byte each); whitespace controls become
  a space.
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
- The `mcp-http` feature (`snob mcp --listen`: hyper's server half and
  rmcp's Streamable HTTP; tokio-stream and sse-stream are the only new
  crates, the rest was in the tree through reqwest): 486,912 bytes, 4.8%
  of the Windows x86-64 release binary (10,180,608 against 9,693,696
  with `--no-default-features --features xlsx,mcp`); on by default.

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
