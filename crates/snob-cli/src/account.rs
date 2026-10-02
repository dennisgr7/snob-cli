//! Which account a command acts as.
//!
//! **Everything that acts as an account is handed one resolved here**, and
//! the order is fixed: `--account`, then `SNOB_ACCOUNT`, then the active
//! account, then the only one there is. A name is an id when it is the id of
//! an account signed in here, and otherwise a username, compared without
//! regard to case as Instagram compares them.

use snob_core::Pk;
use snob_core::model::printable;
use snob_store::registry::{Registered, Registry};

use crate::app::Viewer;
use crate::exit::{ExitCode, ExitError};

/// The username `pk` last signed in with, if it is registered and had one.
pub fn username(registry: &Registry, pk: Pk) -> Option<&str> {
    registry
        .get(pk)
        .map(|account| account.username.as_str())
        .filter(|name| !name.is_empty())
}

/// The account as a run acting as it is reported, with the username it had
/// when it last signed in. An empty one is no name, so the label falls back to
/// the id.
impl From<&Registered> for Viewer {
    fn from(account: &Registered) -> Self {
        Self {
            pk: account.pk,
            username: Some(account.username.clone()).filter(|name| !name.is_empty()),
        }
    }
}

/// Why no account could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// No account is signed in here.
    NoAccount,
    /// The name given is no account signed in here.
    Unknown(String),
    /// The name given is the username of more than one account.
    Ambiguous(String),
    /// Several accounts are signed in, none is active and none was named.
    NoneChosen,
}

impl Unresolved {
    /// The refusal, with the exit code it carries: no account at all is the
    /// missing session every command already reports; the rest are mistakes
    /// in what was asked.
    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::NoAccount => crate::commands::common::no_session(),
            Self::Unknown(name) => ExitError::new(
                ExitCode::Error,
                format!("no account {} is signed in here", shown(&name)),
            )
            .with_hint("see \"snob account list\", or sign in as it with \"snob login --add\"")
            .into(),
            Self::Ambiguous(name) => ExitError::new(
                ExitCode::Error,
                format!("more than one account here is {}", shown(&name)),
            )
            .with_hint("name it by its id")
            .into(),
            Self::NoneChosen => ExitError::new(
                ExitCode::Error,
                "several accounts are signed in here and none is active",
            )
            .with_hint(
                "pick one with \"snob account use\", or name one with --account or SNOB_ACCOUNT",
            )
            .into(),
        }
    }
}

/// The account a command acts as, from what was typed, the environment and
/// the registry, in that order.
pub fn resolve(
    flag: Option<&str>,
    env: Option<&str>,
    registry: &Registry,
) -> Result<Viewer, Unresolved> {
    if registry.accounts.is_empty() {
        return Err(Unresolved::NoAccount);
    }
    if let Some(name) = given(flag, env) {
        return named(name, registry);
    }
    let active = registry.active_account();
    let only = match registry.accounts.as_slice() {
        [only] => Some(only),
        _ => None,
    };
    active
        .or(only)
        .map(Viewer::from)
        .ok_or(Unresolved::NoneChosen)
}

/// The name this run was given, by `--account` or else `SNOB_ACCOUNT`. Set
/// and empty is unset, as a shell leaves it after `SNOB_ACCOUNT=`.
pub fn given<'a>(flag: Option<&'a str>, env: Option<&'a str>) -> Option<&'a str> {
    flag.or(env).map(str::trim).filter(|name| !name.is_empty())
}

/// [`resolve`] for a command that also has something to do with no account
/// signed in at all.
pub fn resolve_if_any(
    flag: Option<&str>,
    env: Option<&str>,
    registry: &Registry,
) -> anyhow::Result<Option<Viewer>> {
    match resolve(flag, env, registry) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(Unresolved::NoAccount) => Ok(None),
        Err(other) => Err(other.into_error()),
    }
}

/// The account `name` names, by id or username, with no fallback to the
/// active one: an empty name is no account's.
pub fn named(name: &str, registry: &Registry) -> Result<Viewer, Unresolved> {
    if registry.accounts.is_empty() {
        return Err(Unresolved::NoAccount);
    }
    let name = name.trim();
    let name = name.strip_prefix('@').unwrap_or(name);
    if let Ok(pk) = name.parse::<Pk>()
        && let Some(account) = registry.get(pk)
    {
        return Ok(account.into());
    }
    let mut matching = registry
        .accounts
        .iter()
        .filter(|account| !name.is_empty() && account.username.eq_ignore_ascii_case(name));
    match (matching.next(), matching.next()) {
        (Some(account), None) => Ok(account.into()),
        (None, _) => Err(Unresolved::Unknown(name.to_string())),
        (Some(_), Some(_)) => Err(Unresolved::Ambiguous(name.to_string())),
    }
}

/// A name somebody typed, as a refusal repeats it.
fn shown(name: &str) -> String {
    format!("@{}", printable(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Epoch;

    const A: Pk = Pk::new(11);
    const B: Pk = Pk::new(22);

    fn registry(accounts: &[(Pk, &str)], active: Option<Pk>) -> Registry {
        let mut registry = Registry::default();
        for (pk, username) in accounts {
            registry.upsert(*pk, username, Epoch::new(1_700_000_000));
        }
        registry.active = active;
        registry
    }

    fn pk(resolved: Result<Viewer, Unresolved>) -> Pk {
        resolved.expect("an account is resolved").pk
    }

    #[test]
    fn the_flag_beats_the_environment_which_beats_the_active_account() {
        let two = registry(&[(A, "alice"), (B, "bob")], Some(A));

        assert_eq!(pk(resolve(Some("bob"), Some("alice"), &two)), B);
        assert_eq!(pk(resolve(None, Some("bob"), &two)), B);
        assert_eq!(pk(resolve(None, None, &two)), A);
        // Set and empty is unset, as a shell leaves it after `SNOB_ACCOUNT=`.
        assert_eq!(pk(resolve(None, Some(" "), &two)), A);
    }

    #[test]
    fn the_only_account_needs_no_choosing_and_several_do() {
        assert_eq!(pk(resolve(None, None, &registry(&[(A, "alice")], None))), A);
        assert_eq!(
            resolve(None, None, &registry(&[(A, "alice"), (B, "bob")], None)),
            Err(Unresolved::NoneChosen)
        );
        // An active mark naming no account is no mark.
        assert_eq!(
            resolve(
                None,
                None,
                &registry(&[(A, "alice"), (B, "bob")], Some(Pk::new(9)))
            ),
            Err(Unresolved::NoneChosen)
        );
    }

    #[test]
    fn a_name_is_an_id_a_username_or_neither() {
        let two = registry(&[(A, "alice"), (B, "22bis")], None);

        assert_eq!(pk(resolve(Some("22"), None, &two)), B);
        assert_eq!(pk(resolve(Some("@ALICE"), None, &two)), A);
        assert_eq!(pk(resolve(Some("22bis"), None, &two)), B);
        assert_eq!(
            resolve(Some("carol"), None, &two),
            Err(Unresolved::Unknown("carol".into()))
        );
        // A number that is no account's id is looked up as a username.
        let numeric = registry(&[(A, "12345")], None);
        assert_eq!(pk(resolve(Some("12345"), None, &numeric)), A);
    }

    /// A name alone never falls back to the active account, whatever it is.
    #[test]
    fn a_name_alone_names_an_account_or_none() {
        let two = registry(&[(A, "alice"), (B, "")], Some(A));
        assert_eq!(
            named(" @bob ", &two),
            Err(Unresolved::Unknown("bob".into()))
        );
        assert_eq!(named(" ", &two), Err(Unresolved::Unknown(String::new())));
        assert_eq!(named("@", &two), Err(Unresolved::Unknown(String::new())));
        assert_eq!(named("22", &two).map(|found| found.pk), Ok(B));
        assert_eq!(
            named("alice", &Registry::default()),
            Err(Unresolved::NoAccount)
        );
    }

    /// An account registered with no username is named by its id, not by a
    /// bare at sign, and its JSON carries no name.
    #[test]
    fn an_account_with_an_empty_username_is_named_by_its_id() {
        let two = registry(&[(A, "alice"), (B, "")], None);

        let unnamed = named("22", &two).unwrap();
        assert_eq!(unnamed.username, None);
        assert_eq!(unnamed.label(), "account 22");
        assert_eq!(unnamed.json()["username"], serde_json::Value::Null);

        let alice = resolve(Some("alice"), None, &two).unwrap();
        assert_eq!(alice.username.as_deref(), Some("alice"));
    }

    #[test]
    fn a_username_two_accounts_share_is_refused() {
        let renamed = registry(&[(A, "same"), (B, "Same")], None);
        assert_eq!(
            resolve(Some("same"), None, &renamed),
            Err(Unresolved::Ambiguous("same".into()))
        );
    }

    #[test]
    fn nobody_signed_in_is_no_session_and_a_mistake_is_an_error() {
        let empty = Registry::default();
        assert_eq!(
            resolve(Some("alice"), None, &empty),
            Err(Unresolved::NoAccount)
        );
        assert_eq!(resolve_if_any(None, None, &empty).unwrap(), None);

        let code = |e: Unresolved| crate::exit::from_chain(&e.into_error());
        assert_eq!(code(Unresolved::NoAccount), Some(ExitCode::NoSession));
        assert_eq!(code(Unresolved::Unknown("x".into())), Some(ExitCode::Error));
        assert_eq!(
            code(Unresolved::Ambiguous("x".into())),
            Some(ExitCode::Error)
        );
        assert_eq!(code(Unresolved::NoneChosen), Some(ExitCode::Error));

        let one = registry(&[(A, "alice")], None);
        let refused = resolve_if_any(Some("carol"), None, &one).unwrap_err();
        assert!(refused.to_string().contains("@carol"), "{refused}");
    }
}
