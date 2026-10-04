use anyhow::Context;
use clap::Parser;
use snob_cli::account::Unresolved;
use snob_cli::cli::{Cli, Command};
use snob_cli::commands;
use snob_cli::commands::sets::SetOp;
use snob_cli::exit::ExitCode;
use snob_core::model::ListKind;
use snob_store::paths::{AccountPaths, AppPaths};
use snob_store::registry::Registry;
use snob_store::secrets::SecretStore;

/// Two workers, always two, whatever the machine has.
///
/// The default is one per core, which is both too many and — on the machines
/// this project explicitly supports — too few. Too many because the program
/// makes one request at a time and spends most of its life asleep between them,
/// so thirty-two worker threads on a thirty-two core desktop are thirty-one
/// doing nothing. Too few because on a one-vCPU server or container the default
/// is **one**, and that is the case that breaks something.
///
/// It breaks Ctrl+C. `tokio::signal::ctrl_c()` permanently disables the process
/// default handler from the first call onwards, so the only thing that can stop
/// a run is the task waiting on that signal. Give it a single worker and let
/// anything block that worker — the consent prompt waiting on a human, a
/// request budget waiting out the busy timeout — and the signal task cannot be
/// scheduled at all. Ctrl+C then does nothing, twice, and the run cannot be
/// stopped. A homelab is a first-class place to run this, and a one-vCPU box is
/// what a homelab is.
///
/// The second worker is what guarantees there is always somewhere for that task
/// to go.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if let Err(refused) = cli.refuse_a_named_account() {
        refused.exit();
    }
    init_tracing(cli.verbose, matches!(cli.command, Command::BrowserOwner));
    restore_terminal_on_panic();

    let wording = wording_for(&cli);
    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            snob_cli::report::print_error(&e, wording);
            snob_cli::exit::exit_code_for(&e)
        }
    };
    std::process::ExitCode::from(snob_cli::exit::code(code))
}

/// Whether a failure is told in JSON: when the answer was going to be.
///
/// The same decision the command makes about its result, made once more
/// here for its failure -- `--format json`, `--json`, an extension that
/// means JSON, or standard output not being a terminal, which is what turns
/// a list into JSON on its own. A program reading one stream should not
/// have to read the other in English.
fn wording_for(cli: &Cli) -> snob_cli::report::Wording {
    use snob_cli::cli::{AccountCommand, Format, WatchCommand};
    use snob_cli::output::effective_format;
    use snob_cli::report::Wording;

    let document = |format: Option<Format>, to: Option<&std::path::Path>| {
        matches!(effective_format(format, to), Format::Json | Format::Ndjson)
    };
    let json = match &cli.command {
        Command::Followers(args)
        | Command::Following(args)
        | Command::Unfollowers(args)
        | Command::Fans(args)
        | Command::Friends(args) => document(args.output.format, args.output.path.as_deref()),
        Command::Scan(args) => document(args.output.format, args.output.path.as_deref()),
        // `-o` is where the listing goes only when listing; with `-d` it is
        // where the download goes, and names no format.
        Command::Stories(args) => {
            document(args.list.format.map(Format::from), args.action.listing_to())
        }
        Command::Highlights(args) => {
            document(args.list.format.map(Format::from), args.action.listing_to())
        }
        Command::Posts(args) => {
            document(args.list.format.map(Format::from), args.action.listing_to())
        }
        Command::Post(args) => {
            document(args.list.format.map(Format::from), args.action.listing_to())
        }
        Command::Profile(args) => matches!(
            effective_format(args.format.map(Format::from), args.output.as_deref()),
            Format::Json
        ),
        Command::Whoami(args) => args.output.json,
        Command::Status(args) => args.output.json,
        Command::Account(AccountCommand::List(args)) => args.output.json,
        // It takes no format flag and answers in JSON down a pipe, the way
        // `commands::import::run` decides it.
        Command::Import(_) => document(None, None),
        Command::Watch(args) => match &args.command {
            None => args.run.output.json,
            Some(WatchCommand::Once(once)) => once.output.json,
            Some(WatchCommand::Check(check)) => check.output.json,
            Some(WatchCommand::Status(status)) => status.output.json,
            Some(WatchCommand::Diff(diff)) => diff.output.json,
            Some(WatchCommand::Setup(_)) => false,
        },
        Command::Account(AccountCommand::Use(_))
        | Command::Login(_)
        | Command::Logout(_)
        | Command::Purge(_)
        | Command::Pfp(_)
        | Command::Fetch(_)
        | Command::Follow(_)
        | Command::Unfollow(_)
        | Command::BrowserOwner => false,
    };
    if json { Wording::Json } else { Wording::Prose }
}

/// Whether a file really holds PEM certificates.
///
/// **Emptiness is the case that matters**, and it is why this is not a bare
/// `is_err()`. `from_pem_bundle` scans for BEGIN/END blocks and answers `Ok`
/// with an empty list when there are none, so a text file, a DER file or a
/// mistyped path that happened to exist would pass in silence — and
/// `tls_certs_only` would then be handed Mozilla's roots and nothing of the
/// user's, which is the one outcome `--tls-extra-root` exists to prevent. It
/// would fail at the handshake, hours later, against Instagram and nowhere else.
fn holds_a_certificate(pem: &[u8]) -> anyhow::Result<()> {
    match snob_ig::http::reqwest::Certificate::from_pem_bundle(pem) {
        Ok(found) if !found.is_empty() => Ok(()),
        Ok(_) => anyhow::bail!("it holds no PEM certificates"),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn trust_from(cli: &Cli) -> anyhow::Result<snob_ig::http::Trust> {
    if !cli.strict_roots {
        return Ok(snob_ig::http::Trust::Platform);
    }
    if !snob_ig::http::CAN_NARROW {
        anyhow::bail!(
            "--strict-roots does nothing on this build, so it is refused rather than \
             ignored.\n\
             This is the Windows on ARM64 binary, which uses the operating system's TLS \
             stack; that stack has no way to be told \"these roots and no others\"."
        );
    }

    let mut extra = Vec::new();
    for path in &cli.tls_extra_root {
        // Read and checked here rather than at the first request, so a typo in
        // a path is an error before anything has been walked -- and so the
        // failure names the file rather than arriving as a handshake error
        // hours later.
        let pem =
            std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        holds_a_certificate(&pem)
            .with_context(|| format!("{} is not usable as an extra root", path.display()))?;
        extra.push(pem);
    }
    Ok(snob_ig::http::Trust::Narrow { extra })
}

/// Where this run keeps its files, whether the keyring is off, and — for a
/// sandbox — the keyring namespace it is confined to.
///
/// One place, so the sandbox seam is one `cfg` block rather than a condition at
/// every site that opens something. In a release build it is
/// `AppPaths::discover()` and the flag the user typed, and nothing else exists.
///
/// A sandbox forces the file backend as well as the directory. Not a
/// convenience: the keyring is per user and not per directory, so a sandbox run
/// that used it would read, overwrite and — through `purge` — delete the real
/// stored session of whoever is running the tests. The same rule
/// `crates/snob-core/tests/keyring.rs` holds the test suite to, applied to the
/// binary.
///
/// **Forcing the backend is not enough on its own.** `SecretStore` keeps
/// talking to the keyring whatever backend it is on, and it must: `save`
/// deletes the keyring entry so that `load`, which reads the keyring first,
/// cannot go on serving a session the file has replaced, and the two watch
/// secrets have no file form at all, so they never consult the backend. Every
/// one of those entries is named by the service, so on the real service a
/// sandbox `login` would delete the developer's session, a sandbox `purge`
/// would take the webhook secrets with it, and a sandbox that had not logged in
/// yet would load the real cookie and carry it to the redirected server, which
/// the flag pairing is documented to make impossible. So the third element
/// gives the sandbox a keyring namespace of its own, and every one of those
/// operations lands on entries nothing outside the sandbox can see.
fn wiring(cli: &Cli) -> anyhow::Result<(AppPaths, bool, Option<String>)> {
    // Before any client exists, which is what `use_trust` requires: a run
    // cannot change what it trusts halfway through. A second answer is an
    // error and not a shrug: `http.rs` says a security option that silently
    // does nothing is worse than one that is not offered. The same value twice
    // is accepted, because the tests below call this more than once in one
    // process and a repeat of the same answer changes nothing.
    let trust = trust_from(cli)?;
    if let Err(already) = snob_ig::http::use_trust(trust.clone())
        && already != trust
    {
        anyhow::bail!("the trust store was already decided for this run");
    }

    #[cfg(feature = "testing")]
    if let Some(root) = &cli.sandbox_root {
        if let Some(base) = cli.ig_base_url.clone() {
            // Before any client is built, and once. Clap has already refused
            // this flag without a sandbox root, so a redirected client can only
            // carry a session out of the store inside `root`.
            snob_ig::client::point_every_client_at(base)
                .map_err(|already| anyhow::anyhow!("already pointed at {already}"))?;
        }
        // The browser path against the fake server too, so it can be driven
        // end to end. Off by default: the rest of the suite reaches its mock
        // servers directly, with no browser on the machine required.
        if cli.through_the_browser {
            snob_ig::client::page::use_the_page_off_instagram();
        }
        return Ok((
            AppPaths::rooted_at(root),
            true,
            Some(sandbox_keyring_namespace(root)),
        ));
    }
    Ok((AppPaths::discover()?, cli.no_keyring, None))
}

/// The keyring service name a sandbox run is confined to.
///
/// Derived from the root rather than one shared constant, so two sandboxes
/// running at the same time — which is the normal case, the suite runs in
/// parallel — cannot delete each other's entries.
///
/// FNV-1a rather than `DefaultHasher`: the name has to come out the same on the
/// *next* invocation, because one process writes the entry and another reads it
/// back, and `DefaultHasher`'s output is explicitly not promised to be stable
/// between Rust releases. It is not a security boundary and does not need to be
/// one — the isolation comes from the name being different, not from it being
/// hard to guess.
#[cfg(feature = "testing")]
fn sandbox_keyring_namespace(root: &std::path::Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("snob-ig-sandbox-{hash:016x}")
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let (paths, no_keyring, keyring_namespace) = wiring(&cli)?;
    if let Command::BrowserOwner = cli.command {
        let linger = if keyring_namespace.is_some() {
            snob_cli::owner::Linger::for_a_sandbox()
        } else {
            snob_cli::owner::Linger::for_people()
        };
        snob_cli::owner::serve(&paths, linger).await?;
        return Ok(ExitCode::Ok);
    }
    let store = SecretStore::new(paths.clone(), no_keyring);
    let store = match &keyring_namespace {
        Some(service) => store.with_service(service),
        None => store,
    };

    // The single-account layout moves under its account before anything reads
    // it, with no browser of an older layout's owner still writing into it.
    // Not for `purge`, which removes it whole and must work when the move
    // cannot, nor for `import` and `fetch`, which store nothing.
    if !matches!(
        cli.command,
        Command::Purge(_) | Command::Import(_) | Command::Fetch(_)
    ) && snob_store::layout::pending(&paths)
    {
        snob_cli::owner::quit(&paths).await;
        let settled = snob_store::layout::settle(&paths, &store)?;
        if let Some(parked) = settled.parked {
            snob_cli::ui::warn(&format!(
                "a database from before several accounts could be signed in was set aside \
                 at {}, because its account already had one; nothing reads it now",
                parked.display()
            ));
        }
        if let Some(why) = settled.unreadable {
            snob_cli::ui::warn(&format!(
                "the session stored before several accounts could be signed in does not \
                 read ({why}), so it was set aside and nobody is signed in; log in again \
                 with \"snob login\". It may still be active on Instagram."
            ));
        }
    }

    // Every request to Instagram goes out from a browser from here on, unless
    // this run was told not to — see `headless/`. The switch is for a
    // machine with no Chromium browser on it, and for comparing the two. The
    // browsers are the owner's (`owner`), shared with every other command,
    // unless this run was told to keep its own.
    if !snob_cli::headless::env_flag("SNOB_NO_BROWSER") {
        let start = if snob_cli::headless::env_flag("SNOB_NO_OWNER") {
            None
        } else {
            Some(owner_args(&cli)?)
        };
        snob_cli::owner::install(&paths, start);
    }
    // A copy for the command, which consumes its store; the write-back needs
    // the same store, on the same keyring service, once it is done.
    let outcome = dispatch(cli, store.clone(), &paths).await;
    // Whatever the command came to: the browser kept the session current
    // while it ran, and the stored copy learns what it kept.
    snob_cli::owner::finish(&store, &paths).await;
    outcome
}

/// Runs the command, as the account it resolves to when it acts as one.
///
/// Every command that acts as an account is handed that account's paths and
/// session, and nothing else of any account's: `account::resolve` is the one
/// place that decides which.
async fn dispatch(cli: Cli, store: SecretStore, paths: &AppPaths) -> anyhow::Result<ExitCode> {
    // Read here rather than by clap: `purge` is narrowed to one account only
    // by a flag somebody typed, never by the environment.
    let env = std::env::var("SNOB_ACCOUNT").ok();
    // Read only by a command that acts as an account: a damaged registry must
    // not stop `purge` from removing it.
    let resolved = || {
        let registry = Registry::load(paths)?;
        let found =
            snob_cli::account::resolve_if_any(cli.account.as_deref(), env.as_deref(), &registry)?;
        if let Some(found) = &found {
            snob_cli::report::act_as(found.clone());
        }
        Ok::<_, anyhow::Error>(found.map(|found| paths.account(found.pk)))
    };
    // A command that has nothing to do without an account refuses as every
    // command does with no session.
    let required =
        || -> anyhow::Result<AccountPaths> { resolved()?.ok_or_else(commands::common::no_session) };
    let session = |account: &AccountPaths| store.session_of(account);

    // Whether this run was told which account, rather than left to the
    // registry.
    let named = snob_cli::account::given(cli.account.as_deref(), env.as_deref()).is_some();

    match cli.command {
        Command::Login(args) => {
            // An account being added is none of those signed in here, and
            // with none active there is none to log in again as.
            let renewing = if args.add {
                None
            } else {
                let registry = Registry::load(paths)?;
                match snob_cli::account::resolve(cli.account.as_deref(), env.as_deref(), &registry)
                {
                    Ok(found) => {
                        snob_cli::report::act_as(found.clone());
                        Some(found)
                    }
                    Err(Unresolved::NoAccount | Unresolved::NoneChosen) => None,
                    Err(refused) => return Err(refused.into_error()),
                }
            };
            commands::login::run(args, store, paths, renewing, named).await
        }
        Command::Whoami(args) => commands::whoami::run(args, store, resolved()?).await,
        Command::Status(args) => commands::status::run(args, &store, paths, resolved()?),
        Command::Logout(args) => {
            let account = if args.all { None } else { resolved()? };
            commands::logout::run(args, store, paths, account).await
        }
        Command::Purge(args) => match cli.account.as_deref() {
            Some(name) => commands::purge::run_for_account(args, store, paths, name).await,
            None => {
                // The whole data directory goes, the owner's log with it, so
                // the owner is asked to leave, not only to close its browsers.
                snob_cli::owner::quit(paths).await;
                commands::purge::run(args, store, paths)
            }
        },
        Command::Account(command) => commands::account::run(
            command,
            &store,
            paths,
            cli.account.as_deref(),
            env.as_deref(),
        ),
        Command::Followers(args) => {
            commands::lists::run(args, store, &required()?, ListKind::Followers).await
        }
        Command::Following(args) => {
            commands::lists::run(args, store, &required()?, ListKind::Following).await
        }
        Command::Profile(args) => commands::profile::run(args, store, &required()?).await,
        Command::Scan(args) => commands::scan::run(args, store, &required()?).await,
        Command::Unfollowers(args) => {
            commands::sets::run(args, store, &required()?, SetOp::Unfollowers).await
        }
        Command::Fans(args) => commands::sets::run(args, store, &required()?, SetOp::Fans).await,
        Command::Friends(args) => {
            commands::sets::run(args, store, &required()?, SetOp::Friends).await
        }
        Command::Pfp(args) => commands::pfp::run(args, store, &required()?).await,
        Command::Stories(args) => commands::stories::run(args, store, &required()?).await,
        Command::Highlights(args) => commands::highlights::run(args, store, &required()?).await,
        Command::Posts(args) => commands::posts::run(args, store, &required()?).await,
        Command::Post(args) => commands::post::run(args, store, &required()?).await,
        Command::Follow(args) => {
            let account = required()?;
            let verb = commands::follow::Verb::Follow;
            commands::follow::run(args, verb, session(&account), &account).await
        }
        Command::Unfollow(args) => {
            let account = required()?;
            let verb = commands::follow::Verb::Unfollow;
            commands::follow::run(args, verb, session(&account), &account).await
        }
        Command::Watch(args) => commands::watch::run(args, store, paths, resolved()?).await,
        Command::Import(command) => commands::import::run(command),
        Command::Fetch(args) => {
            // Only the User-Agent is read off the account, and only when one
            // was not given; a registry or session that will not read is no
            // reason not to fetch a public file.
            let account_ua = || {
                let account = resolved().ok().flatten()?;
                let session = session(&account).load().ok().flatten()?;
                Some(session.user_agent)
            };
            let user_agent = match args.user_agent.clone() {
                Some(given) => Some(given),
                None => account_ua(),
            };
            commands::fetch::run(args, user_agent).await
        }
        // Answered at the top.
        Command::BrowserOwner => Ok(ExitCode::Ok),
    }
}

/// What the owner of the browsers is started with: the global flags that
/// decide where its files are, so that it lands on the same ones, and the
/// hidden command. A sandbox root is made absolute, since the owner does not
/// run from here (`owner::spawn`). `--verbose` too, which then holds for
/// every command that owner serves, and for none it serves when the command
/// that started it did not ask.
fn owner_args(cli: &Cli) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    #[cfg(feature = "testing")]
    if let Some(root) = &cli.sandbox_root {
        args.push("--sandbox-root".into());
        args.push(std::path::absolute(root)?.into());
    }
    if cli.verbose {
        args.push("--verbose".into());
    }
    args.push(snob_cli::owner::COMMAND.into());
    Ok(args)
}

/// Gives the cursor back if the process dies with a menu on screen.
///
/// The release profile is `panic = "abort"`, so nothing runs on the way out —
/// and `ui::menu` hides the cursor and takes the terminal raw while a menu
/// is up, as the story browser does. A panic during the login menu would
/// otherwise leave the user with an invisible cursor for the rest of their
/// shell session, which reads as the terminal being broken rather than as this
/// program having failed.
///
/// The hook still runs under an aborting runtime: `set_hook` is documented to
/// run with both runtimes.
fn restore_terminal_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        snob_cli::ui::restore_terminal();
        previous(info);
    }));
}

/// What is logged, and at which level.
///
/// `--verbose` turns on `debug` for this workspace's crates and nothing else;
/// without it, `SNOB_LOG` is read as a list of `target=level` directives --
/// `snob_ig=debug,warn` -- and `warn` is the answer when it is unset or does not
/// parse. A directive that does not parse is said so, once, rather than
/// silently read as `warn`: somebody who set the variable is debugging and is
/// the one person who needs to know it was ignored.
///
/// The owner of the browsers says, besides, when it starts: one owner after
/// another appends to the same log.
///
/// `Targets` rather than `EnvFilter`, which is the obvious type: `EnvFilter`
/// understands span and field matchers, and to do so it links a
/// regular-expression engine. Nothing here logs a span. The manifest says what
/// that costs.
fn log_filter(
    verbose: bool,
    spec: Option<&str>,
    owner: bool,
) -> tracing_subscriber::filter::Targets {
    use tracing::Level;
    use tracing_subscriber::filter::Targets;

    if verbose {
        return Targets::new()
            .with_target("snob", Level::DEBUG)
            .with_target("snob_ig", Level::DEBUG)
            .with_target("snob_core", Level::DEBUG)
            .with_target("snob_store", Level::DEBUG)
            .with_target("snob_cli", Level::DEBUG);
    }
    let quiet = || {
        let quiet = Targets::new().with_default(Level::WARN);
        if owner {
            quiet.with_target("snob_cli::owner::server", Level::INFO)
        } else {
            quiet
        }
    };
    match spec {
        Some(spec) => spec.parse::<Targets>().unwrap_or_else(|e| {
            eprintln!("warning: SNOB_LOG was ignored: {e}");
            quiet()
        }),
        None => quiet(),
    }
}

/// A command's lines are read as it runs; the owner's, in its log, later, so
/// they say when. So do a command's under `--verbose`, which is for reading
/// the pacing back: the wait before a list and between its pages is only
/// visible in the gaps between the lines.
fn init_tracing(verbose: bool, owner: bool) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let spec = std::env::var("SNOB_LOG").ok();
    let filter = log_filter(verbose, spec.as_deref(), owner);
    let lines = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // **The filter decides, and nothing before it.** The builder's own
        // ceiling, INFO by default, would drop every debug event before
        // `log_filter` sees it.
        .with_max_level(tracing::Level::TRACE);
    if owner || verbose {
        lines.finish().with(filter).init();
    } else {
        lines.without_time().finish().with(filter).init();
    }
}

#[cfg(test)]
mod filter_tests {
    use super::log_filter;
    use tracing::Level;

    /// The two settings a person reaches for, and the one they mistype.
    #[test]
    fn the_filter_reads_what_was_asked_for() {
        let verbose = log_filter(true, None, false);
        assert!(verbose.would_enable("snob_ig::client", &Level::DEBUG));
        assert!(!verbose.would_enable("hyper_util::client", &Level::DEBUG));
        assert!(!verbose.would_enable("snob_ig::client", &Level::TRACE));

        let quiet = log_filter(false, None, false);
        assert!(quiet.would_enable("snob_ig::client", &Level::WARN));
        assert!(!quiet.would_enable("snob_ig::client", &Level::INFO));
        assert!(!quiet.would_enable("snob_cli::owner::server", &Level::INFO));

        // The owner says when it starts, and otherwise no more than a command.
        let owner = log_filter(false, None, true);
        assert!(owner.would_enable("snob_cli::owner::server", &Level::INFO));
        assert!(!owner.would_enable("snob_cli::owner::server", &Level::DEBUG));
        assert!(!owner.would_enable("snob_cli::headless", &Level::INFO));

        let chosen = log_filter(false, Some("snob_store=debug,warn"), false);
        assert!(chosen.would_enable("snob_store::secrets", &Level::DEBUG));
        assert!(!chosen.would_enable("snob_ig::client", &Level::DEBUG));
        assert!(chosen.would_enable("snob_ig::client", &Level::WARN));

        // A spec that does not parse falls back to the quiet default rather
        // than to nothing at all, so a typo does not also silence warnings.
        let typo = log_filter(false, Some("snob_ig=loud"), false);
        assert!(typo.would_enable("snob_ig::client", &Level::WARN));
        assert!(!typo.would_enable("snob_ig::client", &Level::INFO));
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use clap::Parser;

    /// The one property the whole sandbox rests on.
    ///
    /// Not "it returns something": it returns something that is **not** the
    /// name the real entries are under. `SecretStore` reaches the keyring on
    /// every backend, so this string is the only thing standing between a
    /// sandbox `login` and the developer's own session.
    #[test]
    fn a_sandbox_never_shares_the_real_keyring_service() {
        let cli = Cli::try_parse_from(["snob", "--sandbox-root", "/tmp/one", "whoami"])
            .expect("the flag parses");
        let (_, prefer_file, namespace) = wiring(&cli).expect("a sandbox needs no discovery");

        assert!(prefer_file, "a sandbox stores its session in a file");
        let namespace = namespace.expect("a sandbox is given a keyring namespace of its own");
        assert_ne!(
            namespace, "snob-ig",
            "a sandbox on the real service deletes the real session"
        );
        assert!(namespace.starts_with("snob-ig-sandbox-"), "{namespace}");
    }

    /// One process writes the entry and another reads it back, so a name that
    /// changed between invocations would lose the session every time.
    #[test]
    fn the_namespace_is_the_same_answer_twice_and_differs_per_root() {
        let one = sandbox_keyring_namespace(std::path::Path::new("/tmp/one"));
        let again = sandbox_keyring_namespace(std::path::Path::new("/tmp/one"));
        let other = sandbox_keyring_namespace(std::path::Path::new("/tmp/two"));

        assert_eq!(one, again, "the same root has to name the same entries");
        assert_ne!(
            one, other,
            "two sandboxes at once must not be able to delete each other's entries"
        );
    }

    /// Without the flag there is no namespace, so a real run is untouched by
    /// any of this.
    #[test]
    fn an_ordinary_run_is_left_on_the_real_service() {
        let cli = Cli::try_parse_from(["snob", "whoami"]).expect("it parses");
        let (_, _, namespace) = wiring(&cli).expect("discovery works on a test machine");
        assert_eq!(namespace, None);
    }
    /// A file that holds no certificate is refused, and that is the case a bare
    /// error check misses.
    ///
    /// `from_pem_bundle` looks for BEGIN/END blocks and answers `Ok` with an
    /// empty list when it finds none, so plain text, a DER file, or a path that
    /// happened to exist all passed. The narrowing would then hand
    /// `tls_certs_only` Mozilla's roots and none of the user's -- the one
    /// outcome the flag exists to prevent -- and it would fail at the handshake
    /// against Instagram, hours later, and nowhere else.
    ///
    /// Checked against a real certificate rather than only against rejections,
    /// because a predicate that refuses everything also passes the first half.
    #[test]
    fn an_extra_root_has_to_be_a_certificate() {
        assert!(
            holds_a_certificate(
                b"not a certificate
"
            )
            .is_err()
        );
        assert!(holds_a_certificate(b"").is_err());
        assert!(
            holds_a_certificate(
                b"-----BEGIN CERTIFICATE-----
not base64
-----END CERTIFICATE-----
"
            )
            .is_err(),
            "a block that is not a certificate is not one"
        );

        // One of Mozilla's own, so the accepting half is exercised too.
        let real = snob_ig::http::reqwest::Certificate::from_der(
            &webpki_root_certs::TLS_SERVER_ROOT_CERTS[0],
        );
        assert!(real.is_ok(), "the bundled roots are certificates");
    }
}
