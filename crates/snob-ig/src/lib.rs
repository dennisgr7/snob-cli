//! Client for the web API that instagram.com itself uses.
//!
//! **Almost all of it reads.** Two write operations exist — follow and unfollow
//! — and a third one does not compile: `IgClient::post` is the only function
//! here that sends a method other than GET to Instagram, and it takes a
//! [`graphql::Mutation`] rather than a path, so what this crate can write is the
//! set of variants that enum has. No block, no remove-follower, no like, no
//! comment, no message, and **nothing that marks a story as seen** — which is a
//! write dressed as a read, because it puts the user in somebody else's viewer
//! list.
//!
//! The two verbs come with a regime rather than a permission: one account per
//! invocation, a budget of its own for writes, and a question asked before the
//! request. `AGENTS.md` carries the whole of it.
//!
//! Two things hold the line, and neither is a promise. [`graphql::Mutation::ALL`]
//! makes "there are two" a value a test can read, and the three matches it feeds
//! have no wildcard arm, so a new variant is a build error until somebody has
//! written it into all of them. And `crates/snob-core/tests/no_seen.rs` is the
//! backstop for the one write that must never appear at all: it reads all three
//! crates for the call that registers a view, in every spelling it is known by,
//! including the Relay operation the web client really sends. It is a denylist
//! and it says so — the identifier Instagram acts on is a `doc_id`, and no list
//! of words contains a number — which is why the line above it, the one that
//! makes a third mutation fail to compile, is the rule and this is the net.
//!
//! From the browser, [`allowlist`] holds it as well: every page a client
//! sends through refuses, before it leaves, any call that is not one of the
//! reads and two writes it names. That is snob's own calls; the app's, on
//! the documents the tab loads, it does not see.
//!
//! Each endpoint is documented above the method that calls it, in
//! `client/read.rs` and `client/write.rs`; the calls the web app itself reads
//! with are built in [`web`], and [`allowlist`] names every call that may be
//! sent.

pub mod allowlist;
pub mod awake;
pub mod client;
pub mod client_hints;
pub mod error;
pub mod graphql;
pub mod http;
pub mod login;
pub mod model;
pub mod pace;
pub mod page_values;
pub mod pager;
pub mod shortcode;
pub mod web;

/// Identifies the request as coming from Instagram's web app. Without this
/// header several endpoints answer 401 or 403.
pub const IG_APP_ID: &str = "936619743392459";

pub const BASE_URL: &str = "https://www.instagram.com";
