use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "snob",
    // Without this, the help on Windows announces "snob.exe", which is not how
    // the command is typed.
    bin_name = "snob",
    version,
    about = "Instagram from the terminal",
    long_about = "Instagram from the terminal.\n\n\
                  Walks your followers and your following, crosses them, and answers who \
                  does not follow you back, who you never followed back, and who you and \
                  somebody else both know. It also shows and downloads the stories an \
                  account has up and the highlights it keeps.\n\n\
                  It changes exactly two things and asks first about both: \"follow\" and \
                  \"unfollow\", one account at a time. It never blocks, never removes a \
                  follower, and never tells anybody you looked at their story.",
    after_help = EXAMPLES
)]
pub struct Cli {
    /// The account to act as, by username or id
    ///
    /// Without it, SNOB_ACCOUNT names one; then the active account, which the
    /// last "snob login" leaves; then the only one signed in.
    // Not `env = "SNOB_ACCOUNT"`: `main` reads the variable for every command
    // but one, so a purge is narrowed to one account only by a flag typed for
    // it.
    #[arg(long, global = true, display_order = 899, value_name = "USER")]
    pub account: Option<String>,

    /// Stop rather than wait longer than this for the request budget
    ///
    /// A list stopped this way is kept and continues where it left off, a
    /// follow is not sent, and the run exits 130 with the wait in the message.
    /// The pace's own steps and the rest after a long stretch of pages are
    /// never cut short.
    // For callers that cannot sit for hours: a script with a deadline, an
    // agent's tool call (`snob mcp` passes it to every command it runs).
    #[arg(
        long,
        global = true,
        display_order = 898,
        value_name = "DURATION",
        value_parser = duration
    )]
    pub max_wait: Option<std::time::Duration>,

    /// Stop, as Ctrl+C stops, when standard input reaches its end
    // Hidden: plumbing for a parent that runs snob and cannot send it a
    // signal, which on Windows is every parent (`snob mcp`, a script's
    // timeout). `interrupt::stop_when_stdin_closes` says the rest.
    #[arg(long, global = true, hide = true)]
    pub stop_when_stdin_closes: bool,

    /// Refuse to send anything for the account while another run given this
    /// flag does
    // Hidden: plumbing for `snob mcp`, which gives it to every child that
    // may reach Instagram, so an agent's parallel calls cannot walk one
    // account from two processes at once. Not for people in this release:
    // a command typed by hand does not take the lock, and nothing that is
    // not an agent's should be refused as busy (`app::spend_alone`).
    #[arg(long, global = true, hide = true)]
    pub spend_alone: bool,

    /// Say how a walk goes as one JSON line on standard error per page and
    /// per wait, instead of drawing a bar
    // Hidden: plumbing for a job's supervisor (`jobs/supervise.rs`), which
    // reads the walk's progress back from its log (`jobs::show`). Wins over
    // `--no-progress`, which the walk's command line may carry
    // (`progress::progress_json`).
    #[arg(long, global = true, hide = true)]
    pub progress_json: bool,

    /// Store the session in a protected file instead of the system keyring.
    /// For environments without a desktop session.
    // Ordered last, with `verbose`. Both are global, so without this they are
    // propagated into every subcommand and land in the middle of its own
    // options, splitting a list that reads in a deliberate order.
    //
    // **Left out of `-h`** (`hide_short_help`), as `verbose` and the two TLS
    // flags are: they are in every command's help, and in `-h` they were a
    // third of each one's options, none of them about the command. `--help`
    // still lists them, and `-h` says it shows more.
    #[arg(long, global = true, display_order = 900, hide_short_help = true)]
    pub no_keyring: bool,

    /// Show diagnostic traces
    ///
    /// The browsers' own go to browser-owner.log in the data directory, at the
    /// level of the command that started the process that runs them.
    #[arg(long, global = true, display_order = 901, hide_short_help = true)]
    pub verbose: bool,

    /// Check Instagram's certificate against Mozilla's roots only
    ///
    /// Off by default: the roots installed on this computer are trusted,
    /// which is what makes snob work on a managed machine, and also what lets
    /// a TLS-inspecting root installed there read the session in transit.
    /// This trusts Mozilla's roots alone instead. Refused on Windows for
    /// ARM64, where it cannot be done, and never applied to the webhook.
    // Why each of those, in full: reqwest 0.13 made the platform verifier the
    // default, and the trade between a managed machine and a middlebox is
    // only the person running it can make. On Windows ARM64 the TLS backend
    // is schannel, which has no way to express "these roots and no others",
    // and a security flag that silently does nothing is worse than one that
    // is not offered. A private CA in front of somebody's own webhook
    // receiver is legitimate, and `snob_ig::http::plain` has no argument for
    // this -- the same shape that stops the session reaching a webhook. That
    // reasoning was the flag's help once, in every command's `--help`.
    #[arg(long, global = true, display_order = 904, hide_short_help = true)]
    pub strict_roots: bool,

    /// Also trust the certificates in this PEM file
    ///
    /// The way back out of --strict-roots, and it requires it: on the
    /// platform store there is nothing to add to, because whatever an
    /// administrator installed is already trusted. Repeatable.
    #[arg(
        long,
        global = true,
        display_order = 905,
        requires = "strict_roots",
        value_name = "PEM",
        hide_short_help = true
    )]
    pub tls_extra_root: Vec<PathBuf>,

    /// Keep every file this run reads or writes under this directory
    ///
    /// A testing build only. Replaces the discovered data and configuration
    /// directories, puts the session in a file inside it rather than in the
    /// system keyring, **and gives the run a keyring namespace of its own** —
    /// so a sandbox run cannot read, write or delete the real one. That is the
    /// property [`Cli::ig_base_url`] leans on.
    ///
    /// All three, and the third is not decoration. The store reaches the
    /// keyring whatever backend it is on, so with the real service name a
    /// sandbox `login` deleted the developer's session and a sandbox that had
    /// not logged in yet loaded the real cookie — which is the exact thing the
    /// pairing below exists to prevent. `main::wiring` is where the namespace
    /// is assigned and says the rest.
    #[cfg(feature = "testing")]
    #[arg(long, global = true, hide = true, display_order = 902)]
    pub sandbox_root: Option<std::path::PathBuf>,

    /// Ask this server instead of Instagram
    ///
    /// A testing build only, and it **requires `--sandbox-root`**. That is the
    /// whole safety argument, and it is enforced by clap rather than described:
    /// a redirected client can only ever carry a session out of a store inside
    /// the sandbox root, so the session belonging to the person running this is
    /// not reachable from a redirected run. Without that pairing the flag would
    /// be a way to send a real session cookie to somebody else's server.
    ///
    /// Nothing about it is loopback-only, deliberately. `IgClient::is_live`
    /// decides whether the pace is real by address, so a proxy on `127.0.0.1`
    /// forwarding to Instagram would be a test server by address and Instagram
    /// by content — a real account walked with no waits between pages. A
    /// loopback restriction would look like the safe option and be the
    /// dangerous one; an empty sandbox store is the thing that actually helps.
    #[cfg(feature = "testing")]
    #[arg(
        long,
        global = true,
        hide = true,
        display_order = 903,
        requires = "sandbox_root",
        value_name = "URL"
    )]
    pub ig_base_url: Option<url::Url>,

    /// Send the requests to `--ig-base-url` from the browser, as they are sent
    /// to Instagram. A testing build only.
    #[cfg(feature = "testing")]
    #[arg(
        long,
        global = true,
        hide = true,
        display_order = 904,
        requires = "ig_base_url"
    )]
    pub through_the_browser: bool,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Refuses `--account` beside `login --add` or `logout --all`, which are
    /// about no one account. Checked here rather than by clap, which does not
    /// see a global flag written before the command.
    pub fn refuse_a_named_account(&self) -> Result<(), clap::Error> {
        let flag = match &self.command {
            Command::Login(args) if args.add => "--add",
            Command::Logout(args) if args.all => "--all",
            _ => return Ok(()),
        };
        if self.account.is_none() {
            return Ok(());
        }
        use clap::CommandFactory;
        Err(Self::command().error(
            clap::error::ErrorKind::ArgumentConflict,
            format!("the argument '{flag}' cannot be used with '--account <USER>'"),
        ))
    }
}

/// A shared secret long enough to be worth having.
///
/// Without a floor, `--sign-with a` would produce a well-formed signature that
/// anybody could reproduce. The reason that matters is already written down at
/// `delivery.rs`: handing a third party a body and its MAC gives them
/// everything they need to guess a human-chosen secret offline, at whatever
/// rate their hardware allows. HMAC-SHA256 accepts a key of any length, so
/// nothing below this would have complained.
///
/// Thirty-two characters is the shortest that is not a guessing target. It is
/// counted in characters rather than bytes because the person typing it is
/// counting characters.
///
/// **The one floor, wherever a key arrives.** `--sign-with` applies it as a
/// value parser and `watch setup` applies it to what was typed at the prompt,
/// so a key cannot pass one door and be refused at the other. Trimming is part
/// of the rule: the prompt's non-terminal path reads a whole line, newline
/// included, and a key that differs from itself by invisible whitespace is a
/// support case.
pub(crate) fn signing_secret(value: &str) -> Result<String, String> {
    const FLOOR: usize = 32;
    let value = value.trim();
    let length = value.chars().count();
    if length < FLOOR {
        // Named rather than captured: a `format!` cannot reach an identifier
        // through a `concat!`, and `concat!` is what keeps `cargo fmt` from
        // rejoining these lines and leaving the indentation inside the string.
        return Err(format!(
            concat!(
                "a signing secret has to be at least {FLOOR} characters and this one is ",
                "{length}. A short one can be guessed offline by anybody who has been ",
                "sent one signed report. Generate one instead -- ",
                "\"openssl rand -hex 32\", or ",
                "\"python -c \\\"import secrets; print(secrets.token_hex(32))\\\"\"."
            ),
            FLOOR = FLOOR,
            length = length
        ));
    }
    Ok(value.to_string())
}

/// An account named on the command line, refused by clap (exit 2) when it
/// cannot be a username (`snob_core::model::possible_username`), so the
/// mistake is told before the consent question and before anything is spent.
/// The value is kept as typed; the commands take the at sign off themselves.
pub(crate) fn username(value: &str) -> Result<String, String> {
    if snob_core::model::possible_username(value) {
        Ok(value.to_string())
    } else {
        Err(format!(
            "\"{}\" cannot be an Instagram username: letters, digits, '.' and '_', at most {}",
            snob_core::model::printable(value),
            snob_core::model::USERNAME_MAX
        ))
    }
}

/// The line of [`EXAMPLES`] that announces `SNOB_MCP_KEY`, in a build that
/// reads it (`commands::mcp::key`, feature `mcp-http`) and in no other: a
/// build without the HTTP half has no key to read.
#[cfg(feature = "mcp-http")]
macro_rules! mcp_key_variable {
    () => {
        "
  SNOB_MCP_KEY      the key of snob mcp served over the network, instead of
                    the keyring's; see \"snob mcp key --help\""
    };
}
#[cfg(not(feature = "mcp-http"))]
macro_rules! mcp_key_variable {
    () => {
        ""
    };
}

/// Shown under the option list.
///
/// The note about quoting is not decoration. On PowerShell `@` is the splatting
/// operator, so an unquoted `@name` is consumed by the shell and never reaches
/// this program, which then answers about the user's own account instead. It
/// cannot be detected from here, so the only place to say it is the help, and
/// none of the examples above it are written that way.
const EXAMPLES: &str = concat!(
    "\
Examples:
  snob login                          store your session, once
  snob status --budget                what you can still send today, for no request
  snob unfollowers                    who does not follow you back
  snob profile someone                their page: counts, bio, who you both know
  snob scan someone                   the full picture of another account
  snob pfp someone -o picture.jpg     their profile picture, at full size
  snob stories someone                browse what they have up; a listing in a pipe
  snob highlights someone             browse the highlights its profile keeps
  snob highlights someone 2 -d all    save everything in the second one
  snob posts someone                  browse the posts on their grid
  snob posts someone 3 -d all         save every photo and video of the third
  snob reel https://www.instagram.com/reel/AbCdEfGhIjK/ -d all
  snob unfollow someone               the one thing snob changes, after asking
  snob unfollowers --format csv -o unfollowers.csv

A username may be written with or without a leading @. If you write the @, quote
it (\"@someone\"): on PowerShell an unquoted one is eaten by the shell.

Exit codes:
  0   it worked; a list cut short by --limit or --max-pages is still a 0
  1   it failed, or a result was refused because a list came back incomplete
  2   the command line could not be parsed; nothing was done, and running it
      again unchanged will not help
  3   no session, or the stored one no longer works -- run \"snob login\"
  4   Instagram wants the account verified -- open the address it prints
  5   Instagram is throttling, or the account is in cooldown and nothing was
      sent -- wait; \"snob status\" says until when
  130 stopped by you: Ctrl+C, or a confirmation not given -- including with
      no terminal to ask at, where -y confirms in advance

Environment:
  NO_COLOR          no styling, whatever the terminal supports
  CLICOLOR_FORCE    styled messages on standard error even where it is not a
                    terminal; a result is styled only on a terminal
  FORCE_HYPERLINK   OSC 8 hyperlinks even where they were not detected
  SNOB_ACCOUNT      the account to act as when --account is not given
  SNOB_LOG          what --verbose shows, as target=level pairs
  SNOB_NO_BROWSER   send the requests directly instead of from a browser, for
                    a machine with none; Instagram can tell the difference
  SNOB_NO_OWNER     run this command's browser itself instead of sharing the
                    one background process, e.g. from a systemd timer
  SNOB_FFMPEG       the ffmpeg that joins a video saved at its best, an
                    absolute path, in place of the one on PATH
  SNOB_THEME        \"brand\" draws the accent in snob's own pink rather than
                    your terminal's magenta, where it shows 24-bit color
  SNOB_IMAGES       how a picture is drawn, when the guess from your terminal
                    is wrong: kitty, iterm, sixel, or blocks
  SNOB_CSRFTOKEN, SNOB_SIGNING_KEY
                    the two secrets a command line would otherwise carry;
                    see \"snob login --help\" and \"snob watch --help\"",
    mcp_key_variable!()
);

/// What `snob mcp --help` says under its options, in every build that has
/// the server; `mcp_listen_help` follows it where the HTTP half is built.
#[cfg(feature = "mcp")]
macro_rules! mcp_after_help {
    () => {
        "Started by the agent's client, not typed: \"snob mcp config <client>\" \
                      prints what to add to its configuration. Each tool call runs one snob \
                      command, so the budget, the cooldown and consent hold exactly as they do \
                      in a terminal. It offers the status, the lists and profiles; with \
                      --allow-writes, follow and unfollow too. Each of those, and walking \
                      another account's lists, runs only after a yes for that one call, \
                      asked in the client when it can show a form, or else put to you by the \
                      agent in the chat (--confirm-in-chat narrows what that may cover). \
                      snob cannot tell who gave the yes, so keep the writes on \"ask\" in the \
                      client's permissions. Never login, purge, the monitor or a \
                      download.\n\n\
                      A call waits at most --max-wait for the request budget (five minutes \
                      when it is not given) and is refused past it; a list stopped that way \
                      continues next time. Calls that may reach Instagram run one at a time; \
                      another is refused as busy, never queued. A long list is handed back \
                      a page at a time, up to 200 accounts; the later pages come from what \
                      was already read and send nothing.\n\n\
                      A list that has to be walked runs as a job (\"snob jobs\"): the call \
                      answers with the result when the walk ends within its wait_seconds, \
                      and otherwise with the job, which the agent asks about, lists and \
                      stops with snob_job, snob_jobs and snob_job_cancel, and which goes on \
                      after the call, the client and the server. A client that supports MCP \
                      Tasks gets the job as a task instead, which it polls and cancels on \
                      its own. A job's walk waits at most an hour for the request budget, or \
                      --max-wait when it is given.\n\n\
                      What runs on a yes is recorded in the account's database before its \
                      first request, and \"snob status --agents\" lists the last 24 hours. \
                      Agents may make --max-writes-per-day follows and unfollows on the \
                      account in any 24 hours (ten when not given, at most 99), attempts \
                      counted; past that the call is refused and nothing is sent. What you \
                      type in a terminal is neither recorded nor counted.\n\n\
                      After upgrading snob, restart the server (reconnect it in the client): \
                      one whose binary changed under it refuses every call and runs nothing."
    };
}

/// The paragraph of `snob mcp --help` about `--listen`, which exists only
/// with the `mcp-http` feature.
#[cfg(feature = "mcp-http")]
macro_rules! mcp_listen_help {
    () => {
        "--listen 127.0.0.1:8750 serves the same over HTTP at /mcp on this \
                      machine's loopback, for a tunnel of your own (ssh -L, cloudflared, a VPN) \
                      to reach from elsewhere; any other address is refused. On Linux and \
                      macOS, --listen socket serves on a socket in snob's data directory that \
                      only you can reach, which no other user can take while snob is not \
                      running (ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:8750:<its path>, \
                      or cloudflared's unix:<its path>); on a \
                      port, whoever listens there while snob does not is handed the key. Every \
                      request \
                      carries the key \"snob mcp key new\" makes, as Authorization: Bearer, \
                      and the server does not start without one. A yes in the chat covers \
                      reading lists there and not the writes, unless --confirm-in-chat all; \
                      whoever holds the key can still confirm both in a form it answers. \
                      --public-url names the https address the tunnel serves it at. TLS ends \
                      at the tunnel, whose provider sees the key and every answer; put an \
                      access layer in front where it offers one. \"snob mcp config <client> \
                      --remote <URL>\" prints what a client needs to reach it there; the \
                      client reads the key from SNOB_MCP_TOKEN."
    };
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Store your Instagram session: in the system keyring, or in a file
    /// where there is none
    Login(LoginArgs),

    /// Show which account you are authenticated as
    Whoami(WhoamiArgs),

    /// Where the account stands: session, budget, answers, cooldown, lists, monitor, agents, jobs
    #[command(
        after_help = "Read from what is stored here; nothing is sent to Instagram and nothing stored is \
                      changed, not even a database an older snob left, which is read as the \
                      current one without being migrated. With no section named, all of \
                      them; --budget, --cooldown, --session, --lists, --watch, --agents and \
                      --jobs narrow it to those. --agents is what agents were told yes to \
                      through \"snob mcp\" in the last 24 hours: each follow, unfollow and walk \
                      of another account's lists, how the yes was given and through which \
                      client. --jobs is the walks an agent left running as jobs (\"snob \
                      jobs\").\n\n\
                      It exits 5 while the account is in cooldown, whatever sections were \
                      asked for, so \"snob status --budget && snob scan\" runs the scan only \
                      when it may send; 3 with no session; 0 otherwise.\n\n\
                      The budget counts what goes out now without a wait, which is the \
                      smaller of two buckets every request pays: the pace allows twenty-one \
                      requests in a row, the day about two thousand. Writes go three in a \
                      row and then one every fifteen minutes. Lists read at most 2,000 \
                      accounts in any 24 hours, 1,000 for a week after a push-back. In \
                      --json a moment something may happen again is rounded up to its \
                      second."
    )]
    Status(StatusArgs),

    /// List the accounts signed in here, and pick the one commands act as
    #[command(
        subcommand,
        after_help = "A command acts as the account --account names, then the one SNOB_ACCOUNT \
                      names, then the active one, then the only one signed in. \"snob login\" \
                      makes the account it signs in the active one."
    )]
    Account(AccountCommand),

    /// Delete the stored session and browser profile of the account in use
    #[command(
        after_help = "This removes the account's stored session and its browser profile, \
                      which holds a live session; --all does it for every account. The \
                      account's data stays, and so does its place in \"snob account list\". \
                      \"snob purge\" clears everything else snob has stored on this computer: \
                      the databases, watch.toml."
    )]
    Logout(LogoutArgs),

    /// Delete everything snob has stored on this computer, before uninstalling
    #[command(
        after_help = "The sessions, the databases and the browser profiles. The binary itself \
                      is left alone: uninstall it with whatever installed it.\n\n\
                      With --account, only that account's session, data and browser profile, \
                      and its place in \"snob account list\". SNOB_ACCOUNT never narrows a \
                      purge: the account has to be typed."
    )]
    Purge(PurgeArgs),

    /// An account as its page shows it: counts, bio, who you both know, highlights
    #[command(
        after_help = "What you would see opening the profile: the counters, the bio, whether \
                      you follow each other, the accounts you follow that follow them, the \
                      highlights and whether anything is up right now. A few requests for the \
                      page, and up to ten pages of the accounts you follow that follow them, \
                      which count toward the account's sitting as a list's pages do, so it may \
                      wait out one rest between sittings, up to 15 minutes, before or between \
                      those pages (one an earlier walk left owed, or one its own pages draw). \
                      It walks neither of its lists and stores nothing.\n\n\
                      From the browser, finding another account's id costs one more until snob \
                      has walked its lists or a list that names it; your own is the session's. A \
                      name that cannot be an Instagram username is refused before any request. \
                      With SNOB_NO_BROWSER the name is asked about as typed, for no extra \
                      request.\n\n\
                      \"snob scan\" is the crossing, and costs both lists."
    )]
    Profile(ProfileArgs),

    /// Summary of the whole account: followers, following, and how they cross
    Scan(ScanArgs),

    /// Who joined, left or was renamed between two stored captures of a list
    #[command(
        after_help = "Reads what earlier walks stored and sends nothing to Instagram, \
                      so it answers in a cooldown too. Without --since, --from or --to, it \
                      compares the two newest whole captures of each list. A walk that \
                      stopped short is never one of the two: the accounts it missed would \
                      read as departures. \"snob snapshots\" lists the captures and their \
                      ids.\n\n\
                      Examples:\n  snob changes\n  snob changes --list followers --since 7d\n  \
                      snob changes --since 2026-11-01\n  snob changes --from 12 --to 31 \
                      --format csv -o changes.csv"
    )]
    Changes(ChangesArgs),

    /// The captures of an account's lists stored here, and how each walk ended
    #[command(
        after_help = "Reads only what is stored, and sends nothing to Instagram. One \
                      row per capture: its id (what \"snob changes --from\" takes), the \
                      list, when the walk ended (or began, for one still open), where it came \
                      from (a walk, or Instagram's own export through \"snob import dyi\"), \
                      how many accounts it holds and how many Instagram said the list had, \
                      and why it stopped."
    )]
    Snapshots(SnapshotsArgs),

    /// Accounts you follow that do not follow you back, or another account's
    Unfollowers(ListArgs),

    /// Accounts that follow you that you do not follow back, or another account's
    Fans(ListArgs),

    /// Accounts that you follow and that follow you back, or another account's
    // `mutuals` is the earlier name, kept as a hidden alias so anything written
    // against it still runs rather than failing at the shell.
    #[command(alias = "mutuals")]
    Friends(ListArgs),

    /// Your followers, or another account's
    Followers(ListArgs),

    /// The accounts you follow, or another account's
    Following(ListArgs),

    /// Download a profile picture in high resolution
    Pfp(PfpArgs),

    /// Show the stories an account has up, and download them
    #[command(
        after_help = "Listing and downloading a story does not tell the account you looked. \
                      snob has no way of doing that and a test keeps it that way."
    )]
    Stories(StoriesArgs),

    /// Show the highlights an account keeps on its profile, and download them
    #[command(
        after_help = "\"snob highlights someone\" numbers the tray; \"snob highlights someone 2\" \
                      lists what the second one holds, and takes -d, -o and -i exactly as \
                      \"stories\" does. Without the number, -d takes whole highlights: \
                      \"-d 2\" saves everything in the second, \"-d all\" the whole profile. \
                      Listing and downloading a highlight does not tell the account you \
                      looked. snob has no way of doing that and a test keeps it that way."
    )]
    Highlights(HighlightsArgs),

    /// Show the posts on an account's grid, and download them
    #[command(
        after_help = "\"snob posts someone\" numbers the grid, twelve posts a page; \"snob posts \
                      someone 3\" lists what the third holds, a photo or a video each, and takes \
                      -d, -o and -i as \"highlights\" does. Without the number, -d takes whole \
                      posts: \"-d 3\" saves everything in the third, \"-d all\" every post the \
                      listing read. --pages reads more than the first page; the view reads the \
                      next one as you scroll to it. Each page is one request, and opening a post \
                      in the view is two more: the post and its comments, as the app asks them. \
                      Videos are saved at their best when ffmpeg is installed, as \"post\" says. \
                      Listing and downloading a post does not tell the account you looked."
    )]
    Posts(PostsArgs),

    /// Show one post or reel by its link, and download it
    #[command(
        visible_alias = "reel",
        after_help = "The link is any post's or reel's address, as the app's \"Copy link\" gives \
                      it, or the code in it; only the code is read, and nothing else in the \
                      address is sent. Two requests read the post and its first page of \
                      comments, as the app opens one, from the page it opens on, which is never \
                      loaded; each further page of comments is one more. A video is saved at \
                      its best, usually 1080 wide, when ffmpeg is installed, which joins its \
                      picture and its sound into one MP4 without re-encoding (VP9, which \
                      QuickTime does not play); without it, or with --quality compat, as the \
                      app's 720-wide MP4. Listing and downloading a post does not tell the \
                      account you looked."
    )]
    Post(PostArgs),

    /// List the accounts that liked a post, as many as Instagram shows
    #[command(
        after_help = "The link is read as \"post\" reads it: only the code. Two requests, as \
                      the app makes them when a post opens and its \"others\" under the likes \
                      is clicked: the post, then its likers. Instagram shows about a hundred \
                      of them, the accounts you know first, and never the whole list; the \
                      count says how many there are, unless the owner hid it. They are other \
                      people's accounts, so snob asks first, as it asks before reading somebody \
                      else's followers; -y answers in advance. They count toward the accounts \
                      a day may read."
    )]
    Likers(LikersArgs),

    /// Download a picture or a video from Instagram's CDN by its address
    #[command(
        after_help = "For an address already known, such as one a listing's JSON gives: \
                      the file is fetched from the CDN with no session, no request to \
                      Instagram and nothing charged to any account's budget, and works with \
                      nobody signed in. Only addresses on cdninstagram.com and fbcdn.net are \
                      fetched. They are signed and expire after a while; a refused one has to \
                      be asked for again, with \"snob post\" or the listing it came from.\n\n\
                      The User-Agent sent is --user-agent, then the signed-in account's, then \
                      the installed browser's. With no -o the file goes to standard output \
                      down a pipe, and into the working directory under the address's own \
                      file name at a terminal."
    )]
    Fetch(FetchArgs),

    /// Follow an account
    #[command(
        after_help = "One account per run, on purpose: what Instagram acts on is a burst of \
                      follows rather than the day's total. The CSRF token is the browser's \
                      own, so any session can write; with SNOB_NO_BROWSER the session needs \
                      one, which \"snob login --browser\" captures or \"snob login --paste \
                      --csrftoken\" adds."
    )]
    Follow(FollowArgs),

    /// Unfollow an account
    #[command(
        after_help = "One account per run, on purpose: what Instagram acts on is a burst of \
                      unfollows rather than the day's total. The CSRF token is the browser's \
                      own, so any session can write; with SNOB_NO_BROWSER the session needs \
                      one, which \"snob login --browser\" captures or \"snob login --paste \
                      --csrftoken\" adds."
    )]
    Unfollow(FollowArgs),

    /// Track an account over time and report what changed
    #[command(
        after_help = "With no subcommand it stays up and runs on a schedule; the subcommands \
                      are the things a person does by hand. \"--json\" here emits one JSON \
                      object per run, one per line -- what the list commands would call \
                      ndjson, spelled --json because each object is the state of one run.\n\n\
                      An account named like a subcommand -- \"status\", \"once\", \"diff\", \
                      \"check\", \"setup\" -- is read as the subcommand; write it \"@status\" \
                      to watch the account."
    )]
    Watch(WatchArgs),

    /// The walks running as jobs for an agent: list them, see one, stop one
    #[command(
        args_conflicts_with_subcommands = true,
        after_help = "A walk an agent starts through \"snob mcp\" may run as a job, which goes \
                      on after the agent's call has answered and is asked about by its id. One \
                      runs at a time per account. Each is kept with its result for a day, \
                      and its record for a week.\n\n\
                      With no subcommand, the jobs not finished yet; --all, every one kept. \
                      \"snob jobs cancel <id>\" stops a walk as Ctrl+C stops it: what it read is \
                      kept, and the next walk of the list continues from there. Nothing here \
                      sends anything; \"snob logout\" and \"snob purge\" stop the account's jobs \
                      first."
    )]
    Jobs(JobsArgs),

    /// Read Instagram's own data export, with no session and no request
    #[command(
        subcommand,
        after_help = "The one way to answer who does not follow you back without anything \
                      talking to Instagram on your behalf: request \"Followers and following\" \
                      as HTML or JSON from Accounts Center, \"Download your information\", and \
                      point this at the zip, or the folder it was extracted into. It reads the \
                      export and prints the answer; nothing is stored and nothing is sent to \
                      Instagram. What \
                      it shows describes the moment Instagram built the export."
    )]
    Import(crate::commands::import::ImportCommand),

    /// Offer snob to an AI agent as an MCP server, on standard input and output
    /// or over HTTP on loopback
    #[cfg(feature = "mcp")]
    #[cfg_attr(
        feature = "mcp-http",
        command(after_help = concat!(mcp_after_help!(), "\n\n", mcp_listen_help!()))
    )]
    // A build without the HTTP half (`mcp-http`) has no --listen, no key and
    // no remote server of its own to tell of.
    #[cfg_attr(
        not(feature = "mcp-http"),
        command(
            about = "Offer snob to an AI agent as an MCP server, on standard input and output",
            after_help = mcp_after_help!()
        )
    )]
    Mcp(McpArgs),

    /// Update snob to its latest release, the way it was installed
    #[command(
        after_help = "Without an option, asks GitHub for the latest release and, when it is newer, \
                      updates this copy of snob after asking: with the package manager that \
                      installed it (Homebrew, Scoop, WinGet, Cargo), by replacing itself where \
                      the install script put it, or, for the .deb, with the apt command it \
                      prints. Every download is checked against the release's SHA256SUMS.\n\n\
                      snob also checks once a day on its own, downloads a newer release, and \
                      applies it the next time it starts with no other snob running (or after \
                      a restart); \"--auto\" says how much of that it does, and \
                      SNOB_NO_UPDATE_CHECK=1 turns it off. Nothing here talks to Instagram."
    )]
    Update(UpdateArgs),

    /// The process that runs the browsers for every command (`owner`).
    /// Started by the commands themselves; nobody types it.
    #[command(name = crate::owner::COMMAND, hide = true)]
    BrowserOwner,

    /// The updater's own processes (`update`): started by the commands;
    /// nobody types them.
    #[command(name = crate::update::COMMAND, hide = true, subcommand)]
    Updater(UpdaterCommand),

    /// A job's own processes (`jobs`): started by the commands and by
    /// `snob mcp`; nobody types them.
    #[command(name = crate::jobs::start::COMMAND, hide = true, subcommand)]
    Job(JobCommand),
}

/// `snob update`.
#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Only say whether a newer release is out; download nothing
    #[arg(long, conflicts_with_all = ["status", "auto"])]
    pub check: bool,

    /// Say what the updater knows and has done, asking nobody
    #[arg(long, conflicts_with = "auto")]
    pub status: bool,

    /// How much snob does on its own: off, notify (say a release is out),
    /// download (and fetch it), or apply (and apply it when snob next starts
    /// alone; the default)
    #[arg(long, value_enum, value_name = "MODE")]
    pub auto: Option<AutoUpdate>,

    /// Print the updater's state as JSON once done
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub consent: ConsentArgs,
}

/// `snob update --auto`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUpdate {
    Off,
    Notify,
    Download,
    Apply,
}

/// `snob __update`: the updater's background processes (`update`).
#[derive(Subcommand, Debug)]
pub enum UpdaterCommand {
    /// Ask GitHub for the latest release, and fetch it when it is newer
    Fetch,
    /// Run the package manager's upgrade, once no snob runs
    Apply(UpdaterApplyArgs),
}

#[derive(Args, Debug)]
pub struct UpdaterApplyArgs {
    /// The binary the upgrade is for, which says how snob was installed
    #[arg(long = "for", value_name = "PATH")]
    pub exe: std::path::PathBuf,
}

/// `snob __job`: what starts, runs and reads a job (`jobs`).
#[derive(Subcommand, Debug)]
pub enum JobCommand {
    /// Start a list walk as a job, and answer with one JSON document
    Start(JobStartArgs),
    /// Run a job: the supervisor, started detached by `start`
    Run(JobRunArgs),
    /// One job, and its result once it has one
    Show(JobShowArgs),
    /// The jobs not finished yet
    List(JobListArgs),
    /// Ask a job to stop
    Cancel(JobCancelArgs),
}

#[derive(Args, Debug)]
pub struct JobStartArgs {
    /// Answer with the result if the walk ends within this long
    #[arg(long, value_name = "DURATION", default_value = "0s", value_parser = duration)]
    pub wait: std::time::Duration,

    /// The tool call the job is for, recorded with it
    #[arg(long, value_name = "NAME")]
    pub tool: Option<String>,

    /// The call's arguments, as a JSON object, recorded with it
    #[arg(long, value_name = "JSON")]
    pub arguments: Option<String>,

    /// The client that started it, by name and version
    #[arg(long, value_name = "CLIENT")]
    pub started_by: Option<String>,

    /// Make nothing: say whether the walk would start now, or be the job
    /// already running it, or what would refuse it
    #[arg(long)]
    pub check: bool,

    /// The walk, after `--`: one of the list commands and its flags
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Args, Debug)]
pub struct JobRunArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub command: Option<JobsCommand>,

    #[command(flatten)]
    pub list: JobListArgs,
}

/// `snob jobs`: reading and stopping only. A job is started by an agent's
/// call, never typed.
#[derive(Subcommand, Debug)]
pub enum JobsCommand {
    /// The jobs not finished yet, or every one kept
    List(JobListArgs),
    /// One job: what it walks, how far it is, and how it ended
    Show(JobShowArgs),
    /// Stop a job as Ctrl+C stops a walk, keeping what it read
    Cancel(JobCancelArgs),
}

#[derive(Args, Debug)]
pub struct JobShowArgs {
    /// The job's id, as "snob jobs" lists it
    pub id: String,

    /// Wait up to this long for it to finish
    #[arg(long, value_name = "DURATION", default_value = "0s", value_parser = duration)]
    pub wait: std::time::Duration,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct JobListArgs {
    /// Every job kept, the finished ones too
    #[arg(long)]
    pub all: bool,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct JobCancelArgs {
    /// The job's id, as "snob jobs" lists it
    pub id: String,

    /// Wait up to this long for it to have stopped
    #[arg(long, value_name = "DURATION", default_value = "0s", value_parser = duration)]
    pub wait: std::time::Duration,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("method").multiple(false))]
pub struct LoginArgs {
    /// Paste the sessionid copied from the developer tools
    #[arg(long, group = "method")]
    pub paste: bool,

    /// Open a browser, wait for you to log in, and capture the session
    #[arg(long, group = "method")]
    pub browser: bool,

    /// Log in from the browser of the computer you are at, through an SSH tunnel
    ///
    /// For a machine reached over SSH, where a browser window would open
    /// nowhere you can see: snob's browser runs here without one, and prints
    /// an ssh command that opens a tunnel from your computer and a link to
    /// open there. You log in on the page that link shows, which is snob's
    /// browser seen from afar, and the session stays on this machine as with
    /// --browser. The link works once; the tunnel carries pictures and input,
    /// never the session.
    #[arg(long, group = "method")]
    pub remote: bool,

    /// User-Agent of the session, for requests sent without the browser
    ///
    /// Only the requests sent directly (SNOB_NO_BROWSER) and the media
    /// downloads carry it: from the browser, every request carries the
    /// browser's own User-Agent, whatever is given here. If omitted, it is
    /// worked out from the installed browser. It has to match the browser the
    /// session came from, or Instagram will reject the session.
    #[arg(long, value_name = "STRING")]
    pub user_agent: Option<String>,

    /// The csrftoken cookie, alongside the pasted sessionid
    ///
    /// Only "follow" and "unfollow" need it, and only when requests are sent
    /// directly (SNOB_NO_BROWSER): from the browser, the token is the
    /// browser's own. "--browser" picks it up on its own either way.
    ///
    /// A value typed here lands in the shell history and in "ps"; the
    /// SNOB_CSRFTOKEN environment variable is read instead when it is set.
    #[arg(
        long,
        value_name = "TOKEN",
        requires = "paste",
        env = "SNOB_CSRFTOKEN",
        hide_env_values = true
    )]
    pub csrftoken: Option<String>,

    /// Sign in as another account, besides those signed in here
    ///
    /// Without it, and with an account signed in already, login asks whether
    /// to log in again as the account in use or add another; with nobody to
    /// ask, it logs in again. --account names the account to log in again as.
    #[arg(long)]
    pub add: bool,

    /// Does nothing any more: the profile a login creates is always kept,
    /// because snob sends its requests from it. Hidden and accepted, so a
    /// script written for it still runs.
    #[arg(long, requires = "browser", hide = true)]
    pub keep_profile: bool,
}

#[derive(Args, Debug)]
pub struct WhoamiArgs {
    /// Do not check with Instagram whether the session is still alive
    #[arg(long)]
    pub offline: bool,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    /// The file's address on Instagram's CDN
    pub url: String,

    /// Destination file; "-" for standard output
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// The User-Agent to send, instead of the account's or the browser's
    #[arg(long, value_name = "STRING")]
    pub user_agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// The session: which account, where it is stored, when it was last checked
    #[arg(long)]
    pub session: bool,

    /// The request budget: what goes out now without a wait, and accounts read today
    #[arg(long)]
    pub budget: bool,

    /// How long Instagram's answers took in the last 24 hours, and whether the pace is slowing for them
    #[arg(long)]
    pub answers: bool,

    /// The cooldown: whether one stands, until when and why, and the last one
    #[arg(long)]
    pub cooldown: bool,

    /// Your lists as stored: when each was last walked, and a walk left to resume
    #[arg(long)]
    pub lists: bool,

    /// The monitor's last run on each account it watches from this one
    #[arg(long)]
    pub watch: bool,

    /// What agents were told yes to through snob mcp in the last 24 hours
    #[arg(long)]
    pub agents: bool,

    /// The walks running as jobs, what they walk, since when and for whom
    #[arg(long)]
    pub jobs: bool,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

/// Which parts of `snob status` a run asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSections {
    pub session: bool,
    pub budget: bool,
    pub answers: bool,
    pub cooldown: bool,
    pub lists: bool,
    pub watch: bool,
    pub agents: bool,
    pub jobs: bool,
}

impl StatusSections {
    /// The JSON keys of the sections this run shows, in the order it shows
    /// them.
    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.session, "session"),
            (self.budget, "budget"),
            (self.answers, "answers"),
            (self.cooldown, "cooldown"),
            (self.lists, "lists"),
            (self.watch, "watch"),
            (self.agents, "agents"),
            (self.jobs, "jobs"),
        ]
        .into_iter()
        .filter_map(|(shown, name)| shown.then_some(name))
        .collect()
    }
}

impl StatusArgs {
    /// The sections named, or every one when none was.
    /// Whether a section was named, which narrows the report to those
    /// named; otherwise it is the whole report, the updater's state included.
    pub fn names_a_section(&self) -> bool {
        let s = self.sections_named();
        s.session || s.budget || s.answers || s.cooldown || s.lists || s.watch || s.agents || s.jobs
    }

    fn sections_named(&self) -> StatusSections {
        StatusSections {
            session: self.session,
            budget: self.budget,
            answers: self.answers,
            cooldown: self.cooldown,
            lists: self.lists,
            watch: self.watch,
            agents: self.agents,
            jobs: self.jobs,
        }
    }

    pub fn sections(&self) -> StatusSections {
        if self.names_a_section() {
            self.sections_named()
        } else {
            StatusSections {
                session: true,
                budget: true,
                answers: true,
                cooldown: true,
                lists: true,
                watch: true,
                agents: true,
                jobs: true,
            }
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum AccountCommand {
    /// The accounts signed in here; * marks the active one
    List(AccountListArgs),

    /// Make an account the one commands act as when none is named
    Use(AccountUseArgs),
}

#[derive(Args, Debug)]
pub struct AccountListArgs {
    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct AccountUseArgs {
    /// The account, by username or id
    // Not `account`: that is the global flag's name.
    #[arg(value_name = "USER")]
    pub name: String,
}

#[derive(Args, Debug)]
pub struct LogoutArgs {
    /// Log every account out, not only the one in use
    #[arg(long)]
    pub all: bool,

    /// Does nothing any more: logout always deletes the browser profile, which
    /// holds the session snob sends its requests with. Hidden and accepted, so
    /// a script written for it still runs.
    #[arg(long, hide = true)]
    pub purge_profile: bool,
}

#[derive(Args, Debug)]
pub struct PurgeArgs {
    #[command(flatten)]
    pub consent: ConsentArgs,

    /// List what would be deleted and delete nothing
    #[arg(long, conflicts_with = "yes")]
    pub dry_run: bool,
}

/// The options every command that prints a list of accounts takes.
///
/// Composed from three groups rather than written as one struct, and the
/// reason is `scan`: it walks the same two lists with the same filter and
/// writes to the same destinations, but it prints counts, so `--limit` has
/// nothing to trim. A command takes the groups it acts on — [`ScanArgs`] is
/// this without the cap — and a flag it would ignore is one clap refuses.
///
/// The order of the fields is the order of the help, so a reader meets the
/// target, then what to show, then where, then how the walk is made.
#[derive(Args, Debug)]
pub struct ListArgs {
    /// Account to analyze. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Trim the output to the first N accounts. Saves no requests: --max-pages
    /// is what does that.
    #[arg(long, value_name = "N", conflicts_with = "dry_run")]
    pub limit: Option<usize>,

    #[command(flatten)]
    pub browse: BrowseArgs,

    #[command(flatten)]
    pub walk: WalkArgs,

    #[command(flatten)]
    pub agent: AgentArgs,

    /// Walk as a job that outlives this command, and say its id at once:
    /// "snob jobs show <id>" says how far it is and, once it ends, what it
    /// found. One job runs at a time per account. Asks first, as the walk
    /// would, for another account's lists
    // Refused beside every flag a job's command does not take
    // (`jobs::argv::ALLOWED`), rather than leaving one out quietly: the job
    // would walk with the default where somebody typed something else.
    #[arg(
        long,
        conflicts_with_all = [
            "dry_run", "format", "path", "interactive", "hide", "only", "no_verified",
            "exclude_list", "max_age", "no_resume", "same_day", "confirmed_via",
        ]
    )]
    pub background: bool,
}

/// `snob scan`: [`ListArgs`] without `--limit`, because a summary of five
/// counts has no rows to cut.
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Account to summarize. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    #[command(flatten)]
    pub browse: BrowseArgs,

    #[command(flatten)]
    pub walk: WalkArgs,
}

/// The list commands' way into the account browser. One definition, like
/// [`ConsentArgs`], so the flags and their conflicts cannot drift between the
/// six commands that take them.
///
/// **The browser is the default at a human terminal**, the same rule the
/// media browsers keep and for the same reason, the owner's: the browsers are
/// what a person at a terminal wants first. The printed listing is still the
/// answer everywhere else, by the same detection the media commands use: a
/// pipe, a redirect and a script are exactly the runs `attending` refuses, so
/// scrollback, the terminal's own search and `snob unfollowers | wc -l` all
/// read the printed form. [`BrowseArgs::browses`] is
/// [`MediaActionArgs::browses`] minus the verbs lists do not have, and the
/// matrix is a test the same way.
///
/// One conflict is stricter than the media group's: `-i` is refused beside
/// `--format` and `-o` rather than out-ranking them, because a list browser
/// downloads nothing — there is no later step for a format or a destination
/// to apply to, so accepting either would be accepting-and-ignoring it.
/// `--no-interactive` conflicts with `-i` alone: beside `--format` or `-o`
/// it is redundant rather than ignored — each of those already prints — and
/// a script that says it defensively should not break.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct BrowseArgs {
    /// Move through the result with the arrow keys: Enter opens the
    /// account's profile, "/" filters as you type. The default on a
    /// terminal; this forces it, and fails where no terminal can be drawn
    #[arg(short = 'i', long, conflicts_with_all = ["format", "path"])]
    pub interactive: bool,

    /// Print the result and exit, even on a terminal
    #[arg(long, conflicts_with = "interactive")]
    pub no_interactive: bool,
}

impl BrowseArgs {
    /// Whether this run takes the terminal over. The order is
    /// `browse_decision`'s. `attending` is
    /// [`crate::ui::a_human_would_watch_the_listing_scroll_by`]: all three
    /// streams a terminal, because the browser reads keys, draws on standard
    /// error and *withholds* the result from standard output.
    ///
    /// A pure function of its inputs so the whole matrix is testable without
    /// a terminal; the caller supplies the one detected bit.
    pub fn browses(&self, printed_form_asked: bool, attending: bool) -> bool {
        browse_decision(
            self.interactive,
            self.no_interactive,
            printed_form_asked,
            attending,
        )
    }
}

/// The one rule every browse group applies: what was *said* wins over what
/// is detected — `-i` first, then anything that already asked for the
/// printed or written form — and only a run that asked for nothing at all
/// falls to detection.
///
/// One function rather than four copies of the ordering, because the
/// ordering is the contract and four copies of a contract drift. The groups
/// stay separate structs — their conflicts differ, and clap conflicts are
/// declared per command — but the decision they feed is this one.
fn browse_decision(
    interactive: bool,
    no_interactive: bool,
    printed_form_asked: bool,
    attending: bool,
) -> bool {
    if interactive {
        return true;
    }
    if no_interactive || printed_form_asked {
        return false;
    }
    attending
}

/// Which accounts a list keeps.
#[derive(Args, Debug, Clone, Default)]
pub struct FilterArgs {
    /// Hide accounts with any of these attributes
    #[arg(long, value_delimiter = ',', value_name = "ATTR")]
    pub hide: Vec<Attr>,

    /// Show only accounts with all of these attributes
    // Written out because the two combine in opposite ways: `--only
    // verified,private` reads as "the verified ones and the private ones" and
    // returns neither — it means verified AND private. See `Filter::allows`
    // for why that is the useful reading of `only`.
    #[arg(long, value_delimiter = ',', value_name = "ATTR")]
    pub only: Vec<Attr>,

    /// Shorthand for --hide verified
    // Hidden rather than removed: it is the spelling the first release
    // documented, so a script written against it still runs. It is kept out of
    // the help because `--hide` is the general form and a second way to say
    // one thing is the beginning of one per attribute.
    #[arg(long, hide = true)]
    pub no_verified: bool,

    /// File of usernames to exclude from the result, one per line
    #[arg(long, value_name = "FILE")]
    pub exclude_list: Option<PathBuf>,
}

/// Where a list goes and in what shape.
#[derive(Args, Debug, Clone, Default)]
pub struct OutputArgs {
    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Write the result to a file instead of standard output ("-" is
    /// standard output)
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub path: Option<PathBuf>,
}

/// Which of the two lists a command is about, when it may be either.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListChoice {
    Followers,
    Following,
}

impl From<ListChoice> for snob_core::model::ListKind {
    fn from(choice: ListChoice) -> Self {
        match choice {
            ListChoice::Followers => Self::Followers,
            ListChoice::Following => Self::Following,
        }
    }
}

#[derive(Args, Debug)]
pub struct SnapshotsArgs {
    /// Account whose captures to list. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// Only the captures of this list
    #[arg(long, value_enum)]
    pub list: Option<ListChoice>,

    #[command(flatten)]
    pub output: OutputArgs,
}

#[derive(Args, Debug)]
pub struct ChangesArgs {
    /// Account whose lists to compare. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// Only this list
    #[arg(long, value_enum)]
    pub list: Option<ListChoice>,

    /// Compare against the newest capture from before this moment: a date
    /// (2026-11-01) or how long ago (7d, 12h)
    #[arg(long, value_name = "WHEN", value_parser = since, conflicts_with_all = ["from", "to"])]
    pub since: Option<Since>,

    /// The earlier capture, by its id in "snob snapshots"
    #[arg(long, value_name = "ID")]
    pub from: Option<i64>,

    /// The later capture, by its id; the newest of the same list when left out
    #[arg(long, value_name = "ID")]
    pub to: Option<i64>,

    #[command(flatten)]
    pub output: OutputArgs,
}

/// The moment `--since` names, kept as typed until the run reads the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// How long before now.
    Ago(std::time::Duration),
    /// The start of a day, in the local zone.
    Day(chrono::NaiveDate),
}

impl Since {
    /// The moment, against `now`.
    pub fn at(self, now: snob_core::Epoch) -> snob_core::Epoch {
        match self {
            Self::Ago(ago) => now - ago,
            Self::Day(day) => {
                use chrono::TimeZone;
                let midnight = day.and_hms_opt(0, 0, 0).unwrap_or_default();
                let at = chrono::Local
                    .from_local_datetime(&midnight)
                    .earliest()
                    .map_or_else(|| midnight.and_utc().timestamp(), |t| t.timestamp());
                snob_core::Epoch::new(at)
            }
        }
    }
}

fn since(text: &str) -> Result<Since, String> {
    if let Ok(day) = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Ok(Since::Day(day));
    }
    snob_core::duration::parse(text)
        .map(Since::Ago)
        .map_err(|_| format!("\"{text}\" is neither a date (2026-11-01) nor a duration (7d, 12h)"))
}

/// The one question a command asks, answered in advance.
///
/// One definition for the one convention: each command asks at most one thing
/// — consent to enumerate somebody else's lists, confirmation of a write,
/// confirmation of a purge — and `-y` is always the answer to that one thing.
/// What the question *is* stays with the command that asks it, in its
/// `after_help` and in the question itself.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct ConsentArgs {
    /// Answer yes in advance to the one question this command asks.
    /// Needed when there is no terminal to ask at.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Whether the progress bar draws. One definition, like [`ConsentArgs`]:
/// the bar already hides itself when standard error is not a terminal, so
/// this flag is only for the terminal that has one and does not want it.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct ProgressArgs {
    /// Do not draw the progress bar
    #[arg(long)]
    pub no_progress: bool,
}

/// The status-object switch, for commands that report the state of things.
///
/// `--json` is for a status object; `--format` is for a document. One
/// definition holds the first half of that convention the way the narrowed
/// format enums hold the second.
#[derive(Args, Debug, Clone, Copy, Default)]
pub struct StatusOutputArgs {
    /// Return the data as JSON
    #[arg(long)]
    pub json: bool,
}

/// How a walk is made: whether to make one at all, how far, and whether
/// anybody has to be asked first.
///
/// `--offline` answers out of storage and spends nothing, so the two flags
/// that shape a walk conflict with it rather than being accepted and ignored.
#[derive(Args, Debug, Clone)]
pub struct WalkArgs {
    /// Walk the list again even if there is a recent snapshot
    #[arg(long, conflicts_with = "offline")]
    pub refresh: bool,

    /// Answer from the last stored snapshot without touching the network
    // `--cache` is the earlier name, kept as a hidden alias so a script
    // written against it still runs, the same way `mutuals` and
    // `--no-verified` are kept. `--offline` is the spelling that generalizes:
    // `whoami` uses it too, for the one intention a script has — spend no
    // network.
    #[arg(long, alias = "cache", conflicts_with = "refresh")]
    pub offline: bool,

    /// Maximum age of a reusable snapshot whose counter has not moved (30m,
    /// 6h, 2d)
    // `engine::DEFAULT_MAX_AGE`, which a test below holds this string to.
    #[arg(long, value_name = "DURATION", default_value = "24h", value_parser = duration)]
    pub max_age: std::time::Duration,

    /// Start from scratch instead of continuing an interrupted walk
    #[arg(long, conflicts_with = "offline")]
    pub no_resume: bool,

    /// Stop the walk after N pages, saving requests
    #[arg(long, value_name = "N", conflicts_with = "offline")]
    pub max_pages: Option<u32>,

    /// If the list does not fit in today's account budget, finish it today
    /// anyway instead of pausing until the day makes room (riskier for the
    /// account: past the day's account budget, only the budget of about 2,000
    /// requests a day stops the walk)
    #[arg(long, conflicts_with = "offline")]
    pub same_day: bool,

    /// Say what the walks would cost, against what today has left, and send
    /// nothing
    // Refused beside every flag it would ignore: it asks nothing (`-y`),
    // draws no bar, opens no view, and lists nobody to filter. `--limit` says
    // the same on its own side, since `scan` has none. `--format` and `-o`
    // stay, for the estimate's own two forms (`commands::dry_run`).
    #[arg(
        long,
        conflicts_with_all = [
            "offline", "yes", "no_progress", "interactive", "no_interactive",
            "hide", "only", "no_verified", "exclude_list",
        ]
    )]
    pub dry_run: bool,

    #[command(flatten)]
    pub progress: ProgressArgs,

    #[command(flatten)]
    pub consent: ConsentArgs,
}

/// What no flags mean. Written by hand because a derived `Default` would put
/// `--max-age` at zero seconds, and a test building arguments with
/// `..Default::default()` would then find every snapshot stale.
impl Default for WalkArgs {
    fn default() -> Self {
        Self {
            refresh: false,
            offline: false,
            max_age: crate::engine::DEFAULT_MAX_AGE,
            no_resume: false,
            max_pages: None,
            same_day: false,
            dry_run: false,
            progress: ProgressArgs::default(),
            consent: ConsentArgs::default(),
        }
    }
}

/// Parses durations written like `30m`, `6h`, `2d`, `2w`.
///
/// A thin wrapper because clap wants this exact signature. The parser itself
/// lives in `snob-core`: the monitor's schedule and its configuration file read
/// the same durations, and a second copy that understood `w` while this one did
/// not is how `--max-age 2w` comes to mean two seconds.
fn duration(text: &str) -> Result<std::time::Duration, String> {
    snob_core::duration::parse(text)
}

// The schedule's flags are judged here, each by the domain rule that would
// refuse it later, so a mistake in one is a mistake on the command line:
// exit 2 and clap's message, before the file is read or anything is
// settled, as every other value typed wrong is. Whether the flags make a
// schedule together (`--on` with neither `--at` nor `--every`) is still
// `schedule_from`'s question, since clap judges values one at a time.

/// `--every`: a duration, and one the monitor may run at
/// (`Schedule::every`: not twice inside its minimum gap, nor rarer than it
/// can count).
fn interval(text: &str) -> Result<std::time::Duration, String> {
    let interval = duration(text)?;
    snob_core::watch::schedule::Schedule::every(interval)
        .map(|_| interval)
        .map_err(|e| e.to_string())
}

/// `--at`: a time of day, kept as typed for the file and the banner.
fn clock_time(text: &str) -> Result<String, String> {
    snob_core::watch::schedule::parse_time(text)
        .map(|_| text.trim().to_string())
        .map_err(|e| e.to_string())
}

/// `--on`: a day of the week, kept as typed.
fn weekday(text: &str) -> Result<String, String> {
    snob_core::watch::schedule::Weekday::parse(text)
        .map(|_| text.to_string())
        .ok_or_else(|| format!("\"{text}\" is not a day (try mon, thu)"))
}

/// `--cron`: five fields that name at least one moment.
fn cron_expression(text: &str) -> Result<String, String> {
    snob_core::watch::schedule::Schedule::cron(text)
        .map(|_| text.to_string())
        .map_err(|e| e.to_string())
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct WatchArgs {
    /// Absent means "run on a schedule".
    #[command(subcommand)]
    pub command: Option<WatchCommand>,

    #[command(flatten)]
    pub run: WatchRunArgs,
}

/// `snob watch` with no subcommand: stay up and run on a schedule.
#[derive(Args, Debug, Default)]
pub struct WatchRunArgs {
    /// Account to watch. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// How often to run: 6h, 2d, 2w
    #[arg(long, value_name = "DURATION", value_parser = interval)]
    pub every: Option<std::time::Duration>,

    /// Times of day to run at: 09:00,21:00
    #[arg(long, value_delimiter = ',', value_name = "HH:MM", value_parser = clock_time)]
    pub at: Vec<String>,

    /// Days to run on: mon,thu. Defaults to every day.
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "DAYS",
        conflicts_with = "cron",
        value_parser = weekday
    )]
    pub on: Vec<String>,

    /// A five-field cron expression, for a schedule you already have written
    #[arg(long, value_name = "EXPR", conflicts_with = "at", value_parser = cron_expression)]
    pub cron: Option<String>,

    /// How far a run may be pushed later, so it does not land on the same
    /// second every day. Worked out from the interval if not given; 0 turns it
    /// off.
    #[arg(long, value_name = "DURATION", value_parser = duration)]
    pub jitter: Option<std::time::Duration>,

    /// Run once at start, then follow the schedule
    #[arg(long)]
    pub now: bool,

    #[command(flatten)]
    pub delivery: WebhookArgs,

    // A doc comment here would not reach the help -- clap renders the
    // flattened group's own docs -- so the "one object per run, one per line"
    // sentence lives in the Watch command's after_help, where it shows.
    #[command(flatten)]
    pub output: StatusOutputArgs,

    #[command(flatten)]
    pub progress: ProgressArgs,
}

/// Where a report goes, shared by the scheduled run and `once`.
#[derive(Args, Debug, Clone, Default)]
pub struct WebhookArgs {
    /// POST each report to this address, as JSON
    #[arg(long, value_name = "URL")]
    pub webhook: Option<String>,

    /// Header to send with it, repeatable: --header "Authorization: Bearer x"
    // A literal token here ends up in the shell history and in `ps`. The help
    // says so rather than the code refusing it: this is the shape that works in
    // a systemd unit, where the value comes from an environment file.
    #[arg(long, value_name = "NAME: VALUE")]
    pub header: Vec<String>,

    /// Sign the body with this secret, so the receiver can check it came from
    /// here. Sent as an X-Snob-Signature header. At least 32 characters.
    ///
    /// The floor is checked here rather than at the point of sending, and that
    /// is deliberate: a report is queued before it is delivered, so refusing a
    /// weak key at send time would strand one that had already been made.
    /// Here, nothing has been queued yet.
    ///
    /// A value typed here lands in the shell history and in "ps", where on
    /// Linux every local user can read it for as long as the run lasts; the
    /// SNOB_SIGNING_KEY environment variable is read instead when it is set,
    /// which is also the shape a systemd unit wants. "snob watch setup" puts
    /// the key in the keyring, and then neither is needed.
    #[arg(
        long,
        value_name = "SECRET",
        value_parser = signing_secret,
        env = "SNOB_SIGNING_KEY",
        hide_env_values = true
    )]
    pub sign_with: Option<String>,

    /// Send a report even when nothing changed, so something watching for
    /// silence can tell "nothing happened" from "it stopped running"
    #[arg(long)]
    pub heartbeat: bool,

    /// What the body is: json (the report, for a program), or the summary as
    /// slack, discord, ntfy or text want it posted. The address in watch.toml
    /// takes its "format" from there.
    #[arg(long, value_name = "FORMAT", value_parser = webhook_format)]
    pub webhook_format: Option<snob_store::config::WebhookFormat>,
}

/// One of the five words `format` under `[webhook]` takes, read as the file
/// reads it so the two cannot disagree.
fn webhook_format(text: &str) -> Result<snob_store::config::WebhookFormat, String> {
    #[derive(serde::Deserialize)]
    struct One {
        format: snob_store::config::WebhookFormat,
    }
    serde_json::from_value::<One>(serde_json::json!({ "format": text }))
        .map(|one| one.format)
        .map_err(|_| format!("\"{text}\" is not json, slack, discord, ntfy or text"))
}

/// The monitor.
///
/// Optional: `snob watch` with no subcommand is the scheduled run, taking its
/// interval from the command line or from `watch.toml`. The subcommands are the
/// things a person does by hand — look once, read the last diff, configure it,
/// ask what it has been doing.
#[derive(Subcommand, Debug)]
pub enum WatchCommand {
    /// What has changed since the last time the monitor reported
    #[command(
        after_help = "Reads what is already stored and spends no requests, so it costs nothing \
                      to run as often as you like and it never moves the monitor on: ask twice \
                      and you get the same answer.\n\n\
                      With nothing walked yet there is nothing to compare against. Run \
                      \"snob followers\" or \"snob following\" once first."
    )]
    Diff(WatchDiffArgs),

    /// Look now, report what changed, and remember having reported it
    #[command(
        after_help = "One run of the monitor. Meant for cron, a systemd timer or Windows Task \
                      Scheduler; \"snob watch\" with no subcommand schedules \
                      itself instead.\n\n\
                      It reads the account's counters and only walks a list if its counter moved, \
                      so a run with nothing to report costs one request per watched account, \
                      two the first time an account's name is seen. Unlike \"diff\", \
                      this moves the monitor on: whatever it reports is not reported again.\n\n\
                      The first run on an account has nothing to compare against, so it reports \
                      nothing and says so."
    )]
    Once(WatchOnceArgs),

    /// Write the configuration file, step by step
    #[command(
        after_help = "Asks how often to run, where to send the reports, and which accounts to \
                      watch, then writes a file you can edit afterwards.\n\n\
                      A token or a signing key goes into the system keyring, never into the \
                      file: it sits at a guessable path and would end up in every backup of \
                      your home directory. \"snob purge\" removes both."
    )]
    Setup(WatchSetupArgs),

    /// Check the configuration would work, before it runs unattended
    #[command(
        after_help = "Everything a scheduled run needs, checked while somebody is still here to \
                      fix it: the schedule through the evaluator that actually decides it, the \
                      session, that each configured account resolves and may be read, and the \
                      webhook — by posting one \"watch.preflight\" message to it.\n\n\
                      It writes nothing and walks no list, and it exits non-zero when something \
                      would stop a run, which is what makes it usable as a monitoring probe. \
                      Poll it hourly rather than by the minute: every invocation is charged to \
                      the same daily budget the walks draw on, and a probe that drains it \
                      causes the condition it is watching for.\n\n\
                      Cost: one request per configured account, and from the browser one \
                      more for another account until a list snob walked has named it. The \
                      session is checked from the page the browser already loaded, which \
                      costs none; with \
                      SNOB_NO_BROWSER it costs one request, and one more while the session has \
                      not learned its own account's name — which it does the first time \
                      \"snob whoami\" runs."
    )]
    Check(WatchCheckArgs),

    /// What is configured, when it last ran and runs next, and what is still
    /// owed
    Status(WatchStatusArgs),

    /// Send the reports still owed to the webhook, and nothing else
    #[command(
        after_help = "For a receiver that was down and is back: what the monitor queued for it \
                      goes now, whatever wait the queue had set before the next try, without \
                      waiting for the next run. Up to ten reports per account at a time; run it \
                      again for more.\n\n\
                      It walks nothing and sends no request to Instagram, so it needs no \
                      session and costs nothing in the budget. Only the reports queued for \
                      this webhook's address go, in the shape they were queued in.\n\n\
                      Exits 1 (reason not_delivered) when a report it tried did not arrive."
    )]
    Deliver(WatchDeliverArgs),

    /// Print what to give a scheduler: systemd, cron, launchd or schtasks
    #[command(
        after_help = "Prints and installs nothing. Every unit runs \"snob watch once \
                      --when-due\" every 15 minutes, and the run decides whether the schedule \
                      in watch.toml says it is due, so the schedule is kept in one place and \
                      a run that is not due sends nothing.\n\n\
                      With no argument, the unit for this system: systemd on Linux, launchd \
                      on macOS, schtasks on Windows. The path printed is one that survives an \
                      upgrade where the installer keeps one."
    )]
    Unit(WatchUnitArgs),
}

#[derive(Args, Debug, Default)]
pub struct WatchUnitArgs {
    /// Which scheduler. Defaults to this system's own
    pub kind: Option<UnitKind>,
}

/// The schedulers `snob watch unit` prints for.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Systemd,
    Cron,
    Launchd,
    Schtasks,
}

impl UnitKind {
    /// The scheduler this system comes with.
    pub fn of_this_platform() -> Self {
        if cfg!(windows) {
            Self::Schtasks
        } else if cfg!(target_os = "macos") {
            Self::Launchd
        } else {
            Self::Systemd
        }
    }
}

#[derive(Args, Debug, Default)]
pub struct WatchDeliverArgs {
    #[command(flatten)]
    pub delivery: WebhookArgs,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug, Default)]
pub struct WatchCheckArgs {
    #[command(flatten)]
    pub output: StatusOutputArgs,

    /// Do not post anything to the webhook
    #[arg(long)]
    pub no_webhook: bool,

    /// Sign the check's message with this secret, as a run given it signs
    /// its reports. SNOB_SIGNING_KEY is read when it is set.
    // The same flag and variable as a run's (`WebhookArgs::sign_with`): a
    // check that ignored the variable a scheduled run signs with posted an
    // unsigned message and said "ok", so the one path a headless machine
    // signs by was the one it could not probe.
    #[arg(
        long,
        value_name = "SECRET",
        value_parser = signing_secret,
        env = "SNOB_SIGNING_KEY",
        hide_env_values = true
    )]
    pub sign_with: Option<String>,
}

#[derive(Args, Debug)]
pub struct WatchSetupArgs {
    /// Print what would be written and write nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct WatchStatusArgs {
    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct WatchDiffArgs {
    /// Account to report on. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

#[derive(Args, Debug)]
pub struct WatchOnceArgs {
    #[command(flatten)]
    pub delivery: WebhookArgs,

    /// Account to watch. Defaults to your own.
    // No -y here, and deliberately. Consent to enumerate somebody else's lists
    // is a thing a person gives, and an unattended run that could be handed one
    // on the command line is one whose consent came from whoever wrote the cron
    // entry. Reading another account needs a terminal to ask at, or an
    // `[[account]]` in `watch.toml` carrying the answer somebody gave once,
    // which is the only thing an unattended run accepts.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// Run only if the schedule in watch.toml says a run is due, and
    /// otherwise exit 0 having sent nothing. For a timer that asks often:
    /// "snob watch unit" prints one
    #[arg(long)]
    pub when_due: bool,

    #[command(flatten)]
    pub output: StatusOutputArgs,

    #[command(flatten)]
    pub progress: ProgressArgs,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    /// Account to show. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<ProfileFormat>,

    /// Write the result to a file instead of standard output ("-" is
    /// standard output)
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    // Not [`BrowseArgs`], deliberately: that group's conflicts name the ids
    // `format` and `path`, and this command's `-o` field is `output` — clap
    // refuses unknown ids, and `Cli::command().debug_assert()` holds it.
    // The rule itself is shared through [`browse_decision`].
    /// Browse the profile with the arrow keys: Enter opens what is under the
    /// cursor. The default on a terminal; this forces it, and fails where no
    /// terminal can be drawn
    #[arg(short = 'i', long, conflicts_with_all = ["format", "output"])]
    pub interactive: bool,

    /// Print the profile and exit, even on a terminal
    #[arg(long, conflicts_with = "interactive")]
    pub no_interactive: bool,
}

impl ProfileArgs {
    /// Whether this run takes the terminal over. See [`browse_decision`].
    pub fn browses(&self, attending: bool) -> bool {
        browse_decision(
            self.interactive,
            self.no_interactive,
            self.format.is_some() || self.output.is_some(),
            attending,
        )
    }
}

#[derive(Args, Debug)]
pub struct PfpArgs {
    /// Account whose profile picture to download
    #[arg(value_parser = username)]
    pub target: String,

    /// Destination file; "-" for standard output
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Look at the picture before deciding: Enter opens it in the system
    /// viewer, D saves it here. The default on a terminal; this forces it,
    /// and fails where no terminal can be drawn
    #[arg(short = 'i', long, conflicts_with = "output")]
    pub interactive: bool,

    /// Download the picture and exit, even on a terminal
    #[arg(long, conflicts_with = "interactive")]
    pub no_interactive: bool,
}

impl PfpArgs {
    /// Whether this run takes the terminal over. See [`browse_decision`].
    pub fn browses(&self, attending: bool) -> bool {
        browse_decision(
            self.interactive,
            self.no_interactive,
            self.output.is_some(),
            attending,
        )
    }
}

/// What `-d/--download` selects, decided at the parser so a typo is exit 2
/// and nothing was fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadSelection {
    /// Every story in the tray.
    All,
    /// The numbered ones, in the order they were asked for: `3`, `1,3`, `2-4`.
    These(Vec<usize>),
}

/// Parses `all`, one number, or a comma-separated run of numbers and ranges.
///
/// The listing is one-based, so zero is refused by name rather than
/// underflowing later. The count is capped well past any real tray, so
/// `-d 1-999999999` is a parse error instead of an allocation.
fn download_selection(text: &str) -> Result<DownloadSelection, String> {
    const MOST: usize = 200;
    if text.eq_ignore_ascii_case("all") {
        return Ok(DownloadSelection::All);
    }
    let mut numbers: Vec<usize> = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        let (from, to) = match part.split_once('-') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (part, part),
        };
        let complaint =
            || format!("\"{part}\" is not a number from the listing, a range like 2-4, or \"all\"");
        let from: usize = from.parse().map_err(|_| complaint())?;
        let to: usize = to.parse().map_err(|_| complaint())?;
        if from == 0 {
            return Err("the listing starts at 1".to_string());
        }
        if from > to {
            return Err(format!("\"{part}\" runs backwards"));
        }
        for n in from..=to {
            if numbers.len() >= MOST {
                return Err(format!(
                    "that is more than {MOST}; \"all\" is the way to ask for everything listed"
                ));
            }
            if !numbers.contains(&n) {
                numbers.push(n);
            }
        }
    }
    Ok(DownloadSelection::These(numbers))
}

/// What a media command does with its tray: download some of it, or take the
/// terminal over and browse it.
///
/// One definition, written for `stories` and for `highlights`, which AGENTS.md
/// makes a command of its own with "a tray level and an items level" — so the
/// second command takes this group instead of copying four flags and their
/// conflicts.
#[derive(Args, Debug)]
pub struct MediaActionArgs {
    /// Download from the listing: a number, a set (1,3 or 2-4), or "all"
    #[arg(
        short = 'd',
        long,
        value_name = "N|all",
        value_parser = download_selection,
        conflicts_with = "interactive"
    )]
    pub download: Option<DownloadSelection>,

    /// What "-d all" was spelled before -d took it; hidden, kept so a script
    /// written against the first release still runs
    #[arg(long, hide = true, conflicts_with_all = ["download", "interactive"])]
    pub all: bool,

    /// Move through the listing with the arrow keys. The default on a
    /// terminal; this forces it, and fails where no terminal can be taken
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Print the listing and exit, even on a terminal
    // Long-only, like every flag since the short space was closed. It
    // conflicts with `-i` alone: beside `-d`, `-o` or `--format` it is
    // redundant rather than ignored -- each of those already prints -- and a
    // script that says `--no-interactive` defensively should not break the
    // day a download is added to it.
    #[arg(long, conflicts_with = "interactive")]
    pub no_interactive: bool,

    /// Where a download goes. A directory when several are saved, a file when
    /// one is; "-" writes one file to standard output, for a pipe. Without
    /// -d, the listing's file, "-" being standard output
    // Refused beside `-i` rather than accepted and ignored: the browser's own
    // D saves into the working directory, one item at a time, and has nowhere
    // to take a path from. `pfp` draws the same line.
    #[arg(short = 'o', long, value_name = "PATH", conflicts_with = "interactive")]
    pub output: Option<PathBuf>,
}

impl MediaActionArgs {
    /// `--all` folded into the selection, so a command reads one field.
    pub fn selection(&self) -> Option<DownloadSelection> {
        if self.all {
            return Some(DownloadSelection::All);
        }
        self.download.clone()
    }

    /// Where the listing is written, when this run lists rather than
    /// downloads: `-o` means the listing's file only then.
    pub fn listing_to(&self) -> Option<&std::path::Path> {
        self.selection()
            .is_none()
            .then_some(self.output.as_deref())
            .flatten()
            // `-o -` is standard output, where a listing goes anyway.
            .filter(|path| !crate::output::is_stdout(path))
    }

    /// How many files `-d` names, at least, before the listing is read: the
    /// count of the numbers given, or `None` for `all` or no `-d`. What
    /// `output::stdout_download_ahead` refuses `-o -` on before a request.
    pub fn files_named(&self) -> Option<usize> {
        match self.selection()? {
            DownloadSelection::These(numbers) => Some(numbers.len()),
            DownloadSelection::All => None,
        }
    }

    /// Refuses, before anything is asked of Instagram, a download to `-o -`
    /// that the command line already says cannot go there: standard output
    /// is a terminal, or the numbers given name several files. Only with a
    /// download asked for; a listing goes to standard output as it is.
    pub fn refuse_stdout_download_early(&self) -> anyhow::Result<()> {
        if self.selection().is_none() {
            return Ok(());
        }
        crate::output::stdout_download_ahead(self.output.as_deref(), self.files_named())
    }

    /// Whether this run takes the terminal over. The order is
    /// `browse_decision`'s; here the printed or downloaded form is asked for
    /// by `--no-interactive`, a selection, a destination or a format.
    /// `attending` is [`crate::ui::a_human_would_watch_the_listing_scroll_by`]:
    /// all three streams a terminal, because the browser reads keys, draws on
    /// standard error and *withholds* the listing from standard output.
    ///
    /// A pure function of its inputs so the whole matrix is testable without
    /// a terminal; the caller supplies the one detected bit.
    pub fn browses(&self, format_given: bool, attending: bool) -> bool {
        browse_decision(
            self.interactive,
            self.no_interactive,
            // A selection or a destination asks for the downloaded form the
            // way a format asks for the printed one.
            self.selection().is_some() || self.output.is_some() || format_given,
            attending,
        )
    }
}

/// The listing's format, apart from the actions so the conflicts can say it:
/// a download writes a file and the browser draws a screen, so `--format json
/// -d 3` is refused rather than accepted and ignored, which would read as a
/// format that did not work.
#[derive(Args, Debug)]
pub struct MediaListArgs {
    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum, conflicts_with_all = ["download", "all", "interactive"])]
    pub format: Option<StoryFormat>,
}

#[derive(Args, Debug)]
pub struct StoriesArgs {
    /// Account whose stories to show. Defaults to your own.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    #[command(flatten)]
    pub action: MediaActionArgs,

    #[command(flatten)]
    pub list: MediaListArgs,
}

#[derive(Args, Debug)]
pub struct HighlightsArgs {
    /// Account whose highlights to show. Defaults to your own — but the first
    /// bare word is always read as an account, so picking a highlight of your
    /// own takes your username in front of the number.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// A highlight's number from the tray listing: what -d and -i then act on
    /// is what that highlight holds
    #[arg(value_name = "HIGHLIGHT", value_parser = clap::value_parser!(u64).range(1..))]
    pub highlight: Option<u64>,

    #[command(flatten)]
    pub action: MediaActionArgs,

    #[command(flatten)]
    pub list: MediaListArgs,
}

#[derive(Args, Debug)]
pub struct PostsArgs {
    /// Account whose posts to show. Defaults to your own, but the first bare
    /// word is always read as an account, so picking a post of your own takes
    /// your username in front of the number.
    #[arg(value_parser = username)]
    pub target: Option<String>,

    /// A post's number from the grid listing: what -d and -i then act on is
    /// what that post holds
    #[arg(value_name = "POST", value_parser = clap::value_parser!(u64).range(1..))]
    pub post: Option<u64>,

    /// How many pages of twelve posts the listing reads, up to 40, one
    /// sitting. The view reads the next page as you reach it instead
    // A count of pages rather than of posts: a page is what is asked for,
    // one request each, and a person scrolling a grid decides how far in
    // pages. Forty is the sitting a list walk rests after
    // (`pace::PAGES_PER_SITTING`).
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..=40),
        conflicts_with = "interactive"
    )]
    pub pages: Option<u32>,

    #[command(flatten)]
    pub action: MediaActionArgs,

    #[command(flatten)]
    pub list: MediaListArgs,

    #[command(flatten)]
    pub video: VideoArgs,
}

#[derive(Args, Debug)]
pub struct PostArgs {
    /// A post's or a reel's link, or the code in it
    #[arg(value_name = "LINK")]
    pub link: String,

    /// How many pages of comments the listing reads, up to 20, about
    /// fifteen comments each, the first page included. The view reads the
    /// next page as you reach it instead
    // A count of pages, as `posts --pages`: a page is what is asked for, one
    // request each. Twenty is `posts::MOST_COMMENT_PAGES`, and each page after
    // the first waits as the app's "load more" is clicked
    // (`pace::COMMENT_STEP_MS`). A download reads the first page alone, as the
    // app opens a post with it.
    #[arg(
        long,
        value_name = "PAGES",
        value_parser = clap::value_parser!(u32).range(1..=20),
        conflicts_with_all = ["interactive", "download"]
    )]
    pub comments: Option<u32>,

    #[command(flatten)]
    pub action: MediaActionArgs,

    #[command(flatten)]
    pub list: MediaListArgs,

    #[command(flatten)]
    pub video: VideoArgs,
}

#[derive(Args, Debug)]
pub struct LikersArgs {
    /// A post's or a reel's link, or the code in it
    #[arg(value_name = "LINK")]
    pub link: String,

    #[command(flatten)]
    pub output: OutputArgs,

    #[command(flatten)]
    pub consent: ConsentArgs,

    #[command(flatten)]
    pub agent: AgentArgs,

    /// Send nothing and ask nothing: say what would refuse this read now
    /// if an agent made it, or that nothing would
    // Hidden: what `snob mcp` runs before it asks the person for a yes to a
    // post's likers, so nobody says yes to a read that is then refused
    // (`likers::check_ahead`), as `follow --check-ahead` is for a write.
    #[arg(long, hide = true, conflicts_with = "confirmed_via")]
    pub check_ahead: bool,
}

/// How a post's videos are saved.
#[derive(Args, Debug, Default)]
pub struct VideoArgs {
    /// best: the widest video Instagram has, usually 1080, joined with its
    /// sound by ffmpeg, which must be installed. compat: the app's 720-wide
    /// MP4, which plays anywhere. Unset: best when ffmpeg is found, compat
    /// otherwise
    #[arg(long, value_enum, value_name = "QUALITY")]
    pub quality: Option<Quality>,

    /// The ffmpeg to join a video's tracks with, an absolute path, in place
    /// of SNOB_FFMPEG and the one on PATH
    #[arg(long, value_name = "PATH")]
    pub ffmpeg: Option<std::path::PathBuf>,
}

/// What a video is saved as ([`VideoArgs::quality`]).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Best,
    Compat,
}

#[derive(Args, Debug)]
// What `--mcp-write-cap` needs beside it: a run on an agent's yes, which the
// cap counts toward, or the check made before that yes is asked for.
#[command(group(
    clap::ArgGroup::new("vouched")
        .args(["confirmed_via", "check_ahead"])
        .multiple(true)
))]
pub struct FollowArgs {
    /// The one account to follow or unfollow
    #[arg(value_parser = username)]
    pub target: String,

    #[command(flatten)]
    pub consent: ConsentArgs,

    #[command(flatten)]
    pub output: StatusOutputArgs,

    #[command(flatten)]
    pub agent: AgentArgs,

    /// Refuse when this many writes confirmed through snob mcp were made
    /// in the last 24 hours [default: 10]
    // Hidden: plumbing for `snob mcp`, which passes its
    // `--max-writes-per-day` here. Bounded by what the write bucket lets
    // through in a day anyway, so no number given can promise more.
    #[arg(
        long,
        hide = true,
        requires = "vouched",
        value_name = "N",
        value_parser = clap::value_parser!(u32)
            .range(1..=i64::from(snob_store::store::mcp_acts::MOST_WRITES_A_DAY))
    )]
    pub mcp_write_cap: Option<u32>,

    /// Send nothing and ask nothing: say what would refuse this write now if
    /// an agent made it, or that nothing would
    // Hidden: what `snob mcp` runs before it asks the person for a yes to a
    // write, so nobody says yes to one that is then refused
    // (`follow::check_ahead`). With `--mcp-write-cap`, the cap it counts.
    #[arg(long, hide = true, conflicts_with = "confirmed_via")]
    pub check_ahead: bool,
}

/// What a run of a command an agent was told yes to through `snob mcp`
/// records about that yes (`snob_store::store::mcp_acts`), so that the person
/// can see in `snob status` what was done in their name, and an agent's
/// writes stay a few a day.
///
/// **Hidden: plumbing for `snob mcp`**, which adds them to a child's command
/// line after the yes it obtained, and after what the yes is bound to was
/// taken (`commands::mcp::tools::Plan::command_line`). A command a person
/// types records nothing and counts toward no cap.
#[derive(Args, Debug, Clone, Default)]
pub struct AgentArgs {
    /// How the yes this run is made on was given, through snob mcp
    // Only with `-y`: the yes the server obtained is the one `-y` gives in
    // advance, and a run that would still ask has nothing to record yet.
    #[arg(long, value_enum, hide = true, requires = "yes", value_name = "HOW")]
    pub confirmed_via: Option<ConfirmedVia>,

    /// The client the yes came through, by its name and version
    #[arg(long, hide = true, requires = "confirmed_via", value_name = "CLIENT")]
    pub confirmed_for: Option<String>,
}

/// How `snob mcp` obtained a yes (`commands::mcp`, `consent`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmedVia {
    /// A form the client showed, answered in the call (2026-07-28 and later).
    Form,
    /// A form the client showed, asked with `elicitation/create` while the
    /// call waited (the older revisions).
    Elicitation,
    /// A token the agent handed back after asking in the chat.
    Chat,
}

impl ConfirmedVia {
    /// The token stored and printed, which is also the flag's value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Elicitation => "elicitation",
            Self::Chat => "chat",
        }
    }
}

/// `snob mcp`: serve, or print what a client needs to start it.
#[cfg(feature = "mcp")]
#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: Option<McpCommand>,

    /// Offer follow and unfollow as well, each only after a yes for that call,
    /// asked in the client or by the agent in the chat
    #[arg(long)]
    pub allow_writes: bool,

    /// What a yes the agent relays from the chat may confirm, for a client
    /// that cannot show a form: none, reading other accounts' lists, or that
    #[cfg_attr(
        feature = "mcp-http",
        doc = " and the writes too [default: all; lists with --listen]"
    )]
    #[cfg_attr(not(feature = "mcp-http"), doc = " and the writes too [default: all]")]
    //
    // An `Option` rather than a default value, so that whoever reads it can
    // tell "not given" from "all": over HTTP the default is narrower
    // (`commands::mcp::http`), and one value for both would hide that the
    // choice was never made.
    #[arg(long, value_enum, value_name = "WHAT")]
    pub confirm_in_chat: Option<ConfirmInChat>,

    /// Serve over HTTP on this address of this machine's loopback, such as
    /// 127.0.0.1:8750, or on Linux and macOS on "socket", a socket in snob's
    /// data directory only you can reach, for a tunnel of your own; clients
    /// present the key "snob mcp key new" makes
    #[cfg(feature = "mcp-http")]
    #[arg(
        long,
        value_name = "ADDRESS",
        value_parser = crate::commands::mcp::http::listen_at
    )]
    pub listen: Option<crate::commands::mcp::http::ListenAt>,

    /// With --listen, the https address the tunnel serves this at, so that
    /// requests addressed to it are taken
    #[cfg(feature = "mcp-http")]
    #[arg(
        long,
        requires = "listen",
        value_name = "URL",
        value_parser = crate::commands::mcp::http::public_url
    )]
    pub public_url: Option<url::Url>,

    /// The follows and unfollows agents may make in any 24 hours on the
    /// account, with --allow-writes [default: 10; at most 99, what the write
    /// budget allows in a day anyway]
    #[arg(
        long,
        requires = "allow_writes",
        value_name = "N",
        value_parser = clap::value_parser!(u32)
            .range(1..=i64::from(snob_store::store::mcp_acts::MOST_WRITES_A_DAY))
    )]
    pub max_writes_per_day: Option<u32>,
}

/// What `snob mcp` takes a yes in the chat for (`commands::mcp`, `consent`).
#[cfg(feature = "mcp")]
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmInChat {
    /// Nothing: a call that needs a yes needs a client that can show a form.
    None,
    /// Walking another account's lists, and not the writes.
    Lists,
    /// The walks and the writes.
    All,
}

#[cfg(feature = "mcp")]
#[derive(Subcommand, Debug)]
pub enum McpCommand {
    /// Print what to add to a client's configuration to start this server,
    /// or to reach one already serving over HTTP
    #[command(
        after_help = "Printed, never written: the snippet goes to standard output, and \
                      where it goes to standard error.\n\n\
                      --remote <URL> configures the client to reach a \"snob mcp --listen\" \
                      already running, at the https address your tunnel serves it at (or \
                      http on this machine's loopback, for ssh -L), instead of starting one. \
                      The snippet holds no key and starts nothing: the client sends the key \
                      from SNOB_MCP_TOKEN in its own environment, which you set to what \
                      \"snob mcp key show --reveal\" prints on the server's machine. Never \
                      set SNOB_MCP_KEY for a client: that is the server's, and it replaces \
                      the key of any snob started where it is set. The server's flags \
                      (--allow-writes, --confirm-in-chat, --max-writes-per-day) go on its own \
                      command line, and are refused here with --remote. Claude Desktop and \
                      claude.ai take a remote server as a custom connector in their \
                      settings, so for them only the address is printed."
    )]
    Config(McpConfigArgs),

    /// Keep the key a client presents to reach this server over the
    /// network, in the system keyring
    #[command(
        subcommand,
        after_help = "One key, kept in the system keyring under its own entry, whatever \
                      --no-keyring says: there is no file form. On a machine with no keyring \
                      (a server, a container), set SNOB_MCP_KEY in the server's environment \
                      instead, to a key \"snob mcp key new\" printed; it wins over the \
                      keyring wherever it is set, so set it only where the server runs. \
                      Only a key in snob's own form is taken: snob_mcp_ and 43 characters, \
                      256 random bits.\n\n\
                      The key is printed only to a terminal, unless --print says otherwise, \
                      so that a program running this does not keep it in what it reads. \
                      \"snob mcp key new --force\" replaces it, and every client given the \
                      old one stops working; \"snob mcp key revoke\" removes it. A server \
                      already running keeps the key it started with until it is restarted. \
                      \"snob purge\" removes it too."
    )]
    #[cfg(feature = "mcp-http")]
    Key(McpKeyCommand),
}

/// `snob mcp key` (`commands::mcp::key`).
#[cfg(feature = "mcp-http")]
#[derive(Subcommand, Debug)]
pub enum McpKeyCommand {
    /// Make a key, keep it in the system keyring, and print it this once
    New(McpKeyNewArgs),
    /// Say where the key is read from, and its fingerprint
    Show(McpKeyShowArgs),
    /// Remove the key from the system keyring
    Revoke,
}

#[cfg(feature = "mcp-http")]
#[derive(Args, Debug)]
pub struct McpKeyNewArgs {
    /// Replace the key there is; every client given it stops working
    #[arg(long)]
    pub force: bool,

    /// Print the key even when standard output is not a terminal
    #[arg(long)]
    pub print: bool,

    /// Make a key for SNOB_MCP_KEY: printed, kept nowhere, and the system
    /// keyring left as it is
    #[arg(long, conflicts_with = "force")]
    pub env: bool,
}

#[cfg(feature = "mcp-http")]
#[derive(Args, Debug)]
pub struct McpKeyShowArgs {
    /// Print the key itself, on a terminal
    #[arg(long)]
    pub reveal: bool,

    /// With --reveal, print it even when standard output is not a terminal
    #[arg(long, requires = "reveal")]
    pub print: bool,
}

#[cfg(feature = "mcp")]
#[derive(Args, Debug)]
pub struct McpConfigArgs {
    /// The client that will start it
    #[arg(value_enum)]
    pub client: McpClient,

    /// Start it with --allow-writes
    #[arg(long)]
    pub allow_writes: bool,

    /// Start it with --confirm-in-chat
    #[arg(long, value_enum, value_name = "WHAT")]
    pub confirm_in_chat: Option<ConfirmInChat>,

    /// Reach a server already serving over HTTP at this address rather
    /// than start one; the client sends the key from SNOB_MCP_TOKEN
    //
    // The server's flags describe a process this does not start, so they
    // are refused rather than printed somewhere they do nothing.
    #[arg(
        long,
        value_name = "URL",
        value_parser = crate::commands::mcp::config::RemoteUrl,
        conflicts_with_all = ["allow_writes", "confirm_in_chat", "max_writes_per_day"]
    )]
    pub remote: Option<url::Url>,

    /// Start it with --max-writes-per-day
    #[arg(
        long,
        requires = "allow_writes",
        value_name = "N",
        value_parser = clap::value_parser!(u32)
            .range(1..=i64::from(snob_store::store::mcp_acts::MOST_WRITES_A_DAY))
    )]
    pub max_writes_per_day: Option<u32>,
}

#[cfg(feature = "mcp")]
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpClient {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    Cursor,
    Vscode,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
    Verified,
    Private,
    NoPfp,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Ndjson,
    Csv,
    Xlsx,
    Md,
}

/// The formats a story listing actually has.
///
/// A narrower enum rather than [`Format`] with three values quietly ignored.
/// `--format xlsx` on a list of five stories would have been accepted, printed
/// a table, and left the user believing they had a spreadsheet somewhere. The
/// three that are missing are the ones that exist to hand a **list of accounts**
/// to something else — a column of usernames — and a story has no username in
/// it. Adding them would mean deciding what a spreadsheet of five expiring
/// links is for, and nobody has asked.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoryFormat {
    Table,
    Json,
    Ndjson,
}

/// The formats a profile has: a card to read, an object to parse, a
/// document to keep. The row formats are for a list of accounts, and a
/// profile is not one — the same reasoning as [`StoryFormat`].
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileFormat {
    Table,
    Json,
    Md,
}

impl From<StoryFormat> for Format {
    fn from(story: StoryFormat) -> Self {
        match story {
            StoryFormat::Table => Self::Table,
            StoryFormat::Json => Self::Json,
            StoryFormat::Ndjson => Self::Ndjson,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// What `snob <sub> --help` prints.
    fn long_help(sub: &str) -> String {
        let mut command = Cli::command();
        let sub = command.find_subcommand_mut(sub).expect("a subcommand");
        sub.render_long_help().to_string()
    }

    /// The help says where a User-Agent given at login goes, and that the
    /// browser's requests are not among them.
    #[test]
    fn a_pinned_user_agent_is_said_to_leave_the_browser_alone() {
        let help = long_help("login");
        for said in [
            "for requests sent without the browser",
            "(SNOB_NO_BROWSER) and the media downloads carry it",
            "the browser's own User-Agent, whatever is given here",
        ] {
            assert!(help.contains(said), "{said}: {help}");
        }
    }

    /// Catches `conflicts_with` pointing at a name that does not exist, and
    /// other definition slips that would otherwise only show at runtime.
    #[test]
    fn the_cli_definition_is_coherent() {
        Cli::command().debug_assert();
    }

    /// The sandbox flag cannot be given without the sandbox.
    ///
    /// This is the whole safety argument for `--ig-base-url` existing at all,
    /// and it is enforced by clap rather than described in a comment: a
    /// redirected client can only ever carry a session out of a store inside
    /// the sandbox root, so the session belonging to the person running this is
    /// not reachable from a redirected run. Alone, the flag would be a way to
    /// send a live session cookie to somebody else's server.
    ///
    /// A testing build only. In a released one neither flag exists, which
    /// `crates/snob-core/tests/sandbox.rs` reads the source to hold down.
    #[cfg(feature = "testing")]
    #[test]
    fn a_redirected_run_cannot_reach_the_real_session() {
        let refused =
            Cli::try_parse_from(["snob", "--ig-base-url", "http://127.0.0.1:9/", "whoami"]);
        assert!(
            refused.is_err(),
            "a base URL with no sandbox root would carry the stored session there"
        );

        let tmp = std::env::temp_dir();
        let paired = Cli::try_parse_from([
            "snob",
            "--sandbox-root",
            tmp.to_str().expect("the temporary directory has a name"),
            "--ig-base-url",
            "http://127.0.0.1:9/",
            "whoami",
        ])
        .expect("the pair is what a sandbox run is");
        assert_eq!(
            paired.ig_base_url.map(|u| u.to_string()).as_deref(),
            Some("http://127.0.0.1:9/")
        );
        assert_eq!(paired.sandbox_root.as_deref(), Some(tmp.as_path()));

        // And a sandbox root on its own is fine: it is what drives everything
        // that does not need Instagram at all.
        assert!(
            Cli::try_parse_from([
                "snob",
                "--sandbox-root",
                tmp.to_str().expect("the temporary directory has a name"),
                "watch",
                "status",
            ])
            .is_ok()
        );
    }

    /// The one claim in the help that was not true, kept out.
    ///
    /// `snob watch check` charges 1 + N against the same GCRA budget the walks
    /// draw on -- and `clear_to_send` charges at **reservation, before** the
    /// owed sleep, so a probe that times out and is killed has already spent a
    /// slot for a request that never went out. A one-minute blackbox probe on
    /// two accounts is 4320 requests a day against a sustained ceiling of 2000;
    /// once that is drained the budget owes about 43 seconds a request, which
    /// is past every probe timeout. So the probe reports the monitor broken
    /// while it is fine, and the budget it drained is the one the walk needed:
    /// a probe that causes the condition it detects.
    ///
    /// The per-invocation cost is stated; this keeps the safety claim from
    /// coming back the next time somebody tidies the paragraph.
    #[test]
    fn check_does_not_advertise_itself_as_free_to_poll() {
        let watch = Cli::command()
            .find_subcommand("watch")
            .expect("watch is a subcommand")
            .clone();
        let help = watch
            .find_subcommand("check")
            .expect("check is a subcommand of watch")
            .get_after_help()
            .expect("check has an after_help")
            .to_string();

        assert!(
            !help.contains("as often as you like"),
            "every invocation is charged to the budget the walks need: {help}"
        );
        assert!(
            help.contains("hourly"),
            "and the help has to name an interval instead of taking it back: {help}"
        );
    }

    /// What this wrapper actually adds: the error carries the text somebody
    /// typed, so clap can say which value it was complaining about. The parsing
    /// itself is `snob_core::duration`'s, and tested there.
    #[test]
    fn a_duration_that_will_not_parse_is_refused_by_name() {
        assert_eq!(
            duration("6h").unwrap(),
            std::time::Duration::from_secs(21_600)
        );
        assert!(duration("six hours").is_err());
    }

    /// The cap on an agent's writes means something only on a run an agent
    /// was told yes to, or on the check made before that yes is asked for,
    /// and the check is never a run on a yes.
    #[test]
    fn the_write_cap_needs_an_agents_yes_or_the_check_ahead() {
        let parses = |extra: &[&str]| {
            let line = ["snob", "follow", "someone", "-y", "--json"];
            Cli::try_parse_from(line.iter().chain(extra)).is_ok()
        };
        assert!(parses(&["--confirmed-via=chat", "--mcp-write-cap=3"]));
        assert!(parses(&["--check-ahead", "--mcp-write-cap=3"]));
        assert!(parses(&["--check-ahead"]));
        assert!(!parses(&["--mcp-write-cap=3"]));
        assert!(!parses(&["--check-ahead", "--confirmed-via=chat"]));
    }

    #[test]
    fn offline_and_refresh_are_mutually_exclusive() {
        let result = Cli::try_parse_from(["snob", "followers", "--offline", "--refresh"]);
        assert!(result.is_err());
    }

    /// `-i` on a list command withholds the printed listing, so it is refused
    /// beside anything that names the printed or written form — accepted and
    /// ignored is the shape the format enums exist to refuse. It composes
    /// with everything that narrows the *result*: the browser shows what the
    /// listing would have shown.
    #[test]
    fn the_list_browser_conflicts_with_the_printed_forms_and_composes_with_the_rest() {
        for command in ["unfollowers", "fans", "friends", "followers", "following"] {
            assert!(
                Cli::try_parse_from(["snob", command, "-i"]).is_ok(),
                "{command} takes -i"
            );
            for said_a_form in [vec!["--format", "json"], vec!["-o", "list.csv"]] {
                let mut line = vec!["snob", command, "-i"];
                line.extend(said_a_form.iter());
                assert!(
                    Cli::try_parse_from(&line).is_err(),
                    "{command} -i beside {said_a_form:?} would be accepted and ignored"
                );
            }
        }
        assert!(
            Cli::try_parse_from(["snob", "unfollowers", "-i", "--limit", "5", "--offline"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["snob", "scan", "-i"]).is_ok(),
            "scan browses its five lists as a tray"
        );
        assert!(Cli::try_parse_from(["snob", "scan", "-i", "--format", "json"]).is_err());

        // The two spellings contradict each other and clap says so; beside
        // the flags that already print, --no-interactive is redundant and
        // allowed, so a defensive script survives.
        assert!(Cli::try_parse_from(["snob", "unfollowers", "--no-interactive", "-i"]).is_err());
        assert!(
            Cli::try_parse_from([
                "snob",
                "unfollowers",
                "--no-interactive",
                "--format",
                "json"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["snob", "scan", "--no-interactive"]).is_ok());
    }

    /// The list commands' half of the take-over matrix, the same shape as
    /// the media one below: what was said beats what was detected, and only
    /// a run that asked for nothing at all listens to the detection bit.
    #[test]
    fn the_list_browser_is_the_default_only_when_nothing_else_was_asked_for() {
        let browse = |line: &[&str]| {
            let Command::Unfollowers(args) = Cli::try_parse_from(line).unwrap().command else {
                panic!("unfollowers");
            };
            let printed = args.output.format.is_some() || args.output.path.is_some();
            (args.browse, printed)
        };

        // Nothing asked for: the detection bit decides.
        let (bare, printed) = browse(&["snob", "unfollowers"]);
        assert!(bare.browses(printed, true));
        assert!(!bare.browses(printed, false), "a pipe never gets a browser");

        // Forced on: a terminal that cannot browse is an error, not a print.
        let (forced, printed) = browse(&["snob", "unfollowers", "-i"]);
        assert!(forced.browses(printed, false));

        // Every explicit route to the printed form wins over an attending
        // terminal. (`-i` beside these is a clap conflict, tested above.)
        for line in [
            &["snob", "unfollowers", "--no-interactive"][..],
            &["snob", "unfollowers", "--format", "json"][..],
            &["snob", "unfollowers", "-o", "list.csv"][..],
        ] {
            let (static_asked, printed) = browse(line);
            assert!(
                !static_asked.browses(printed, true),
                "{line:?} must not open the browser"
            );
        }
    }

    /// `profile` and `pfp` carry their own browse pair — their `-o`/`--format`
    /// ids differ from the lists', so [`BrowseArgs`] cannot be flattened in —
    /// but the decision they feed is the shared one, and this is its matrix.
    #[test]
    fn profile_and_pfp_follow_the_same_browse_rule() {
        // Said beats detected, and only a bare run listens to detection.
        assert!(
            browse_decision(true, false, false, false),
            "-i wins outright"
        );
        assert!(!browse_decision(false, true, false, true));
        assert!(!browse_decision(false, false, true, true));
        assert!(browse_decision(false, false, false, true));
        assert!(!browse_decision(false, false, false, false));

        let profile = |line: &[&str]| {
            let Command::Profile(args) = Cli::try_parse_from(line).unwrap().command else {
                panic!("profile");
            };
            args
        };
        assert!(profile(&["snob", "profile"]).browses(true));
        assert!(!profile(&["snob", "profile"]).browses(false));
        assert!(profile(&["snob", "profile", "-i"]).browses(false));
        assert!(!profile(&["snob", "profile", "--no-interactive"]).browses(true));
        assert!(!profile(&["snob", "profile", "--format", "json"]).browses(true));
        assert!(!profile(&["snob", "profile", "-o", "out.md"]).browses(true));

        let pfp = |line: &[&str]| {
            let Command::Pfp(args) = Cli::try_parse_from(line).unwrap().command else {
                panic!("pfp");
            };
            args
        };
        assert!(pfp(&["snob", "pfp", "x"]).browses(true));
        assert!(!pfp(&["snob", "pfp", "x"]).browses(false));
        assert!(pfp(&["snob", "pfp", "x", "-i"]).browses(false));
        assert!(!pfp(&["snob", "pfp", "x", "--no-interactive"]).browses(true));
        assert!(!pfp(&["snob", "pfp", "x", "-o", "face.jpg"]).browses(true));

        // The conflicts: -i beside a printed or written form is refused, the
        // two spellings contradict each other, and --no-interactive beside a
        // form that already prints stays redundant-and-allowed.
        assert!(Cli::try_parse_from(["snob", "profile", "-i", "--format", "json"]).is_err());
        assert!(Cli::try_parse_from(["snob", "profile", "-i", "-o", "out.md"]).is_err());
        assert!(Cli::try_parse_from(["snob", "profile", "-i", "--no-interactive"]).is_err());
        assert!(
            Cli::try_parse_from(["snob", "profile", "--no-interactive", "--format", "json"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["snob", "pfp", "x", "-i", "-o", "f.jpg"]).is_err());
        assert!(Cli::try_parse_from(["snob", "pfp", "x", "-i", "--no-interactive"]).is_err());
        assert!(
            Cli::try_parse_from(["snob", "pfp", "x", "--no-interactive", "-o", "f.jpg"]).is_ok()
        );
        // The media browsers save where they were started, so a destination
        // beside -i would be accepted and ignored.
        for command in ["stories", "highlights"] {
            assert!(Cli::try_parse_from(["snob", command, "x", "-i", "-o", "dir"]).is_err());
            assert!(Cli::try_parse_from(["snob", command, "x", "-d", "1", "-o", "dir"]).is_ok());
        }
    }

    #[test]
    fn import_dyi_takes_a_path() {
        let cli = Cli::try_parse_from(["snob", "import", "dyi", "export.zip"]).unwrap();
        let Command::Import(crate::commands::import::ImportCommand::Dyi { path }) = cli.command
        else {
            panic!("import dyi");
        };
        assert_eq!(path, std::path::PathBuf::from("export.zip"));
    }

    /// No flags means the one default age, the same one the profile browser
    /// and the monitor use; the clap default is a string and can drift.
    #[test]
    fn the_default_max_age_is_the_engines() {
        let cli = Cli::try_parse_from(["snob", "followers"]).unwrap();
        let Command::Followers(args) = cli.command else {
            panic!("followers");
        };
        assert_eq!(args.walk.max_age, crate::engine::DEFAULT_MAX_AGE);
        assert_eq!(WalkArgs::default().max_age, crate::engine::DEFAULT_MAX_AGE);
    }

    /// `--cache` is the earlier spelling; a script written against it still
    /// runs, and the conflicts follow the alias to the same argument.
    #[test]
    fn the_old_cache_spelling_still_parses_and_is_not_advertised() {
        let cli = Cli::try_parse_from(["snob", "followers", "--cache"]).unwrap();
        let Command::Followers(args) = cli.command else {
            panic!("followers");
        };
        assert!(args.walk.offline);
        assert!(Cli::try_parse_from(["snob", "followers", "--cache", "--refresh"]).is_err());

        let help = long_help("followers");
        assert!(!help.contains("--cache"), "{help}");
        assert!(help.contains("--offline"), "{help}");
    }

    /// `--offline` makes no walk, so a flag that shapes one is refused rather
    /// than accepted and ignored.
    #[test]
    fn a_flag_that_shapes_a_walk_is_refused_with_offline() {
        for flag in [["--max-pages", "2"], ["--no-resume", ""]] {
            let mut line = vec!["snob", "followers", "--offline", flag[0]];
            if !flag[1].is_empty() {
                line.push(flag[1]);
            }
            assert!(
                Cli::try_parse_from(&line).is_err(),
                "{} was accepted alongside --offline",
                flag[0]
            );
        }
    }

    /// `scan` prints counts, so it has no rows for `--limit` to cut and does
    /// not take it.
    #[test]
    fn scan_takes_the_list_options_but_not_the_cap() {
        assert!(Cli::try_parse_from(["snob", "scan", "--limit", "5"]).is_err());
        let cli = Cli::try_parse_from([
            "snob",
            "scan",
            "someone",
            "--hide",
            "verified",
            "--format",
            "json",
            "--offline",
            "-y",
        ])
        .unwrap();
        let Command::Scan(args) = cli.command else {
            panic!("scan");
        };
        assert_eq!(args.target.as_deref(), Some("someone"));
        assert_eq!(args.filter.hide, vec![Attr::Verified]);
        assert_eq!(args.output.format, Some(Format::Json));
        assert!(args.walk.offline && args.walk.consent.yes);
    }

    /// `snob mcp --help` tells of `--listen` and the key only in a build that
    /// has them, and `snob profile --help` of the rest it may wait first.
    #[test]
    fn the_help_tells_only_of_what_the_build_has() {
        let mut cli = Cli::command();
        cli.build();
        #[cfg(feature = "mcp")]
        {
            let mcp = cli.find_subcommand_mut("mcp").unwrap();
            let help = mcp.render_long_help().to_string();
            assert_eq!(
                help.contains("--listen"),
                cfg!(feature = "mcp-http"),
                "{help}"
            );
            assert_eq!(
                help.contains("snob mcp key new"),
                cfg!(feature = "mcp-http"),
                "{help}"
            );
            assert!(help.contains("Started by the agent's client"), "{help}");
        }
        let profile = cli.find_subcommand_mut("profile").unwrap();
        let help = profile.render_long_help().to_string();
        assert!(help.contains("up to 15 minutes"), "{help}");
        assert!(!help.contains("three or four"), "{help}");
    }

    /// The environment variables the program answers to are announced in one
    /// place, under the examples, where exit codes already live.
    ///
    /// Four of them have no flag whose help would mention them: three belong to
    /// the crates behind the styling (`console`, `supports-hyperlinks`) and one
    /// to `main::init_tracing`. This pins
    /// the list. `SNOB_IGNORE_COOLDOWN` is deliberately absent -- its own
    /// doc-comment in `rate_budget` says why it is not advertised -- and the
    /// assertion holds that down too.
    #[test]
    fn the_environment_variables_are_announced_together() {
        for name in [
            "NO_COLOR",
            "CLICOLOR_FORCE",
            "FORCE_HYPERLINK",
            "SNOB_ACCOUNT",
            "SNOB_LOG",
            "SNOB_NO_BROWSER",
            "SNOB_NO_OWNER",
            "SNOB_CSRFTOKEN",
            "SNOB_SIGNING_KEY",
            #[cfg(feature = "mcp-http")]
            "SNOB_MCP_KEY",
        ] {
            assert!(EXAMPLES.contains(name), "{name} is read but not announced");
        }
        // And the other way: a build that does not read the MCP key does not
        // announce it.
        assert_eq!(
            EXAMPLES.contains("SNOB_MCP_KEY"),
            cfg!(feature = "mcp-http"),
            "SNOB_MCP_KEY is announced only where it is read"
        );
        assert!(
            !EXAMPLES.contains("SNOB_IGNORE_COOLDOWN"),
            "the escape hatch is deliberately not advertised"
        );
    }

    /// The account `use` takes is its own argument, and the global flag
    /// still parses beside it.
    #[test]
    fn account_takes_list_and_use() {
        let cli = Cli::try_parse_from(["snob", "account", "list", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Account(AccountCommand::List(AccountListArgs { output })) if output.json
        ));

        let cli = Cli::try_parse_from(["snob", "--account", "a", "account", "use", "b"]).unwrap();
        assert_eq!(cli.account.as_deref(), Some("a"));
        let Command::Account(AccountCommand::Use(args)) = cli.command else {
            panic!("expected account use");
        };
        assert_eq!(args.name, "b");

        assert!(Cli::try_parse_from(["snob", "account", "use"]).is_err());
        assert!(Cli::try_parse_from(["snob", "account"]).is_err());
    }

    /// Adding an account and logging every account out name no account, so
    /// naming one beside them is refused, on either side of the command.
    #[test]
    fn add_and_all_refuse_a_named_account() {
        let checked = |args: &[&str]| {
            Cli::try_parse_from(args)
                .unwrap()
                .refuse_a_named_account()
                .map_err(|e| e.kind())
        };
        for args in [["login", "--add"], ["logout", "--all"]] {
            assert_eq!(checked(&["snob", args[0], args[1]]), Ok(()));
            for named in [
                ["snob", "--account", "a", args[0], args[1]],
                ["snob", args[0], args[1], "--account", "a"],
            ] {
                assert_eq!(
                    checked(&named),
                    Err(clap::error::ErrorKind::ArgumentConflict),
                    "{named:?}"
                );
            }
        }
        assert_eq!(checked(&["snob", "--account", "a", "login"]), Ok(()));
    }

    /// Hidden from the help, still accepted: the first release documented it.
    #[test]
    fn no_verified_still_parses_and_is_not_advertised() {
        // A positional target next to the shared flags, too.
        let cli =
            Cli::try_parse_from(["snob", "unfollowers", "@someone", "--no-verified"]).unwrap();
        let Command::Unfollowers(args) = cli.command else {
            panic!("unfollowers");
        };
        assert_eq!(args.target.as_deref(), Some("@someone"));
        assert!(args.filter.no_verified);
        let cli = Cli::try_parse_from(["snob", "unfollowers", "--no-verified"]).unwrap();
        let Command::Unfollowers(args) = cli.command else {
            panic!("unfollowers");
        };
        assert!(args.filter.no_verified);

        let help = long_help("unfollowers");
        assert!(!help.contains("--no-verified"), "{help}");
        assert!(help.contains("--hide"), "{help}");
    }

    /// A story listing's format has nothing to say about a download or the
    /// browser, so it is refused next to both rather than accepted and ignored.
    #[test]
    fn a_story_format_only_goes_with_the_listing() {
        for extra in [["-d", "1"], ["--all", ""], ["-i", ""]] {
            let mut line = vec!["snob", "stories", "someone", "--format", "json", extra[0]];
            if !extra[1].is_empty() {
                line.push(extra[1]);
            }
            assert!(
                Cli::try_parse_from(&line).is_err(),
                "--format was accepted alongside {}",
                extra[0]
            );
        }
        assert!(Cli::try_parse_from(["snob", "stories", "someone", "--format", "json"]).is_ok());
    }

    /// `-d` takes the listing's numbers in every shape the help promises, and
    /// refuses the shapes that would only fail later, at the parser -- exit 2,
    /// nothing fetched.
    #[test]
    fn a_download_selection_parses_numbers_ranges_and_all() {
        use DownloadSelection::{All, These};

        let selected = |line: &[&str]| {
            let cli = Cli::try_parse_from(line).unwrap();
            let Command::Stories(args) = cli.command else {
                panic!("stories");
            };
            args.action.selection()
        };

        assert_eq!(
            selected(&["snob", "stories", "x", "-d", "3"]),
            Some(These(vec![3]))
        );
        assert_eq!(
            selected(&["snob", "stories", "x", "-d", "1,3"]),
            Some(These(vec![1, 3]))
        );
        assert_eq!(
            selected(&["snob", "stories", "x", "-d", "2-4"]),
            Some(These(vec![2, 3, 4]))
        );
        // A duplicate is asked for once: 2-4 already brought 3.
        assert_eq!(
            selected(&["snob", "stories", "x", "-d", "2-4,3,1"]),
            Some(These(vec![2, 3, 4, 1]))
        );
        assert_eq!(selected(&["snob", "stories", "x", "-d", "all"]), Some(All));
        // The first release's spelling still works, hidden, and folds into
        // the same selection.
        assert_eq!(selected(&["snob", "stories", "x", "--all"]), Some(All));
        assert_eq!(selected(&["snob", "stories", "x"]), None);

        for bad in ["0", "3-1", "one", "1,,2", "1-999999999"] {
            assert!(
                Cli::try_parse_from(["snob", "stories", "x", "-d", bad]).is_err(),
                "{bad} should have been refused at the parser"
            );
        }
        // The two spellings of "everything" cannot be combined with each
        // other or with the browser.
        assert!(Cli::try_parse_from(["snob", "stories", "x", "-d", "all", "--all"]).is_err());
        assert!(Cli::try_parse_from(["snob", "stories", "x", "-d", "1", "-i"]).is_err());
        assert!(Cli::try_parse_from(["snob", "stories", "x", "--all", "-i"]).is_err());

        // Hidden means hidden: the help teaches -d, not --all.
        let help = long_help("stories");
        assert!(!help.contains("--all"), "{help}");
        assert!(help.contains("--download"), "{help}");
    }

    /// The whole matrix of "does this run take the terminal over".
    ///
    /// What was said beats what was detected, in every combination: `-i`
    /// wins outright, every static flag refuses, and only a run that asked
    /// for nothing at all listens to the detection bit.
    #[test]
    fn the_browser_is_the_default_only_when_nothing_else_was_asked_for() {
        let action = |line: &[&str]| {
            let Command::Stories(args) = Cli::try_parse_from(line).unwrap().command else {
                panic!("stories");
            };
            (args.action, args.list.format.is_some())
        };

        // Nothing asked for: the detection bit decides.
        let (bare, fmt) = action(&["snob", "stories", "x"]);
        assert!(bare.browses(fmt, true));
        assert!(!bare.browses(fmt, false), "a pipe never gets a browser");

        // Forced on: a terminal that cannot browse is an error, not a print,
        // so the answer stays true whatever was detected.
        let (forced, fmt) = action(&["snob", "stories", "x", "-i"]);
        assert!(forced.browses(fmt, false));

        // Every explicit route to the static forms wins over an attending
        // terminal.
        for line in [
            &["snob", "stories", "x", "--no-interactive"][..],
            &["snob", "stories", "x", "-d", "2"][..],
            &["snob", "stories", "x", "--all"][..],
            &["snob", "stories", "x", "--format", "json"][..],
            &["snob", "stories", "x", "-o", "somewhere"][..],
        ] {
            let (static_asked, fmt) = action(line);
            assert!(
                !static_asked.browses(fmt, true),
                "{line:?} must not open the browser"
            );
        }

        // The two spellings contradict each other and clap says so; beside
        // the flags that already print, --no-interactive is redundant and
        // allowed, so a defensive script survives growing a download.
        assert!(Cli::try_parse_from(["snob", "stories", "x", "--no-interactive", "-i"]).is_err());
        assert!(
            Cli::try_parse_from(["snob", "stories", "x", "--no-interactive", "-d", "1"]).is_ok()
        );
        assert!(Cli::try_parse_from(["snob", "highlights", "x", "2", "--no-interactive"]).is_ok());
    }

    /// The `highlights` positionals: an account, then a number naming one
    /// entry of its tray -- AGENTS.md's "a tray level and an items level".
    /// With the number, `-d`, `-o`, `-i` act on the items exactly as `stories`
    /// has them; without it `-d` takes whole entries.
    #[test]
    fn highlights_takes_an_account_and_then_a_number() {
        let cli = Cli::try_parse_from(["snob", "highlights", "someone"]).expect("tray listing");
        let Command::Highlights(args) = cli.command else {
            panic!("highlights");
        };
        assert_eq!(args.target.as_deref(), Some("someone"));
        assert_eq!(args.highlight, None);

        let cli =
            Cli::try_parse_from(["snob", "highlights", "someone", "2", "-d", "3"]).expect("item");
        let Command::Highlights(args) = cli.command else {
            panic!("highlights");
        };
        assert_eq!(args.highlight, Some(2));
        assert_eq!(
            args.action.selection(),
            Some(DownloadSelection::These(vec![3]))
        );

        // No account at all is the viewer's own tray.
        let cli = Cli::try_parse_from(["snob", "highlights"]).expect("own tray");
        let Command::Highlights(args) = cli.command else {
            panic!("highlights");
        };
        assert_eq!(args.target, None);

        // The listing starts at 1, and the parser is where zero stops.
        assert!(Cli::try_parse_from(["snob", "highlights", "someone", "0"]).is_err());

        // The same conflicts as stories: one action per run.
        assert!(Cli::try_parse_from(["snob", "highlights", "x", "2", "-d", "1", "-i"]).is_err());
        assert!(
            Cli::try_parse_from(["snob", "highlights", "x", "--format", "json", "-d", "1"])
                .is_err()
        );
    }

    /// `--tls-extra-root` only means something alongside `--strict-roots`.
    ///
    /// On the platform store there is nothing to add to: whatever an
    /// administrator installed is already trusted, so a lone `--tls-extra-root`
    /// would be a flag that reads a file and changes nothing. Enforced by clap
    /// rather than described, the same way `--ig-base-url` is paired with
    /// `--sandbox-root`.
    #[test]
    fn an_extra_root_needs_the_narrowing_it_widens() {
        let alone = Cli::try_parse_from(["snob", "--tls-extra-root", "ca.pem", "whoami"]);
        assert!(alone.is_err(), "it was accepted on its own");

        let paired = Cli::try_parse_from([
            "snob",
            "--strict-roots",
            "--tls-extra-root",
            "ca.pem",
            "whoami",
        ])
        .expect("together they are the way out of the narrowing");
        assert!(paired.strict_roots);
        assert_eq!(paired.tls_extra_root.len(), 1);

        // And narrowing on its own is the ordinary case.
        let narrow = Cli::try_parse_from(["snob", "--strict-roots", "whoami"]).unwrap();
        assert!(narrow.strict_roots);
        assert!(narrow.tls_extra_root.is_empty());

        // Off unless asked, which is the default the audit settled on.
        let plain = Cli::try_parse_from(["snob", "whoami"]).unwrap();
        assert!(!plain.strict_roots);
    }

    #[test]
    fn changes_takes_a_date_or_a_duration_and_not_both_ways_of_choosing() {
        let Command::Changes(by_day) =
            Cli::try_parse_from(["snob", "changes", "--since", "2026-11-01"])
                .unwrap()
                .command
        else {
            panic!("not changes");
        };
        assert!(matches!(by_day.since, Some(Since::Day(_))));
        let Command::Changes(ago) = Cli::try_parse_from(["snob", "changes", "--since", "7d"])
            .unwrap()
            .command
        else {
            panic!("not changes");
        };
        assert_eq!(
            ago.since
                .map(|since| since.at(snob_core::Epoch::new(700_000))),
            Some(snob_core::Epoch::new(700_000 - 7 * 86_400))
        );
        assert!(Cli::try_parse_from(["snob", "changes", "--since", "soon"]).is_err());
        assert!(
            Cli::try_parse_from(["snob", "changes", "--since", "7d", "--from", "3"]).is_err(),
            "a moment and an id are two answers to one question"
        );
    }

    #[test]
    fn a_schedule_flag_the_monitor_could_never_run_on_is_a_command_line_error() {
        for line in [
            ["snob", "watch", "--every", "1m"],
            ["snob", "watch", "--at", "25:00"],
            ["snob", "watch", "--on", "someday"],
            ["snob", "watch", "--cron", "* * *"],
        ] {
            let refused = Cli::try_parse_from(line).expect_err("refused at the parser");
            assert_eq!(refused.exit_code(), 2, "{line:?}");
        }
        assert!(Cli::try_parse_from(["snob", "watch", "--every", "6h", "--at", "09:00"]).is_ok());
        assert!(Cli::try_parse_from(["snob", "watch", "--on", "mon,thu", "--at", "9:00"]).is_ok());
    }
}
