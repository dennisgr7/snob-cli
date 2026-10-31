//! The long lists `snob mcp` handed back one page of, held for the pages
//! after it.
//!
//! A list comes back a page at a time (`answer::page`), and the call for the
//! next page names the same list with an `offset`. **That call never walks
//! anything**, and the page it gets has to be a page of the same list the
//! first call read. Reading it again from the store is not always that: a
//! walk given `max_pages` may have stopped short, and what it read is not
//! stored as a whole list, so `--offline` serves the last whole one; and a
//! crossing whose two lists were checked against Instagram when the first
//! page was read is refused offline when they were walked far apart
//! (`engine::cooldown::check_same_moment`). So the server holds the whole of
//! a list it handed back in part, under the call that read it
//! (`tools::Page::of`), and hands the later pages from that; only a list it
//! no longer holds is read from the store, as `mode: "stored"` would read it.
//!
//! Held in memory only, for [`HELD_FOR`], a few at a time ([`HELD`], at most
//! [`HELD_BYTES`] in all), and gone with the process. It is what was already
//! printed to the server, the same accounts the store has on disk, and
//! holding it opens nothing: the server still builds no client and opens no
//! store.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// How many lists are held at once: the few an agent pages through in one
/// conversation. The oldest goes first.
const HELD: usize = 4;

/// How long a list is held after it was read. Paging through a list of
/// twelve thousand accounts is sixty or so calls, each a turn of the model's;
/// past this the next page is read from the store instead, or for a walk cut
/// short by `max_pages`, the call is made again from the start.
const HELD_FOR: Duration = Duration::from_secs(30 * 60);

/// The most held at once, counted as the compact JSON of the items. An
/// account is about 450 bytes of it with a picture's address, so this is
/// some 35,000 accounts: a few long lists, and a bound on what a server
/// started and forgotten in the background keeps.
const HELD_BYTES: usize = 16 << 20;

/// The lists held, newest first.
#[derive(Default)]
pub struct Held {
    lists: Mutex<VecDeque<List>>,
}

/// A whole list as a call read it.
#[derive(Debug, PartialEq)]
pub struct Whole {
    /// Each item as its compact JSON, which is how pages are measured
    /// (`answer::page`).
    pub items: Vec<String>,
    /// What the first page said about the whole list (`answer::ABOUT`):
    /// where it came from and when, and whether it is all of it, which every
    /// later page says again.
    pub about: Map<String, Value>,
}

struct List {
    /// The call that read it (`tools::Page::of`).
    of: String,
    at: Instant,
    whole: Arc<Whole>,
    bytes: usize,
}

impl Held {
    /// Holds `whole`, the whole list the call `of` read, in place of what an
    /// earlier run of that call read. One too big to hold beside nothing
    /// else is not held, and its later pages are read from the store.
    pub fn keep(&self, of: String, whole: Whole) {
        self.keep_at(of, whole, Instant::now());
    }

    /// The whole list the call `of` read, while it is held.
    pub fn get(&self, of: &str) -> Option<Arc<Whole>> {
        self.get_at(of, Instant::now())
    }

    fn keep_at(&self, of: String, whole: Whole, now: Instant) {
        let bytes = whole.items.iter().map(String::len).sum::<usize>()
            + Value::Object(whole.about.clone()).to_string().len();
        let mut lists = self.lock();
        lists.retain(|list| list.of != of && !expired(list, now));
        if bytes > HELD_BYTES {
            return;
        }
        lists.push_front(List {
            of,
            at: now,
            whole: Arc::new(whole),
            bytes,
        });
        lists.truncate(HELD);
        while lists.iter().map(|list| list.bytes).sum::<usize>() > HELD_BYTES {
            lists.pop_back();
        }
    }

    fn get_at(&self, of: &str, now: Instant) -> Option<Arc<Whole>> {
        let mut lists = self.lock();
        lists.retain(|list| !expired(list, now));
        lists
            .iter()
            .find(|list| list.of == of)
            .map(|list| Arc::clone(&list.whole))
    }

    /// The lists, whatever a call that panicked while holding them left:
    /// every change above leaves them whole, and losing a page is better
    /// than losing the server.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<List>> {
        self.lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn expired(list: &List, now: Instant) -> bool {
    now.saturating_duration_since(list.at) >= HELD_FOR
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Whole {
        Whole {
            items: (0..n).map(|n| format!(r#"{{"pk":{n}}}"#)).collect(),
            about: Map::new(),
        }
    }

    #[test]
    fn a_list_is_held_for_its_own_call_and_replaced_by_a_new_read() {
        let held = Held::default();
        let now = Instant::now();
        held.keep_at("a".into(), items(3), now);
        assert_eq!(held.get_at("a", now).unwrap().items.len(), 3);
        assert!(held.get_at("b", now).is_none());

        held.keep_at("a".into(), items(5), now);
        assert_eq!(held.get_at("a", now).unwrap().items.len(), 5);
        assert_eq!(held.lock().len(), 1, "one list per call");
    }

    #[test]
    fn the_oldest_list_goes_first() {
        let held = Held::default();
        let now = Instant::now();
        for of in ["a", "b", "c", "d", "e"] {
            held.keep_at(of.into(), items(1), now);
        }
        assert!(held.get_at("a", now).is_none());
        for of in ["b", "c", "d", "e"] {
            assert!(held.get_at(of, now).is_some(), "{of}");
        }
    }

    #[test]
    fn a_list_is_forgotten_once_its_time_is_up() {
        let held = Held::default();
        let then = Instant::now();
        held.keep_at("a".into(), items(1), then);
        assert!(held.get_at("a", then + HELD_FOR / 2).is_some());
        assert!(held.get_at("a", then + HELD_FOR).is_none());
        assert!(held.lock().is_empty(), "and no longer kept");
    }

    #[test]
    fn what_is_held_stays_under_its_bytes() {
        let held = Held::default();
        let now = Instant::now();
        let big = |n: usize| Whole {
            items: vec!["x".repeat(n)],
            about: Map::new(),
        };
        held.keep_at("too big".into(), big(HELD_BYTES + 1), now);
        assert!(held.get_at("too big", now).is_none());

        held.keep_at("a".into(), big(HELD_BYTES / 2), now);
        held.keep_at("b".into(), big(HELD_BYTES / 2), now);
        held.keep_at("c".into(), big(1), now);
        assert!(held.get_at("a", now).is_none(), "the oldest made room");
        assert!(held.get_at("b", now).is_some());
        assert!(held.get_at("c", now).is_some());
    }
}
