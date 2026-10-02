//! Everything snob-ig keeps on the machine it runs on.
//!
//! The other side of the line [`snob_core`] draws. That crate is what the
//! things *are*; this one is where they are put: the SQLite database and its
//! migrations, the platform directories the database and the configuration live
//! in, the credential store, and the monitor's `watch.toml`.
//!
//! **The direction is one-way and load-bearing**: this crate depends on
//! `snob_core`, never the reverse. A type that both a walk and a database need
//! belongs over there.
//!
//! Everything here is per user and never per directory — the sessions, the
//! databases and the configuration are one machine account's. `AppPaths`
//! decides where, and `AccountPaths` is the part of it that belongs to one
//! Instagram account.

pub mod config;
pub mod layout;
pub mod paths;
pub mod registry;
pub mod secrets;
pub mod store;
#[cfg(windows)]
pub mod windows_user;
