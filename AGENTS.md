# AGENTS.md

Context for working on this project. `CLAUDE.md` points here; this is the only
instruction document. It describes the current state of the tree; history and
the reasoning behind individual changes live in the commit messages.

**Detailed reasoning lives in the doc-comment above the code it governs, not
here.** Read it before changing anything it names, because it explains what a
number is for, which is what stops it being changed into something that no longer
does its job:

| Subject | Where |
|---|---|
| Request pacing, the numbers and where they come from | `crates/snob-ig/src/pace.rs` |
| The calls snob may send, and the check that refuses the rest | `crates/snob-ig/src/allowlist.rs` |
| How each call is built, in the app's own families | `crates/snob-ig/src/web.rs` |
| The values read from the page and sent back | `crates/snob-ig/src/page_values.rs` |
| Stop conditions of a walk | `crates/snob-ig/src/pager.rs` |
| The browser, what a headless one gives away, the tab | `crates/snob-cli/src/headless/mod.rs`, `tab.rs`, `identity.rs` |
| What the browser listens for (push-back) | `headless/listen.rs` |
| What the tab lets out (the guard) | `headless/guard.rs` |
| The one process that runs the browsers | `crates/snob-cli/src/owner/` |
| The cookie boundary and the protocol client | `cdp/mod.rs`, `cdp/connection.rs`, `pipe.rs` |
| Request budget, cooldowns, the common brake | `snob-core/src/budget.rs`, `snob-store/src/store/rate_budget.rs` |
| The schema | `crates/snob-store/src/store/sql/` |

## What this is

`snob` (Snob CLI) is an Instagram client for the terminal. It tells you who does
not follow you back and tracks changes to your followers and following over
time; follows and unfollows one account at a time; shows an account's page the
way Instagram does; shows and downloads stories, highlights, posts and reels;
and prints everything in stable forms that scripts and AI agents can read. A
single Rust binary that drives a Chromium-based
browser installed on the machine. Windows and Linux on x86_64 and ARM64, macOS
on Apple Silicon.

It is a convenience tool for a person's own accounts, signed in as themselves.
Everything it shows is what the app already shows the same person, read faster
and in a form they can keep. That gap is the reason the project exists, and it
bounds the scope: one person's accounts, at human scale, doing by command what
the person could do by scrolling.

There is no official API for listing followers, so snob asks the same web API
instagram.com asks, with the user's own session, **from a real browser**: every
request leaves from a Chrome, Edge, Brave or Chromium that snob runs without a
window against its own profile, sent with `fetch()` from an instagram.com tab.
Automating that is outside Instagram's Terms of Use, and the realistic outcome is
that Instagram asks the account to verify itself. **Most of the design goes into
being a light, well-behaved client**: modest volume, the app's own requests, and
an immediate stop when the service pushes back.

Commands (`crates/snob-cli/src/cli.rs` is the source of truth; `snob --help` and
`snob <command> --help` print it):

- Session and accounts: `login` (`--paste`, `--browser`, `--add`), `whoami`,
  `account list`, `account use`, `logout` (`--all`), `purge` (`--account`,
  `--dry-run`).
- Lists: `unfollowers`, `fans`, `friends`, `followers`, `following`, `scan`.
- One account: `profile`, `pfp`, `stories`, `highlights`, `posts`.
- One post: `post` (alias `reel`), by its link or its code.
- The only two writes: `follow`, `unfollow`.
- The monitor: `watch` (the scheduled loop), `watch once`, `watch diff`,
  `watch check`, `watch setup`, `watch status`.
- `import dyi`, which reads Instagram's own export and sends nothing.
- Hidden: `__browser-owner` (the process that runs the browsers); with the
  `testing` feature, `--sandbox-root`, `--ig-base-url`, `--through-the-browser`.

Several accounts can be signed in; each command acts as one of them, chosen by the
global `--account`, then `SNOB_ACCOUNT`, then the active account, then the only
one (`account::resolve`). JSON answers and failures name it in `viewer`.

## Rules

Standing instructions from the maintainer. They are not up for re-litigation in
a normal change.

- **Branches**: `main` is stable and only good versions land there; `dev` is
  day-to-day work. Changes come in through pull requests.
- **Everything is written in English**: code, identifiers, comments, user-facing
  strings and documentation. US spelling. `crates/snob-core/tests/language.rs`
  enforces it on every file git would commit, Markdown and config files too, this
  one and `README.md` included. What `.gitignore` declares scratch (`/docs/`) is
  not read. On a false positive fix the test's word list rather than disabling it.
- **`cargo fmt` and `cargo clippy --workspace --all-targets --all-features
  --locked -- -D warnings` before every commit**, and the same without default
  features (see below). Commit messages are in English, imperative, with no
  conventional-commit prefixes.
- **`README.md` is short and read by people first**: what the tool is, how to
  install it, the main commands, the flags for automation. Details belong in
  `--help`. If it ever shows the webhook body as a ```` ```json ```` block,
  `the_readme_publishes_the_body_that_goes_out` (`commands/watch/wire.rs`) checks
  that block against the real payload.
- Prefer the compiled, dependency-free option. Native binary, instant start, broad
  platform support is the point of the project. A new dependency has to argue for
  itself; the trades already weighed and declined are in the comments of the
  workspace `Cargo.toml`, next to the tables they concern.
- **The command line keeps three conventions**:
  - **One question per command, and `-y` answers it in advance.** The flag groups
    in `cli.rs` (`ConsentArgs`, `ProgressArgs`, `StatusOutputArgs`, `FilterArgs`,
    `OutputArgs`, `WalkArgs`) hold this structurally: a flag a command would
    ignore is refused by clap rather than warned about.
  - **`--json` is for a status object; `--format` is for a document.** `whoami`,
    `account list` and the `watch` subcommands take `--json`; everything that
    prints a thing a person reads takes `--format`, with an enum narrowed to the
    forms it has.
  - **Interactivity is detected, and said beats detected.** The full-screen
    browsers are the default exactly when all three standard streams are a
    human's terminal and no flag asked for the printed or downloaded form. `-i`
    forces a browser; `--no-interactive` prints. The one copy of the ordering is
    `cli::browse_decision`; the three-stream predicate is
    `ui::a_human_would_watch_the_listing_scroll_by`. The two wizards (`login` with
    no method flag, `watch setup`) are the written exception, and each has a
    non-interactive route beside it (`login --paste`; `watch.toml` by hand).
  - `--offline` is the one word for "spend no network". The short-flag space is
    deliberately almost empty (`-y -o -d -i`) and a new short flag has to argue for
    itself first. There is no REPL and no global interactive mode.
- **Instagram commands never carry an at sign in examples**: on PowerShell `@` is
  the splatting operator and the argument vanishes. `language.rs` fails the build
  on one in a command to type back. Names are accepted without the sign.

And the domain rules, which exist because breaking them puts a real account at
risk:

- **Two write operations exist, and no third one may be added.** Follow and
  unfollow, one account per invocation, no bulk mode and no flag that makes one.
  Each is paid out of its own, far slower budget (one write per fifteen minutes,
  three in a row), confirmed before it is sent, and a `feedback_required` on one
  is an action block with a twelve-hour cooldown. **Nothing may mark a story as
  seen**, which is a write dressed as a read. Enforcement is structural: a write
  leaves only through `IgClient::post` or as a `web::Ask::Write`, both take a
  `graphql::Mutation`, and neither can be reached without paying the write budget,
  which the variant chooses, so a new write is a build error until somebody has
  written the variant. `crates/snob-core/tests/no_seen.rs` is the backstop: a
  denylist of the seen signals, and the only file that may name them.
- **Never read or decrypt the user's browser cookie store.** The allowed route is a
  browser *we launched* against *our own* profile, which the user logs into
  themselves and which hands the cookies over through its debugging protocol. A
  pasted session is written into that profile, never taken out of anybody else's.
- **On the first 429, `spam:true`, `feedback_required` or `challenge_required`:
  hard stop.** No retry in that run, and the account goes into cooldown. When the
  service says no, the answer is to stop asking. A refusal is never worked around
  by asking somewhere else (`IgError::worth_a_second_route`).
- **Request pacing is not changed without a documented reason.** Slower is always
  acceptable; faster has to be argued for in `pace.rs`. See "Pacing" below.
- **Never walk a real account's lists without the limiter.** Live-API testing is
  done with single, counted requests, by the owner, with the log open.
- **No test may touch the real keyring.** Tests use their own service name through
  `SecretStore::with_service`, and `crates/snob-core/tests/keyring.rs` reads the
  source of every crate to check that they do.

### Real accounts: what an agent must not do

- **Do not run `snob` against real Instagram on your own initiative.** No `login`,
  no list, `profile`, `stories`, `follow`, `watch` or `import` of a live session on
  the maintainer's machine, and nothing that starts a browser on a real profile.
  The environment's keyring and data directory hold real sessions. Every test and
  experiment goes through the sandbox (`--sandbox-root`, the fake in
  `tests/common/ig.rs`).
- A live check is a handful of counted requests, made by the account's owner
  with `--verbose` and `browser-owner.log` open, each step read before the next
  and stopped at the first push-back. Never unattended.
- Never use `SNOB_IGNORE_COOLDOWN` (the undocumented escape hatch in
  `rate_budget.rs`) against a real account, and never lower a number in `pace.rs`
  or `rate_budget.rs` to make a test or a run faster.
- A capture of the real app (`tools/capture/record.js`, its `out/`, and any
  recording kept elsewhere) carries real names and messages even after redaction.
  `out/` is gitignored; never commit one, and delete what it showed once it is
  written down.
- Do not delete or reset the user's data directory, browser profiles or keyring
  entries to "clean up". `snob purge` is the sanctioned route and it is the
  owner's decision.

## Working on it

```bash
cargo build --locked                       # debug binary: target/debug/snob
cargo test --workspace --locked            # everything
cargo test -p snob-ig pager                # one module
cargo test -p snob-cli --test cache        # one integration file
cargo test the_first_run_walks_the_list    # one test by name
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy -p snob-cli --all-targets --no-default-features --locked -- -D warnings
cargo test -p snob-cli --features testing --locked   # the binary itself, on the fake
cargo run -p snob-cli -- --help            # mind the double dash
```

`rust-toolchain.toml` pins an exact compiler (1.97.1); the manifest's
`rust-version` is 1.95, the oldest every locked dependency accepts. The lint
policy lives in the workspace manifest (`undocumented_unsafe_blocks`,
`return_self_not_must_use`), so plain `cargo clippy` gives CI's answer. Every
`unsafe` block carries a `// SAFETY:` comment.

**The `testing` feature is what lets a test drive the program**; everything else in
the suite drives a library. It adds three hidden flags a released binary contains
none of. `--sandbox-root` puts every file a run touches under one directory,
forces the file secret backend, and derives a keyring service name from that root,
which is what actually separates a sandbox from the real credentials.
`--ig-base-url` **requires** `--sandbox-root`; that pairing is the whole safety
argument (`main::wiring`). `--through-the-browser` requires `--ig-base-url` and
sends the redirected requests from the headless browser too, which is how
`tests/headless.rs` drives the path users take; without it a sandbox reaches its
mock server with `reqwest`, so the rest of the suite needs no browser.
`crates/snob-core/tests/sandbox.rs` reads the source to keep all of this out of a
release. Plain `cargo test --workspace` compiles none of it, so run the `testing`
suite too before pushing; CI runs it as its own step on every platform.

**`tests/web.rs` runs the browser path without a browser.** Its tab
(`common::tab::TestTab`) builds each intent with the same `web::build_call` the
headless tab runs and sends it with `reqwest`, against `common::ig::World`: one
invented Instagram that serves both the web app's shapes (`serve_web`) and the
REST path's (`serve_rest`). `World::audit` replays what reached the fake through
`allowlist::refused`, so a test can end by checking that snob sent nothing it may
not.

**`tests/headless.rs` skips where no browser will start**: the Linux runners (no
Chrome on ARM64, no user namespaces for its sandbox on x86_64) and anything
running as root. It runs for real on the Windows and macOS runners, where CI sets
`SNOB_TEST_REQUIRE_BROWSER` so a browser that will not start fails it (and
`tests/browser_pipe.rs`) instead of skipping. Locally on Linux run it as an
ordinary user with a `chromium` on `PATH`. Its tests take turns with the browser,
one at a time, since browsers started together on a loaded machine hold their
profiles past the patience the tests give them; on Windows it is only reliable
single-threaded.

**The two routes show the same thing**: `the_two_paths_show_the_same` in
`tests/headless.rs` runs each command through the browser and through REST on
the same fake and compares the output; the differences allowed are listed in
`EXPECTED_DIFFERENCES`, each with its reason. `tools/parity/tui.sh` does the same
for the full-screen views, between any two binaries.

**The Linux CI job runs on this machine too**, in a container: `bash
tools/ci/run.sh` (from Git Bash on Windows). It is the job that differs most from a
developer's host: musl, a real Secret Service keyring behind a session bus, a
static link. `tools/ci/ci.sh` mirrors `.github/workflows/ci.yml` and has to be kept
in step with it; the image names the compiler again in `tools/ci/Dockerfile`, so
bump it together with `rust-toolchain.toml`.

Remote CI (`.github/workflows/ci.yml`) runs fmt, clippy with `--all-features` and
again without default features, and the suite on Linux x86_64 and ARM64 (musl),
Windows x86_64 and ARM64 (the one on schannel) and macOS, then builds the five
release targets and a `.deb`. Clippy runs once more on Windows and macOS, since the
Linux job never compiles their `cfg` code. `supply-chain.yml` runs `cargo deny`
(advisories, sources, licenses; `deny.toml`) on a weekly timer and on pushes.

**The browser recorder is `node tools/capture/record.js`** (needs `playwright`): it
launches Chrome with a throwaway profile, the person browses by hand, and every
API call, click, console line and socket frame lands redacted in
`tools/capture/out/<timestamp>/events.jsonl`. `summarize.js` reads it back and
`sanitize.js` cleans an older recording. It is how a change in the web app is
studied before snob follows it.

Releases are tag-driven (`.github/workflows/release.yml`) and reproducible: the
tag must be on a commit that reached `main` and agree with the manifest version;
the notes are generated from the commits since the last tag. `rust-toolchain.toml` pins the
compiler, `/Brepro` is in both Windows tables of `.cargo/config.toml`, and the
archives get a deterministic mtime. Two builds of one tag produce the same bytes;
keep it that way. Packaging lives under `packaging/` (`render.sh` writes the
Homebrew formula in `Formula/`, the Scoop manifest in `bucket/` and the winget
manifests from a release's `SHA256SUMS`; `install.sh` and `install.ps1`), and the
`.deb` is described in `crates/snob-cli/Cargo.toml` and packages the README.

## The website

`web/` is the project's site, `snob.dennisgr7.dev`: Astro 7, fully static,
served by Cloudflare as a Worker with static assets and no Worker code
(`web/wrangler.jsonc`). Workers Builds is connected to the repository with `web/`
as its root: a push to `main` deploys, any other branch uploads a version with a
preview URL. It shares nothing with the Rust workspace but the repository and the
rules above (English, US spelling; `language.rs` reads its `.md`, `.json` and
`.yaml` too, and skips `pnpm-lock.yaml` by name as a generated file).

- **The look is decided in `web/DESIGN.md`**: who the page is for, the mark, the
  tokens, the sections and the limits on scripts. Read it before changing anything
  visual; `/design-system` on the dev server shows every token and component.
- **The Workers Builds connection** (Workers & Pages, Create, Continue with
  GitHub, access to this repository only): Worker name `snob-web` (it must match
  `wrangler.jsonc`), root directory `web`, production branch `main`, build
  command `pnpm build`, deploy command `pnpm exec wrangler deploy`, non-production
  branches `pnpm exec wrangler versions upload`. No build variables: Node comes
  from `web/.node-version`, and the image's older pnpm switches itself to the one
  `packageManager` names. The custom domain is created by the first deploy, not
  by hand in DNS; the `dennisgr7.dev` zone has to be in the same Cloudflare
  account as the Worker.
- **pnpm only**, run from `web/` (`pnpm install`, `pnpm dev`, `pnpm check`,
  `pnpm build`; `pnpm preview` serves `dist/` through Wrangler, with the
  production headers and 404). Wrangler is a pinned dev dependency, so a deploy
  never runs whatever version `npx` finds that day. The supply-chain policy is `web/pnpm-workspace.yaml`: a 24-hour
  quarantine on fresh versions, no exotic transitive sources, install scripts
  blocked except the ones listed in `allowBuilds`, exact versions. A new
  dependency has to argue for itself, as in the workspace. Node is pinned in
  `web/.node-version`.
- **`pnpm check` with 0 errors, 0 warnings, 0 hints, a clean `pnpm build` and a
  green `pnpm test`** before every commit that touches `web/`. The tests are
  Playwright's, against the build served by Wrangler as production serves it
  (headers, 404 status); they need a build first and Chromium, installed on
  purpose with `pnpm exec playwright install chromium`, never by a postinstall.
  `.github/workflows/web.yml` runs the same steps on every change under `web/`.
- **One source for the site's identity**, `web/src/lib/site.ts`: the domain
  (`site`), the name and the description. The canonical URL, the sitemap, Open
  Graph, the JSON-LD and the generated `robots.txt` and `llms.txt` all read it.
  The domain must be the root of a host; under a path, crawlers never find
  `robots.txt` or `llms.txt`.
- **The Content Security Policy is the `<meta>` Astro writes** from `security.csp`
  in `web/astro.config.mjs`, with the hash of every script and style each page
  runs. No `'unsafe-inline'`, no third-party origins, no inline `style`
  attributes, and Shiki stays off (it styles inline). `web/public/_headers` adds
  what a `<meta>` cannot carry (`frame-ancestors`, HSTS, `nosniff`, the referrer
  and permissions policies, the `llms.txt` link) and keeps version URLs out of
  search. A response with both policies must satisfy both.
- **Every page goes through `BaseLayout`**: title, description, canonical, robots,
  Open Graph, Twitter card and the JSON-LD `@graph` (`web/src/lib/jsonld.ts`,
  `<` escaped). A new page also gets a line in `llms.txt`; a `noindex` page is
  kept out of the sitemap in `astro.config.mjs`.
- No client JavaScript unless a feature cannot work without it. Tailwind v4 runs
  as a Vite plugin with its entry in `web/src/styles/global.css`; there is no
  `tailwind.config.js`. TypeScript strict, `@/` is `web/src/`.

## Architecture

Four crates, and the line between the first two is the one worth knowing:

| Crate | Responsibility |
|---|---|
| `snob-core` | Domain: models (`Pk`, `Epoch`, `User`), sets, filters, the diff, the schedule, the webhook signature, the request-budget **interface** (`budget`), the session and secret types, the clock. **No I/O** |
| `snob-store` | Everything kept on the machine: the SQLite databases and migrations, the platform directories (`paths`), the keyring (`secrets`), the account registry, `layout` (migration from the single-account layout), `watch.toml` (`config`) |
| `snob-ig` | Instagram's web API: the allowlist, the call builders, the page values, the client, pagination, pacing, error classification, the browser seam (`client::page`) |
| `snob-cli` | The `snob` binary, plus a library so commands can be tested: `cli`, `app`, `engine`, `commands`, `output`, `report`, `ui`, `headless`, `cdp`, `owner`, `pipe` |

**`snob-ig` depends on `snob-core` and on nothing under it**: the Instagram client
compiles no SQLite, no keyring and no TOML parser, and no browser code.

The tool is a session, a database and a request budget, and a browser the requests
leave from. Everything else is a way of asking those something:

```
                         app::App
             the only place they are assembled:
        client · store · progress · cancellation · viewer
                            │
  engine::  target · freshness · cooldown · walk · people · check · watch
                            │
                       commands::*
              orchestration and presentation only
                            │
              output::* · report::* · exit::*
```

Two rules keep it that way:

- **`engine` returns data and where the data came from. It never decides how
  anything looks.** Wording, formats and exit codes live in `commands`, `report`,
  `output` and `exit`; the one exception is the code an outcome exits with, which
  the outcome names (`ListOutcome::exit_code`, `check::Verdict::exit_code`). Every
  list, crossing and summary comes out of `engine::list`.
- **`commands` never builds a client or opens a database. It takes an `App`.** A
  command that assembles its own dependencies can be handed a different budget
  than the rest, which is how rate control gets bypassed by accident. The listed
  exceptions each say why at the line that does it: `whoami` builds an `IgClient`
  directly (it reports on a session that may be dead) but still takes its budget
  through `app::pacer_saying_a_line`; the monitor's run, scheduled loop and
  `status` call `Store::open_existing` directly (a connection is not held across
  a day-long sleep); `purge --account` and the scheduled loop open `shared.db`.

**Everything that acts as an account goes through `account::resolve`, and no query
opens another account's database.** An account's database and session file are
reachable only from its `AccountPaths` (`AppPaths::account(pk)`), and its session
only from its `SessionStore` (`SecretStore::session_of`), so a function that acts
as an account says which in its signature. `App::open` refuses a session whose
`ds_user_id` is not the account's. The places that look past the account in use are
few and each says why: the layout migration (`snob_store::layout::settle`), `login`,
`purge`, `logout --all`, `account list`, the account picker of the full-screen
views (`ui::accounts`), the monitor's run over every viewer
(`commands::watch::run::run_viewers`) and `watch status`, and the browser's
write-back and listener, which write to the account whose browser it was.

### Storage

SQLite in the platform's **local** data directory (WAL on a synced directory is a
documented corruption cause), one database per account at `accounts/<pk>/snob.db`:
its lists, walks, monitor history, request budget and cooldowns. Migrations are
`crates/snob-store/src/store/sql/001..012`, and a released migration is left as it
shipped. `accounts.toml` beside them lists the accounts and the active one and is
written only through `Registry::update` (under a lock). `shared.db` holds the
push-backs every account received, which each account's budget reads to stop with
the others (`budget::common_brake`), and the monitor's interval seed. Every table
is `STRICT`; comparison code reads the `usable_snapshots` view, which cannot return
an incomplete capture. A walk in progress holds a soft lease
(`snapshots.claimed_by`/`claimed_at`); the invariants are at `store/snapshots.rs`.
Captures are kept 30 days. `watch.toml` is in the configuration directory (the
roaming one on Windows), schema 2, one for every account, each `[[account]]` naming
its `viewer`; **no secret is in it**: the webhook token and signing key go to the
keyring. The data directory is private (0700, or a protected DACL on Windows,
`paths::create_private_dir`). An account is named by its id everywhere snob files
it, never by its username.

## The web-client engine

From the browser, every read and both writes go out the way instagram.com's web app
sends them. The command asks an *intent*, and the tab builds the call from what the
page shows. `snob-ig` defines what a page is asked (`client::page`, `web::Ask`) and
compiles no browser code; `snob-cli` (`headless/`) is the one implementation.

A read travels like this:

```
command ── IgClient::ask_page: allowlist::refused_ask, then paid by the Ask's kind
        ── client::page Allowlisted: refused_ask again
        ── owner/ (ToOwner::Ask): one process per user, a browser per account
        ── Headless::ask_as: refused_ask once more, in the process that sends
        ── Live::build → web::build_call, from the current document's page
           values and its app's calls, waited for up to APP_CALLS_PATIENCE (8 s)
        ── allowlist::refused on the built request (headless::refuse)
        ── headless::guard: every API call the tab sends is paused and judged
        ── fetch() from the tab's isolated world
```

The page's tokens never leave the process holding the browser, and the answer comes
back decoded by `model::web` into the same types the REST route fills.

### Intents (`web::Ask`)

`Viewer` (who the document was served to, nothing sent), `Tray` (the stories tray
the home document preloads, nothing sent), `Pk { name }` (a name to a pk, through
the route definitions or the profile document), `Document { path }` (load the home
page or a profile, answered with its bundles and never its HTML), `Query` (a Relay
read of the registry's), `Rest` (a friendship list of the registry's),
`Navigation { route }` (the app's router moving to a profile or its mutual tab;
the tab itself does not move), `Statuses { pks }` (`friendships/show_many` about
a list page's accounts), `Asset` (a file from the CDN, which the tab fetches)
and `Write` (one of the two mutations, built only on the profile document it is
made from).
What the tab answers from its document costs nothing, though a cooldown and Ctrl+C
still stop it. A write is two asks, `Document` and then `Write`, paid from the
write budget; the budget's wait comes before the document is loaded, never between
the two.

### Page values (`page_values.rs`)

A logged-in document defines modules, `["Name",[],{...},id]`, inside the JSON it
bootstraps from. snob reads six, from the first definition of each: `PolarisViewer`
(pk, username, `fbid`; `{data: null, id: null}` is the logged-out marker),
`SiteData` (revision, `hsi`, haste session, `__spin_*`; a document defines it twice
and the app sends the first), `LSD`, `DTSGInitialData` (the per-load Relay token),
`DTSGInitData` (never used to build a call) and `WebBloksVersioningID`. The web
session id (`X-Web-Session-ID` / `__s`), the Relay bitmaps (`__dyn`, `__csr` ...),
the route envelope, the variables of the app's queries and the `__req` counter are
copied from the app's *own* calls on the document (`web::AppCalls`, kept per
document by `headless::listen::Documents`). **A value is read from the page or
copied from the app, never invented.** One not shown yet is `web::Missing` and the
call is not built: nothing is sent, no cooldown is written, the command ends with
exit 1 ("the page has not shown ... yet") and the browser stays open. The values
change with every load, so they are read again on each document; a call is built
only from the current document's.

### Request families (`web.rs`)

The JS module that sends a call picks the header family, not the URL:

- **REST GET** `/api/v1/...`: the app's identifiers, the CSRF token, the
  `X-IG-WWW-Claim` (0 until the server hands one out) and the web session id.
  The one REST POST, `friendships/show_many`, adds `X-Instagram-AJAX` and a
  form of the pks, `jazoest` and the REST token, which is not the Relay one and
  is taken only from the app's own REST POSTs (`AppCalls::rest_token_sent`);
  until the app has sent one, `show_many` is not built.
- **Relay POST** `/api/graphql`: a form of about 29 fields with the page's values,
  `X-FB-Friendly-Name` and `lsd`, its last five fields in the app's order
  (`fb_api_caller_class`, `fb_api_req_friendly_name`, `server_timestamps`,
  `variables`, `doc_id`); never the claim, `X-Requested-With` or the web
  session id.
- **Relay POST** `/graphql/query`: the same plus the answer's root field and the
  Bloks version. An operation goes to one endpoint, never both.
- **Comet** `POST /ajax/bulk-route-definitions/`, for a name's route, and
  `POST /ajax/navigation/`, the app's router opening a profile or its mutual
  tab before a list is read; both in the envelope copied from the app's own
  route call.

Only the headers the app's script sets are listed; the browser adds the rest
(`Cookie`, `User-Agent`, client hints, `Origin`, `Sec-Fetch-*`). The page fills in
`X-CSRFToken` and the claim only on a request that already carries them. The
endpoint, `doc_id` and root field come from the registry, never the caller.

### The allowlist (`allowlist.rs`)

The registry is `Operation` (`ProfilePage`, `HoverCard`, `HighlightsTray`,
`HighlightsPage`, `ReelGallery`, `ReelGalleryPage`, `StoriesTray`, `NoteBubble`,
`SchoolPartnerBadge`, `ProfilePosts`, `ProfilePostsPage`, `Follow`, `Unfollow`,
each with a friendly name, `doc_id`, endpoint and root field), `Rest` (followers,
following, mutual followers of a pk, a post's info and its comments by the post's
pk, each with its allowed query keys), the route definitions, the app's navigation
and `show_many`. `refused` lets out only: a POST of the registry's on its own
endpoint, announcing its name in `X-FB-Friendly-Name` and carrying its own
`doc_id` (for the two writes, whose `doc_id` rotates, only the name and endpoint);
the route definitions; a navigation POST whose one `route_url` is `/`, a profile
or `/<name>/followers/mutualOnly` (`list_route`); a `show_many` whose form is the
pks (one to fifty), `jazoest` and `fb_dtsg` and nothing else; a GET of a registry
REST read by a numeric pk (`media/<pk>/info/` and `media/<pk>/comments/` and
nothing else under a post: not its likers, not the link the app mints to share
it); and a navigation with no query to
`/` or to a profile, never to the app's screens (`stories explore direct reels
accounts`), and never to a post's or a reel's page. `refused_ask` asks the same of an intent. **Anything else is refused
before the browser sees it, as a defect of snob's, and ends the command.** The
allowlist exists because the app's own writes cannot be listed: it sends seen
signals, view counts and history on its own, under names that change.

**The tab's guard** (`headless/guard.rs`) covers the app's own traffic: the app runs
on every document the tab loads and sends calls of its own. The browser pauses each
API call (`/api/graphql`, `/graphql/query`, `/ajax/`, `/api/v1/`), the view-count
path, the event log and the Facebook sync, and judges it: a GET goes on, a POST only
when `allowlist::refused_app` lets it (the registry's calls, plus the reads of the
app's that `APP_READS` names with the reasons, a profile's posts and suggested
accounts among them, the posts under any number the call vouches for since it
rotates, plus the app's empty POST that
hands out the claim, `CLAIM_HANDED_OUT`), anything else, the beacons included, is
failed as a content blocker fails it and logged to `browser-owner.log`. No video reaches the
page (a play counts). The app's realtime socket is not covered.
`CLAIM_HANDED_OUT` is matched exactly and its `/web/` prefix is unconfirmed; if it
is wrong every read carries claim `0`.

### Pacing

All numbers are in `pace.rs` and `rate_budget.rs`; the reasons are next to them.

- **Per page**: twelve accounts (`ACCOUNTS_PER_PAGE`, the app's `count=12`, which
  makes a list take two to four times the requests fifty a page would), one and a
  half to four seconds before a list's first page (`DWELL_MS`, a person looking
  at the profile), one to
  three seconds between pages of an action (`STEP_MS`), **a sitting of forty pages
  (about 480 accounts) and then a rest of five to fifteen minutes**
  (`PAGES_PER_SITTING`, `SITTING_PAUSE_MS`), counted across walks. Somebody else's
  lists use the same cadence with one fewer network retry.
- **The daily ceiling on accounts read** is 2,000 in any 24 hours, 1,000 for seven
  days after a push-back (`budget::accounts_per_day`). It is what binds, except
  under `--same-day`, which reads past it and leaves the request budget as the
  bound.
- **The request budget** (GCRA, persisted per account): a pace bucket of one request
  per 3.83 s with a twenty-request tolerance, a daily bucket of about 2,000
  requests, and the write bucket of one write per fifteen minutes with a tolerance
  of two (three in a row).
- **Cooldowns** (`snob_core::budget`): 2 hours for a rate limit, 12 for an action
  block, 30 minutes for a challenge, escalating on repeats and capped at 24 hours.
  Push-backs on two different accounts less than an hour apart pause every account
  (`common_brake`).
- **What goes with them, each a read paid like any other**: a profile is opened
  with the three reads the app sends beside its query (`IgClient::profile_burst`:
  the highlights tray, kept for the command that shows it, the note bubble, the
  school badge), a list with the app's navigation (`IgClient::open_list`), and
  each list page is followed by its `show_many` before the next page
  (`IgClient::statuses`), when the REST token is known. A failure of one of
  these that is not a push-back is logged and passed over.
- `Pacer::begin_action` advances the action count only where a stretch of time
  begins that the account can move in (a walk, each sitting, a monitor tick); a
  resolved target's counters are reused only within that action.
- Not evidence, and not to be cited as it: Meta's "200 calls per user per hour" is
  the Graph API's limit and has nothing to do with these endpoints.

### Auth

The credential is a session: `sessionid` (and `ds_user_id`, `csrftoken`, `mid`,
`ig_did`, `datr`) as `snob_core::session::Session`, whose secrets are
`secret::Secret` (cannot be printed, cleared on drop). It is stored per account in
the system keyring (`session.<pk>`) or, with no keyring or `--no-keyring`, a file in
the account's directory (DPAPI-sealed on Windows, 0600 elsewhere).

- `login --browser` launches a browser against *our own profile*; the user logs in
  and the cookies come back through `Storage.getCookies` over the pipe. `login
  --paste` writes a pasted `sessionid` into that profile (`headless::profile`). A
  login is authoritative; afterwards the browser's jar is, and the stored copy is
  brought up to date from it at the end of each run and between monitor runs
  (`headless::write_back`), never over a login made since and never another
  account's.
- **One browser profile per account**, `browser-profile/<pk>`, and two accounts
  never share one (`AppPaths::browser_profile_for`). The profile is the device.
- **The session is checked from the page**: `PolarisViewer` on the tab's document,
  no request. A document that is logged out is an expired session (exit 3); a write
  made on a page served to nobody or to another account is not sent.
- The protocol travels on **an inherited pipe, never a port** (`pipe::spawn`,
  `--remote-debugging-pipe`); the browser dies with its parent (a job object on
  Windows). The protocol client is our own (`cdp/`).
- The browser is a headless Chromium corrected only where headlessness changes what
  a page can see (`headless/mod.rs` lists each: `navigator.webdriver`, the
  `HeadlessChrome` token, client hints, the screen, pointer media queries, focus,
  isolated world for snob's fetches). The browser's own User-Agent always goes out;
  `login --user-agent` applies only with `SNOB_NO_BROWSER` and to media downloads.
- A push-back heard on the page, on the app's own calls or snob's, stops the run and
  is recorded once, by the process holding the browser, in that account's database
  (`headless::listen`: `Latch`, `heard`). The tab is then sent to `about:blank`.
- The owner (`owner/`) is one process per user; commands reach it over a `0600`
  socket (a user-only named pipe on Windows) and can send only snob's own
  intents and requests, never the protocol. Browsers close after five idle
  minutes. `SNOB_NO_OWNER` or an unreachable owner means `headless::alone`: the
  command runs its browser itself, and waits (`start_when_free`) while another
  browser holds the profile.
- `SNOB_NO_BROWSER=1` keeps `reqwest` as the last hop with the REST reads and the
  write token scraping, and with every CDN download; from the browser the tab
  fetches the files (`Ask::Asset`), except video and anything past 4 MiB, which stay
  with `reqwest`. TLS is rustls with the
  hybrid post-quantum group everywhere except Windows ARM64, which uses schannel
  (`crates/snob-ig/Cargo.toml`); `--strict-roots` is refused there.

## Rules the code enforces, and where

Each lives in the one place that cannot be bypassed, rather than in something a
caller has to remember. The reasoning is in the doc-comment at each location.

| Rule | Where it lives |
|---|---|
| Every request is paid for, once per redirect hop; nothing is sent in a cooldown | `Pacer::clear_to_send` inside `IgClient::get_body`, and `IgClient::ask_page` by the Ask's kind; `Pacer::clear` |
| Walking without rate control cannot be written | `ListWalker::new` takes only an `IgClient`, which cannot exist without a `Pacer` |
| Only the registry's calls leave, and the app's POSTs are judged too | `allowlist::refused`, `refused_ask`; `headless::guard`; `headless::refuse`, `web::build_call` |
| Nothing marks a story as seen | no `Mutation` variant; the allowlist; the guard; `snob-core/tests/no_seen.rs` |
| A write's shape: not replayed by a redirect, no CSRF token no send, not resent after an ambiguous answer | `redirect::Policy::none()`; `IgError::NoCsrfToken`; `client::write::worth_rediscovering` |
| A 429 or push-back puts the account in cooldown; a text 5xx from the edge proxy is not one | `IgClient::classify_and_record`; `headless::listen::heard`; `error::push_back` |
| A push-back on two accounts within the hour pauses all | `budget::common_brake` over `shared.db` |
| Only Instagram's CDN is downloaded from | `IgClient::check_downloadable`; `graphql::is_bundle` |
| A name is filtered before anything draws it; a name in a URL is encoded, never filtered | `model::printable`, `report::filtered`; `model::in_a_path` |
| An account id, a moment and a count cannot be confused | `snob_core::{Pk, Epoch, EpochMs}` newtypes; `Pk` implements neither `ToSql` nor `FromSql` |
| The credential cannot be printed and clears itself | `secret::Secret` |
| One account's database and session are reached only through its paths | `AccountPaths`, `SecretStore::session_of`; `App::open` checks `ds_user_id` |
| Only a typed `--account` narrows a purge | no `env` on the flag in `cli.rs` |
| Consent before enumerating someone else, before resolving; an unattended run reads a stranger's lists only on a recorded answer | `engine::ask_consent_with`; `Watched::may_run_unattended` |
| Two stored lists are crossed only if nothing happened between the walks; an incomplete list is never crossed against (more than one row in twenty repeated is incomplete) | `engine::cooldown::check_same_moment`; `pager::WalkState::verify_completion`, `REPEAT_LIMIT_PERCENT` |
| A walk in progress has exactly one writer | `snapshots::resumable`/`save_page`/`close` |
| A change is reported once and only from a verified list; a report is never lost because delivery failed | `store::watch::Mark`; `store::watch::commit_report` (queue row and mark in one transaction) |
| A queued report goes only to the address it was made for; credentials configured for one origin are not sent to another | `watch_deliveries.destination`; `delivery::plan` |
| The session cannot reach the user's webhook | `WebhookClient::new` takes no `Session` |
| The schedule reads no clock | `schedule::next_after` takes `now` |
| A file named by a server is created, never written over | `output::create_new` |
| The data directory is limited to this user; no deletion near the root | `paths::create_private_dir`; `paths::is_safe_to_remove`, `AppPaths::owned_dirs` |
| No request is sent after the user asks to stop; a write in flight is the one thing Ctrl+C does not abandon | `Pacer::clear_to_send` reads the token first; `IgClient::post`, `ask_page` for `Ask::Write` |
| The reader leaving is not an error | `ui::say!` (`println!` panics on a closed pipe) |
| A failure is told in the language the answer was going to be in | `report::Wording`, `main::wording_for` |

## Settled, so nobody re-opens them

- **`friendships/show_many` is sent after every list page**, as the app sends it:
  it is a read that happens to be a POST, a write is a `graphql::Mutation` and nothing else, and a list
  read without it is not the app's. It is paid from the read budgets, never the
  write bucket, and goes only with the REST token the app's own REST POSTs
  carried; its answer is read for a push-back and discarded.
- **The stories gallery is opened over the tray the home document preloads**
  (`Ask::Tray`), not over the app's tray query, which the app does not send on the
  home page. An account not in it opens the gallery over itself alone.
- **A call is built in the process holding the browser**, right before it is sent,
  from a typed intent: one `__req` counter per tab, and the page's tokens never
  leave that process.
- **The arithmetic holds**: `unfollowers + friends` is everyone you follow, `fans +
  friends` everyone who follows you. A test asserts it.
- **An incomplete list is never crossed against.** A short *starting* list only
  warrants a warning.
- **The full-size profile picture** comes from the profile query's
  `hd_profile_pic_url_info` (no size beside it); `web_profile_info`'s
  `profile_pic_url_hd` is not the full size.
- **There is no useful logged-out mode.**
- **No biometric verification**, on any platform: any prompt a local process of the
  same user can trigger, that process can satisfy.
- **A challenge's cooldown is not lifted by clearing the challenge.**
- **The browser is talked to over a pipe; there is no debugging port.** A port hands
  the session cookie to any local process.
- **`snob purge` deletes the stored data and not the binary**, session first.
  `snob logout` deletes the account's browser profile and session, keeps its data
  and its registry entry.
- **The trust store is narrowed only when asked** (`--strict-roots`,
  `--tls-extra-root`); certificate pinning stays rejected.
- **The Windows credential is `CRED_PERSIST_LOCAL_MACHINE`** (`secrets.rs::entry_for`).
- **There is no lease over `watch_marks`**: two overlapping runs can report one
  arrival twice, which costs a duplicate and never loses a window.
- **Highlights are a command of their own**, not a flag on `stories`.
- **A post's or a reel's page is never loaded.** A link's code is the post's pk
  (`snob_ig::shortcode`), and the post is its info, asked from the page it opens
  on, as the app asks it; a reel's link loaded cold lands on the app's reels
  screen, which reports what it plays. Nothing of a link's query (the share
  token among it) is read or sent.
- **Posts are a grid and a post, as the app shows them**: `posts` is
  `highlights`' shape (a listing, and a number to look inside one), `post` one
  post by its link. A grid page is a read and not a list of accounts; a page of
  comments is, and is charged to the day's accounts. Videos are the progressive
  file the app plays, 720 wide at most; the DASH tracks are not joined.
- **Stories are handed to the system viewer**, not drawn in the terminal.
- **The site's own page is loaded, and not charged.** The app boots in the tab and
  makes requests of its own, which do not go through the `Pacer`; the counts snob
  reports are its own calls. That traffic is kept on purpose, since the calls
  without it are what a script looks like, but it is listened to for push-back and
  judged by the guard.
- **Everything is per user, never per directory.**
- **`snob import dyi` reads, crosses and prints, and nothing more**: nothing stored,
  nothing crossed live.

## Known walls

- A list of tens of thousands may come back truncated (a short page with no
  cursor); the walker catches it and the set commands refuse.
- The REST reads (`/api/v1/users/{pk}/info/`, `web_profile_info`) draw a 429
  from the headless browser. The app's own web client makes neither call, which
  is why the browser route sends the app's calls instead.
- WebGL is absent in the headless browser. Whether a headless browser on a Windows
  desktop reaches the real GPU has not been measured.
- Two browsers cannot share a profile (see the owner and `start_when_free`).
- A browser costs memory (roughly 425 MB while it runs on a Raspberry Pi 5) and
  about 90 MB of profile per account.

## Planned

- **The retirements, in this order.** Each lands in the commit after its
  replacement:
  1. The REST read path goes (session validation, `web_profile_info`, `topsearch`,
     the REST highlights and stories, the write token scraping); it stays only
     behind `SNOB_NO_BROWSER`.
  2. The `reqwest` transport to Instagram goes, and its tests move to a fake `Page`.
  3. The migrations from older versions (single-account layout, `watch.toml`
     schema 1, the old browser profile, the `watch_state` interval seed) go one
     release later, which then refuses an older layout.
- The path of `CLAIM_HANDED_OUT`, the capture of what the listener counts as a
  push-back, the owner outliving the terminal on a desktop, and whether Ubuntu's
  snap Chromium can open a profile under a hidden directory are unchecked.
- Two endpoints worth a command someday, both GETs about the viewer's own account:
  `friendships/pending/` and `archive/reel/day_shells/`.
- A list the profile card opens changes accounts only from the card
  (`ui::people::browse_in`).

## Costs worth knowing

Measured and recorded so nobody re-measures them. The binary is about 5 MB on
Windows ARM64 (bundled SQLite roughly 9%, the `xlsx` feature roughly 9% more, an
opt-out). The release profile is `opt-level = "s"`, fat LTO, `panic = "abort"`,
stripped; nothing on a walk is CPU-bound against minutes of deliberate pacing.
Running the browser took `snob whoami` from 0.1 s to about 2.7 s on a Pi 5 against a
local fake. `clap` without `color` and `zstd` out of `Accept-Encoding` would save
bytes and are kept anyway as positions: help in color, and an `Accept-Encoding` that
is Chrome's character for character.

## Notes for the working tree

`/docs/` is gitignored scratch (plans, hand-off notes, review output) and is not
read by any guard or test; do not rely on it for anything that has to survive, and
do not commit it. The same goes for `tools/capture/out/`.
