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

    /// Store the session in a protected file instead of the system keyring.
    /// For environments without a desktop session.
    // Ordered last, with `verbose`. Both are global, so without this they are
    // propagated into every subcommand and land in the middle of its own
    // options, splitting a list that reads in a deliberate order.
    #[arg(long, global = true, display_order = 900)]
    pub no_keyring: bool,

    /// Show diagnostic traces
    ///
    /// The browsers' own go to browser-owner.log in the data directory, at the
    /// level of the command that started the process that runs them.
    #[arg(long, global = true, display_order = 901)]
    pub verbose: bool,

    /// Check Instagram's certificate against Mozilla's roots only
    ///
    /// Off by default, and that default is deliberate rather than lazy.
    /// reqwest 0.13 made the platform verifier the default, so snob honors
    /// whatever roots an administrator has installed — which is what makes it
    /// work on a managed machine, and is also how a laptop carrying a
    /// TLS-inspecting root lets that middlebox read the session in transit.
    /// This ends the second at the cost of the first, which is a trade only the
    /// person running it can make.
    ///
    /// It refuses on Windows for ARM64, where the TLS backend is schannel and
    /// has no way to express "these roots and no others". A security flag that
    /// silently does nothing is worse than one that is not offered.
    ///
    /// **Never applied to the webhook.** A private CA in front of somebody's
    /// own receiver is legitimate, and `snob_ig::http::plain` has no argument
    /// for this — the same shape that stops the session reaching a webhook.
    #[arg(long, global = true, display_order = 904)]
    pub strict_roots: bool,

    /// Also trust the certificates in this PEM file
    ///
    /// The way back out of `--strict-roots`, and it requires it: on the
    /// platform store there is nothing to add to, because whatever an
    /// administrator installed is already trusted. Repeatable.
    #[arg(
        long,
        global = true,
        display_order = 905,
        requires = "strict_roots",
        value_name = "PEM"
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

/// Shown under the option list.
///
/// The note about quoting is not decoration. On PowerShell `@` is the splatting
/// operator, so an unquoted `@name` is consumed by the shell and never reaches
/// this program, which then answers about the user's own account instead. It
/// cannot be detected from here, so the only place to say it is the help, and
/// none of the examples above it are written that way.
const EXAMPLES: &str = "\
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
  SNOB_CSRFTOKEN, SNOB_SIGNING_KEY
                    the two secrets a command line would otherwise carry;
                    see \"snob login --help\" and \"snob watch --help\"";

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Store your Instagram session: in the system keyring, or in a file
    /// where there is none
    Login(LoginArgs),

    /// Show which account you are authenticated as
    Whoami(WhoamiArgs),

    /// Where the account stands: session, request budget, cooldown, lists, monitor
    #[command(
        after_help = "Read from what is stored here; nothing is sent and nothing is written. \
                      With no section named, all of them; --budget, --cooldown, --session, \
                      --lists and --watch narrow it to those.\n\n\
                      It exits 5 while the account is in cooldown, whatever sections were \
                      asked for, so \"snob status --budget && snob scan\" runs the scan only \
                      when it may send; 3 with no session; 0 otherwise.\n\n\
                      The budget counts what goes out now without a wait: the pace allows \
                      twenty-one requests in a row, the day about two thousand, and writes \
                      three in a row and then one every fifteen minutes. Lists read at most \
                      2,000 accounts in any 24 hours, 1,000 for a week after a push-back."
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
        after_help = "What you would see opening the profile, for three or four requests: the \
                      counters, the bio, whether you follow each other, the accounts you follow \
                      that follow them, the highlights and whether anything is up right now. It \
                      walks no list and stores nothing.\n\n\
                      From the browser, finding another account's id costs one more until snob \
                      has walked its lists or a list that names it; your own is the session's. A \
                      name that cannot be an Instagram username is not found, for no request. \
                      With SNOB_NO_BROWSER the name is asked about as typed, within the three or \
                      four.\n\n\
                      \"snob scan\" is the crossing, and costs both lists."
    )]
    Profile(ProfileArgs),

    /// Summary of the whole account: followers, following, and how they cross
    Scan(ScanArgs),

    /// Accounts you follow that do not follow you back
    Unfollowers(ListArgs),

    /// Accounts that follow you and you do not follow
    Fans(ListArgs),

    /// Accounts you and they follow each other
    // `mutuals` is the earlier name, kept as a hidden alias so anything written
    // against it still runs rather than failing at the shell.
    #[command(alias = "mutuals")]
    Friends(ListArgs),

    /// Your followers
    Followers(ListArgs),

    /// The accounts you follow
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
                      loaded; each further page of comments is one more. Videos are saved as the app plays them, 720 pixels wide at \
                      most. Listing and downloading a post does not tell the account you looked."
    )]
    Post(PostArgs),

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

    /// Read Instagram's own data export, with no session and no request
    #[command(
        subcommand,
        after_help = "The one way to answer who does not follow you back without anything \
                      talking to Instagram on your behalf: request \"Followers and following\" \
                      as HTML or JSON from Accounts Center, \"Download your information\", and \
                      point this at the zip, or the folder it was extracted into. It reads the \
                      export and prints the answer; nothing is stored and nothing is sent. What \
                      it shows describes the moment Instagram built the export."
    )]
    Import(crate::commands::import::ImportCommand),

    /// The process that runs the browsers for every command (`owner`).
    /// Started by the commands themselves; nobody types it.
    #[command(name = crate::owner::COMMAND, hide = true)]
    BrowserOwner,
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
pub struct StatusArgs {
    /// The session: which account, where it is stored, when it was last checked
    #[arg(long)]
    pub session: bool,

    /// The request budget: what goes out now without a wait, and accounts read today
    #[arg(long)]
    pub budget: bool,

    /// The cooldown: whether one stands, until when and why, and the last one
    #[arg(long)]
    pub cooldown: bool,

    /// Your lists as stored: when each was last walked, and a walk left to resume
    #[arg(long)]
    pub lists: bool,

    /// The monitor's last run on each account it watches from this one
    #[arg(long)]
    pub watch: bool,

    #[command(flatten)]
    pub output: StatusOutputArgs,
}

/// Which parts of `snob status` a run asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSections {
    pub session: bool,
    pub budget: bool,
    pub cooldown: bool,
    pub lists: bool,
    pub watch: bool,
}

impl StatusArgs {
    /// The sections named, or every one when none was.
    pub fn sections(&self) -> StatusSections {
        let named = StatusSections {
            session: self.session,
            budget: self.budget,
            cooldown: self.cooldown,
            lists: self.lists,
            watch: self.watch,
        };
        if named.session || named.budget || named.cooldown || named.lists || named.watch {
            named
        } else {
            StatusSections {
                session: true,
                budget: true,
                cooldown: true,
                lists: true,
                watch: true,
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
    pub target: Option<String>,

    #[command(flatten)]
    pub filter: FilterArgs,

    #[command(flatten)]
    pub output: OutputArgs,

    /// Trim the output to the first N accounts. Saves no requests: --max-pages
    /// is what does that.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    #[command(flatten)]
    pub browse: BrowseArgs,

    #[command(flatten)]
    pub walk: WalkArgs,
}

/// `snob scan`: [`ListArgs`] without `--limit`, because a summary of five
/// counts has no rows to cut.
#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Account to summarize. Defaults to your own.
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

    /// Write the result to a file instead of standard output
    #[arg(short = 'o', long = "output", value_name = "FILE")]
    pub path: Option<PathBuf>,
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
    pub target: Option<String>,

    /// How often to run: 6h, 2d, 2w
    #[arg(long, value_name = "DURATION", value_parser = duration)]
    pub every: Option<std::time::Duration>,

    /// Times of day to run at: 09:00,21:00
    #[arg(long, value_delimiter = ',', value_name = "HH:MM")]
    pub at: Vec<String>,

    /// Days to run on: mon,thu. Defaults to every day.
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "DAYS",
        conflicts_with = "cron"
    )]
    pub on: Vec<String>,

    /// A five-field cron expression, for a schedule you already have written
    #[arg(long, value_name = "EXPR", conflicts_with = "at")]
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

    /// What is configured, when it last ran, and what is still owed
    Status(WatchStatusArgs),
}

#[derive(Args, Debug, Default)]
pub struct WatchCheckArgs {
    #[command(flatten)]
    pub output: StatusOutputArgs,

    /// Do not post anything to the webhook
    #[arg(long)]
    pub no_webhook: bool,
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
    pub target: Option<String>,

    #[command(flatten)]
    pub output: StatusOutputArgs,

    #[command(flatten)]
    pub progress: ProgressArgs,
}

#[derive(Args, Debug)]
pub struct ProfileArgs {
    /// Account to show. Defaults to your own.
    pub target: Option<String>,

    /// Output format. Defaults to a table on a terminal and JSON in a pipe.
    #[arg(long, value_enum)]
    pub format: Option<ProfileFormat>,

    /// Write the result to a file instead of standard output
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
    pub target: String,

    /// Destination file
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
    /// one is.
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
}

#[derive(Args, Debug)]
pub struct PostArgs {
    /// A post's or a reel's link, or the code in it
    #[arg(value_name = "LINK")]
    pub link: String,

    #[command(flatten)]
    pub action: MediaActionArgs,

    #[command(flatten)]
    pub list: MediaListArgs,
}

#[derive(Args, Debug)]
pub struct FollowArgs {
    /// The one account to follow or unfollow
    pub target: String,

    #[command(flatten)]
    pub consent: ConsentArgs,
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
        ] {
            assert!(EXAMPLES.contains(name), "{name} is read but not announced");
        }
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
}
