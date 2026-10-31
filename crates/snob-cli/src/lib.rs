//! snob-ig command line interface.
//!
//! Exposed as a library as well as a binary so the commands can be tested end
//! to end against a mock server. Integration tests cannot import modules from a
//! binary.

// `pub(crate)` where nothing outside the crate reaches in: the integration
// tests and `main` are the two consumers. Exporting a module they never use
// switches off dead-code detection inside it, which is the one thing the
// export of a binary's module costs.
pub mod account;
pub mod app;
pub mod browser;
pub mod cdp;
pub mod cli;
pub mod commands;
/// Starting a process that outlives the command that started it, with
/// nothing of the command's: its output, its terminal, its job.
pub(crate) mod detach;
pub mod engine;
pub mod exit;
pub mod headless;
pub(crate) mod interrupt;
/// A list walk that outlives the call that started it, and `snob jobs`.
pub mod jobs;
pub mod media;
pub mod output;
/// The one process that runs the browsers for every command of this user's.
pub mod owner;
/// Starting the browser with the debugging protocol on a pipe rather than on a
/// loopback port. See the module for why it cannot be `std::process::Command`.
pub(crate) mod pipe;
pub mod posts;
pub(crate) mod power;
pub(crate) mod progress;
pub(crate) mod relay;
pub mod report;
pub mod seat;
pub mod ui;
pub mod watch;
