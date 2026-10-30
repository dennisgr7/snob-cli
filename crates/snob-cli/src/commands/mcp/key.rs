//! `snob mcp key`: the one key a client presents to reach `snob mcp` over the
//! network, kept in the system keyring (`secrets::Kind::McpKey`).
//!
//! **Only a key in snob's own form is taken**, from the keyring and from
//! `SNOB_MCP_KEY` alike: `snob_mcp_` and 43 base64url characters that decode
//! to exactly 32 bytes ([`Key::parse`]). A key a person chose would be as
//! strong as what they chose, and nothing at the far end could tell; one of
//! these is 256 random bits from the operating system, every time.
//!
//! **Keyring only, by design** (`SecretStore::save_secret` says why). On a
//! machine with no keyring, which is where a server is most likely to run,
//! `SNOB_MCP_KEY` in the server's environment is the way: `new` there makes
//! a key it cannot keep and prints it, for the person to put in that
//! environment. The variable wins over the keyring wherever it is set, so
//! `show` and `revoke` say so when it is: a key revoked from the keyring is
//! still taken by a server started with the variable.
//!
//! **The key is printed only to a terminal**, unless `--print` says
//! otherwise ([`may_print`]): an agent with a shell that ran `snob mcp key
//! new` would otherwise have the key in its transcript, and from there in
//! its vendor's logs. Everywhere else it is named by a fingerprint
//! ([`Key::fingerprint`]), which says which key it is and nothing that
//! helps to guess it.

use std::ffi::OsStr;
use std::io::IsTerminal;

use anyhow::Result;
use base64::Engine as _;
use snob_core::secret::Secret;
use snob_store::secrets::{Kind, SecretStore, Stored};
use zeroize::Zeroize as _;

use crate::cli::{McpKeyCommand, McpKeyNewArgs, McpKeyShowArgs};
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// The variable a server reads its key from instead of the keyring. A
/// client reads its copy from a variable of another name, so that a person
/// who exports one for a client does not hand the key to every server.
pub const ENV: &str = "SNOB_MCP_KEY";

/// What every key starts with, so that one found in a file or a log says
/// whose it is.
pub const PREFIX: &str = "snob_mcp_";

/// The random bytes in a key.
const BYTES: usize = 32;

/// The characters after [`PREFIX`]: 32 bytes in base64 without padding.
const ENCODED: usize = 43;

/// How much of the fingerprint is shown: 48 bits, enough to tell two keys
/// apart at a glance and far too few to search for one.
const FINGERPRINT: usize = 12;

/// A key in snob's form. The text is the key, as a client sends it.
pub struct Key(Secret);

/// Why a text is not a key. Never the text itself, which may be one with a
/// typo, and is a secret either way.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

impl Key {
    /// The text, if it is a key in snob's form.
    ///
    /// The decoder refuses trailing bits that are not zero, so a key has
    /// exactly one spelling: two texts that are the same key would be two
    /// keys to a server comparing what it is sent.
    pub fn parse(text: &str) -> Result<Self, Malformed> {
        let encoded = text.strip_prefix(PREFIX).ok_or(Malformed)?;
        if encoded.len() != ENCODED {
            return Err(Malformed);
        }
        let mut bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| Malformed)?;
        let length = bytes.len();
        bytes.zeroize();
        if length != BYTES {
            return Err(Malformed);
        }
        Ok(Self(Secret::new(text)))
    }

    /// A new key, from 256 random bits of the operating system's.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; BYTES];
        crate::relay::fill_random(&mut bytes)?;
        let text = format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        );
        bytes.zeroize();
        Ok(Self(Secret::new(text)))
    }

    /// The key, as it is stored and sent.
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// The first characters of an HMAC under the key, to name it by where
    /// the key itself must not be shown: what `show` prints, and what a
    /// server says it started with.
    pub fn fingerprint(&self) -> String {
        let signed = snob_core::watch::sign::sign("fingerprint", &self.0);
        let hex = signed.strip_prefix("sha256=").unwrap_or(&signed);
        hex[..FINGERPRINT].to_string()
    }
}

/// Where the key a server would take came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// [`ENV`].
    Environment,
    Keyring,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Environment => ENV,
            Self::Keyring => "the system keyring",
        }
    }
}

/// The key a server started now would take: [`ENV`] when it is set and not
/// empty, otherwise the keyring's. `None` is no key anywhere.
///
/// **A malformed key is refused, never passed over**: falling back to the
/// keyring past a variable somebody set on purpose would have the server
/// take a key they did not mean, and say nothing about it.
pub fn load(store: &SecretStore) -> Result<Option<(Key, Source)>> {
    if let Some(key) = from_env(std::env::var_os(ENV).as_deref())? {
        return Ok(Some((key, Source::Environment)));
    }
    match from_store(store)? {
        InStore::Found(key) => Ok(Some((key, Source::Keyring))),
        InStore::Nothing => Ok(None),
        InStore::Unreachable => Err(no_keyring().into()),
    }
}

/// The key in the variable's value, when there is one.
fn from_env(value: Option<&OsStr>) -> Result<Option<Key>> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value
        .to_str()
        .and_then(|text| Key::parse(text).ok())
        .map(Some)
        .ok_or_else(|| {
            ExitError::new(
                ExitCode::Error,
                format!("{ENV} is set to something that is not a key snob made"),
            )
            .with_hint(
                "set it to a key \"snob mcp key new\" printed, or unset it to use the keyring's",
            )
            .into()
        })
}

/// What the keyring holds.
enum InStore {
    Found(Key),
    Nothing,
    Unreachable,
}

fn from_store(store: &SecretStore) -> Result<InStore> {
    Ok(match store.load_secret(Kind::McpKey)? {
        Stored::Found(secret) => InStore::Found(Key::parse(secret.expose()).map_err(|_| {
            ExitError::new(
                ExitCode::Error,
                "the MCP key in the system keyring is not one snob made",
            )
            .with_hint("replace it with \"snob mcp key new --force\"")
        })?),
        Stored::Nothing => InStore::Nothing,
        Stored::Unreachable => InStore::Unreachable,
    })
}

/// A machine with no keyring to keep the key in, said with the way past it.
fn no_keyring() -> ExitError {
    ExitError::new(
        ExitCode::Error,
        "there is no system keyring on this machine to keep the MCP key in",
    )
    .with_hint(format!(
        "\"snob mcp key new\" prints a key it cannot keep; set {ENV} to it in the server's \
         environment"
    ))
}

/// Whether the key may be printed: to a terminal, or anywhere with
/// `--print`.
fn may_print(print: bool, terminal: bool) -> bool {
    print || terminal
}

/// What `new` does, given what the keyring holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Keep a new key, replacing whatever was there.
    Keep,
    /// There is one, and `--force` was not given.
    Refuse,
    /// There is no keyring: make one, print it, keep nothing.
    PrintOnly,
}

fn plan(stored: &Stored, force: bool) -> Plan {
    match stored {
        Stored::Found(_) if !force => Plan::Refuse,
        Stored::Found(_) | Stored::Nothing => Plan::Keep,
        Stored::Unreachable => Plan::PrintOnly,
    }
}

pub fn run(command: &McpKeyCommand, store: &SecretStore) -> Result<ExitCode> {
    match command {
        McpKeyCommand::New(args) => new(args, store),
        McpKeyCommand::Show(args) => show(args, store),
        McpKeyCommand::Revoke => revoke(store),
    }
}

fn new(args: &McpKeyNewArgs, store: &SecretStore) -> Result<ExitCode> {
    let printable = may_print(args.print, std::io::stdout().is_terminal());
    // Read before anything is drawn: whether there is one decides whether
    // to go on at all. A key in a form snob does not take is replaced the
    // same, with --force, which is what the refusal of one says to do.
    let stored = store.load_secret(Kind::McpKey)?;
    let plan = plan(&stored, args.force);
    let replacing = matches!(stored, Stored::Found(_));
    drop(stored);
    match plan {
        Plan::Refuse => {
            return Err(ExitError::new(
                ExitCode::Error,
                "there is already an MCP key in the system keyring",
            )
            .with_hint(
                "\"snob mcp key new --force\" replaces it, and every client given the old one \
                 stops working",
            )
            .into());
        }
        // Checked before a key is made, since this one would be kept nowhere.
        Plan::PrintOnly if !printable => {
            return Err(ExitError::new(
                ExitCode::Error,
                "there is no system keyring on this machine to keep the MCP key in, and the \
                 key is printed only to a terminal",
            )
            .with_hint(format!(
                "run it in a terminal, or with --print, and set {ENV} to what it prints in the \
                 server's environment"
            ))
            .into());
        }
        Plan::Keep | Plan::PrintOnly => {}
    }

    let key = Key::generate()?;
    if plan == Plan::Keep {
        store.save_secret(Kind::McpKey, &Secret::new(key.expose()))?;
    }
    if printable {
        ui::say!("{}", key.expose());
    }
    let fingerprint = key.fingerprint();
    match (plan, printable) {
        (Plan::Keep, true) => ui::info(&format!(
            "Kept in the system keyring, fingerprint {fingerprint}. Give it to the client; it is \
             not shown again, except by \"snob mcp key show --reveal\" in a terminal."
        )),
        (Plan::Keep, false) => ui::info(&format!(
            "Kept in the system keyring, fingerprint {fingerprint}, and not printed, since \
             standard output is not a terminal; \"snob mcp key show --reveal\" in a terminal \
             shows it."
        )),
        _ => ui::warn(&format!(
            "there is no system keyring on this machine, so this key, fingerprint \
             {fingerprint}, was kept nowhere; set {ENV} to it in the server's environment, and \
             only there"
        )),
    }
    if replacing && plan == Plan::Keep {
        ui::info("Every client given the old key stops working.");
    }
    environment_wins(plan == Plan::Keep);
    Ok(ExitCode::Ok)
}

fn show(args: &McpKeyShowArgs, store: &SecretStore) -> Result<ExitCode> {
    let printable = may_print(args.print, std::io::stdout().is_terminal());
    if args.reveal && !printable {
        return Err(ExitError::new(
            ExitCode::Error,
            "the key is printed only to a terminal, and standard output is not one",
        )
        .with_hint("add --print to print it here anyway")
        .into());
    }
    let found = match from_env(std::env::var_os(ENV).as_deref())? {
        Some(key) => Some((key, Source::Environment)),
        None => match from_store(store)? {
            InStore::Found(key) => Some((key, Source::Keyring)),
            InStore::Nothing => None,
            InStore::Unreachable => {
                let refused = no_keyring();
                if args.reveal {
                    return Err(refused.into());
                }
                ui::say!("There is no system keyring here, and {ENV} is not set.");
                if let Some(hint) = refused.hint() {
                    ui::info(&format!("hint: {hint}"));
                }
                return Ok(ExitCode::Ok);
            }
        },
    };
    let Some((key, source)) = found else {
        if args.reveal {
            return Err(ExitError::new(ExitCode::Error, "there is no MCP key")
                .with_hint("make one with \"snob mcp key new\"")
                .into());
        }
        ui::say!("There is no MCP key: none in the system keyring, and {ENV} is not set.");
        ui::info("hint: make one with \"snob mcp key new\"");
        return Ok(ExitCode::Ok);
    };
    let described = format!(
        "The MCP key is read from {}, fingerprint {}.",
        source.describe(),
        key.fingerprint()
    );
    if args.reveal {
        // The key alone on standard output, so it can be handed on as it is.
        ui::say!("{}", key.expose());
        ui::info(&described);
    } else {
        ui::say!("{described}");
    }
    if source == Source::Environment {
        ui::warn(&format!(
            "{ENV} is set here, and wherever it is set it is the key, whatever the system \
             keyring holds"
        ));
    }
    Ok(ExitCode::Ok)
}

fn revoke(store: &SecretStore) -> Result<ExitCode> {
    match store.load_secret(Kind::McpKey)? {
        Stored::Found(secret) => {
            let fingerprint = Key::parse(secret.expose())
                .ok()
                .map(|key| key.fingerprint());
            drop(secret);
            store.forget_secret(Kind::McpKey)?;
            match fingerprint {
                Some(fingerprint) => ui::say!(
                    "The MCP key, fingerprint {fingerprint}, was removed from the system keyring."
                ),
                None => ui::say!("The MCP key was removed from the system keyring."),
            }
            ui::info(
                "A server already running keeps it until it is restarted; clients given it stop \
                 working once it is.",
            );
        }
        Stored::Nothing => ui::say!("There was no MCP key in the system keyring."),
        Stored::Unreachable => {
            ui::say!("There is no system keyring here, so there was no MCP key in one.");
        }
    }
    environment_wins(true);
    Ok(ExitCode::Ok)
}

/// Says that [`ENV`] overrides the keyring, when it is set here: a key kept
/// or removed there changes nothing for a server started with it.
fn environment_wins(about_the_keyring: bool) {
    let set = std::env::var_os(ENV).is_some_and(|value| !value.is_empty());
    if set && about_the_keyring {
        ui::warn(&format!(
            "{ENV} is set here, and a server started with it takes that key, whatever the \
             system keyring holds; unset it where the server is started"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key made here, in the form it is stored and sent.
    fn made() -> String {
        Key::generate().unwrap().expose().to_string()
    }

    #[test]
    fn only_a_key_in_snobs_format_is_accepted() {
        let key = made();
        assert!(key.starts_with(PREFIX));
        assert_eq!(key.len(), PREFIX.len() + ENCODED);
        assert!(Key::parse(&key).is_ok());

        let zeros = format!("{PREFIX}{}", "A".repeat(ENCODED));
        assert!(
            Key::parse(&zeros).is_ok(),
            "32 zero bytes are still 32 bytes"
        );

        let encoded = &key[PREFIX.len()..];
        for refused in [
            String::new(),
            encoded.to_string(),
            format!("snob_mcp{encoded}"),
            format!("SNOB_MCP_{encoded}"),
            format!("{PREFIX}{}", &encoded[1..]),
            format!("{key}A"),
            format!("{key}\n"),
            format!(" {key}"),
            // Standard base64's characters are not base64url's.
            format!("{PREFIX}{}+", &encoded[1..]),
            format!("{PREFIX}{}/", &encoded[1..]),
            // Padded, which this form never is.
            format!("{PREFIX}{}=", &encoded[1..]),
            // 43 characters whose last two bits are not zero: the same 32
            // bytes as some other spelling, so not a spelling of its own.
            format!("{PREFIX}{}B", "A".repeat(ENCODED - 1)),
            // The right length, with a space no key has.
            format!("{PREFIX}{} ", &encoded[1..]),
        ] {
            assert!(
                Key::parse(&refused).is_err(),
                "{refused:?} was taken for a key"
            );
        }
    }

    #[test]
    fn two_keys_made_are_two_keys_with_two_fingerprints() {
        let (a, b) = (made(), made());
        assert_ne!(a, b);
        let (a, b) = (Key::parse(&a).unwrap(), Key::parse(&b).unwrap());
        assert_eq!(a.fingerprint().len(), FINGERPRINT);
        assert!(a.fingerprint().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(
            a.fingerprint(),
            Key::parse(a.expose()).unwrap().fingerprint(),
            "a fingerprint is the key's, not the moment's"
        );
        assert!(!a.expose().contains(&a.fingerprint()));
    }

    /// The variable is the key when it is set, an empty one is not set, and
    /// one set to anything else is refused rather than passed over, without
    /// saying what it was set to.
    #[test]
    fn the_environment_holds_a_key_or_is_refused() {
        assert!(from_env(None).unwrap().is_none());
        assert!(from_env(Some(OsStr::new(""))).unwrap().is_none());

        let key = made();
        let read = from_env(Some(OsStr::new(&key))).unwrap().unwrap();
        assert_eq!(read.expose(), key);

        let typo = format!("{key}x");
        let refused = from_env(Some(OsStr::new(&typo))).err().unwrap();
        let said = format!("{refused:#}");
        assert!(said.contains(ENV), "{said}");
        assert!(!said.contains(&key), "the value was repeated: {said}");
        assert_eq!(crate::exit::exit_code_for(&refused), ExitCode::Error);
    }

    #[test]
    fn a_key_is_kept_only_where_there_is_room_for_it() {
        let found = Stored::Found(Secret::new(made()));
        assert_eq!(plan(&found, false), Plan::Refuse);
        assert_eq!(plan(&found, true), Plan::Keep);
        assert_eq!(plan(&Stored::Nothing, false), Plan::Keep);
        assert_eq!(plan(&Stored::Nothing, true), Plan::Keep);
        assert_eq!(plan(&Stored::Unreachable, false), Plan::PrintOnly);
    }

    #[test]
    fn the_key_is_printed_to_a_terminal_or_on_request() {
        assert!(may_print(false, true));
        assert!(may_print(true, false));
        assert!(may_print(true, true));
        assert!(!may_print(false, false));
    }
}
