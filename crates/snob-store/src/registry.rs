//! The accounts snob is signed in as, and the one commands act as when none is
//! named: `accounts.toml` in the data directory.
//!
//! **Read with serde, written from a template by hand**, for the reason
//! `config.rs` gives: the `toml` crate here has no serializer, and a file a
//! person may open should say what it is. A test writes the template and reads
//! it back, so the two cannot drift.
//!
//! **Every change goes through [`Registry::update`]**, which holds a lock on a
//! file beside it while it reads, changes and writes. Two `snob login`s in two
//! terminals would otherwise each read the same file, add their own account,
//! and the second write would drop the first one's.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::paths::{AppPaths, PathError};
use snob_core::{Epoch, Pk};

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a list of accounts this version of snob can read: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(transparent)]
    Paths(#[from] PathError),
}

/// What the file says.
///
/// `deny_unknown_fields`, so a key this version does not know is refused
/// rather than dropped by the next write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The account commands act as when none is named.
    #[serde(default)]
    pub active: Option<Pk>,
    #[serde(default, rename = "account")]
    pub accounts: Vec<Registered>,
}

/// One account snob has signed in as.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registered {
    pub pk: Pk,
    /// The username it had when it last signed in. The id is what names the
    /// account; this is what a person types and reads.
    pub username: String,
    pub added_at: Epoch,
}

impl Registry {
    /// Reads the file. No file is no accounts.
    pub fn load(paths: &AppPaths) -> Result<Self, RegistryError> {
        let path = paths.registry_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(RegistryError::Read { path, source }),
        }
    }

    /// Reads the file, applies `change` and writes the result, holding the
    /// lock throughout so no other process's change is lost. Returns what was
    /// written.
    pub fn update(paths: &AppPaths, change: impl FnOnce(&mut Self)) -> Result<Self, RegistryError> {
        paths.ensure_dirs()?;
        let path = paths.registry_file();
        let lock_path = path.with_extension("toml.lock");
        let _lock = crate::paths::lock_file(&lock_path).map_err(|source| RegistryError::Write {
            path: lock_path,
            source,
        })?;

        // Read under the lock: what another process wrote a moment ago is
        // what this change is applied to.
        let mut registry = Self::load(paths)?;
        change(&mut registry);

        // Written whole and moved into place, so a reader sees the old file or
        // the new one and never half of one.
        let temporary = path.with_extension("toml.new");
        crate::paths::replace_private(&path, &temporary, registry.template().as_bytes())
            .map_err(|(_, source)| RegistryError::Write { path, source })?;
        Ok(registry)
    }

    pub fn get(&self, pk: Pk) -> Option<&Registered> {
        self.accounts.iter().find(|account| account.pk == pk)
    }

    /// The active account. A mark naming no listed account is a hand-edit,
    /// and read as no mark.
    pub fn active_account(&self) -> Option<&Registered> {
        self.active.and_then(|pk| self.get(pk))
    }

    /// Every account snob may hold a session or data for: the registry's, and
    /// every directory named after an id, in case the file is gone or damaged.
    pub fn known_accounts(paths: &AppPaths) -> Vec<Pk> {
        let mut found: Vec<Pk> = Self::load(paths)
            .map(|registry| registry.accounts.iter().map(|a| a.pk).collect())
            .unwrap_or_default();
        for pk in paths.account_dirs() {
            if !found.contains(&pk) {
                found.push(pk);
            }
        }
        found
    }

    /// Adds the account, or renames it if it is already here. When it was
    /// first added stays what it was.
    pub fn upsert(&mut self, pk: Pk, username: &str, now: Epoch) {
        match self.accounts.iter_mut().find(|account| account.pk == pk) {
            Some(account) => account.username = username.to_string(),
            None => self.accounts.push(Registered {
                pk,
                username: username.to_string(),
                added_at: now,
            }),
        }
    }

    /// Takes the account out, and the active mark with it if it had it.
    pub fn remove(&mut self, pk: Pk) {
        self.accounts.retain(|account| account.pk != pk);
        if self.active == Some(pk) {
            self.active = None;
        }
    }

    /// The file, written out so a person who opens it can read it.
    fn template(&self) -> String {
        let mut out = String::from(
            "# The accounts snob is signed in as. Written by snob: \"snob login\" adds\n\
             # one, and \"snob account use\" picks the one commands act as when none\n\
             # is named.\n",
        );
        if let Some(active) = self.active {
            out.push_str(&format!("\nactive = {active}\n"));
        }
        for account in &self.accounts {
            out.push_str("\n[[account]]\n");
            out.push_str(&format!("pk = {}\n", account.pk));
            out.push_str(&format!(
                "username = {}\n",
                crate::config::quote(&account.username)
            ));
            out.push_str(&format!("added_at = {}\n", account.added_at.get()));
        }
        out
    }
}

fn parse(text: &str, path: &Path) -> Result<Registry, RegistryError> {
    toml::from_str(text).map_err(|e| RegistryError::Invalid {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_accounts() -> Registry {
        let mut registry = Registry::default();
        registry.upsert(Pk::new(4_340_136_074), "someone", Epoch::new(1_786_925_176));
        registry.upsert(Pk::new(42), "with \"quotes\"", Epoch::new(1_786_925_177));
        registry.active = Some(Pk::new(42));
        registry
    }

    /// What the template writes is what the parser reads, the whole of it.
    #[test]
    fn what_is_written_is_what_is_read() {
        for registry in [Registry::default(), two_accounts()] {
            let text = registry.template();
            assert_eq!(
                parse(&text, Path::new("accounts.toml")).unwrap(),
                registry,
                "{text}"
            );
        }
    }

    #[test]
    fn no_file_is_no_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        assert_eq!(Registry::load(&paths).unwrap(), Registry::default());
    }

    #[test]
    fn an_update_is_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        let written = Registry::update(&paths, |registry| *registry = two_accounts()).unwrap();
        assert_eq!(written, two_accounts());
        assert_eq!(Registry::load(&paths).unwrap(), two_accounts());
    }

    /// A key this version does not know is refused, never dropped by the next
    /// write.
    #[test]
    fn an_unknown_key_is_refused() {
        let error = parse("active = 1\nsomething = true\n", Path::new("accounts.toml"));
        assert!(matches!(error, Err(RegistryError::Invalid { .. })));
    }

    /// A second sign-in renames the account and keeps when it was added;
    /// removing the active account leaves none active.
    #[test]
    fn upsert_renames_and_remove_takes_the_active_mark() {
        let mut registry = two_accounts();
        registry.upsert(Pk::new(42), "renamed", Epoch::new(9));
        let account = registry.get(Pk::new(42)).unwrap();
        assert_eq!(account.username, "renamed");
        assert_eq!(account.added_at, Epoch::new(1_786_925_177));
        assert_eq!(registry.accounts.len(), 2);

        registry.remove(Pk::new(42));
        assert!(registry.get(Pk::new(42)).is_none());
        assert_eq!(registry.active, None);
    }

    /// The active mark counts only when it names a listed account.
    #[test]
    fn a_mark_naming_nobody_is_no_active_account() {
        let mut registry = two_accounts();
        assert_eq!(registry.active_account().map(|a| a.pk), Some(Pk::new(42)));
        registry.active = Some(Pk::new(9));
        assert_eq!(registry.active_account(), None);
    }

    /// An account whose directory is there is known, listed or not.
    #[test]
    fn known_accounts_are_listed_ones_and_ones_with_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        Registry::update(&paths, |registry| *registry = two_accounts()).unwrap();
        paths.account(Pk::new(7)).ensure_dirs().unwrap();
        paths.account(Pk::new(42)).ensure_dirs().unwrap();
        std::fs::create_dir_all(paths.unclaimed_dir()).unwrap();

        let mut known: Vec<u64> = Registry::known_accounts(&paths)
            .iter()
            .map(|pk| pk.get())
            .collect();
        known.sort_unstable();
        assert_eq!(known, [7, 42, 4_340_136_074]);
    }

    /// Two processes adding an account at once both land: each reads the file
    /// under the lock, so neither writes over what the other added.
    #[test]
    fn concurrent_updates_lose_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = AppPaths::rooted_at(tmp.path());
        std::thread::scope(|scope| {
            for n in 1..=8u64 {
                let paths = &paths;
                scope.spawn(move || {
                    Registry::update(paths, |registry| {
                        registry.upsert(Pk::new(n), &format!("user{n}"), Epoch::new(0));
                    })
                    .unwrap();
                });
            }
        });

        let registry = Registry::load(&paths).unwrap();
        let mut pks: Vec<u64> = registry.accounts.iter().map(|a| a.pk.get()).collect();
        pks.sort_unstable();
        assert_eq!(pks, (1..=8).collect::<Vec<_>>());
    }
}
