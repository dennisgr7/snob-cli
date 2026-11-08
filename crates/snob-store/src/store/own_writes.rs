//! The follows and unfollows the account made through snob, so a stored
//! following list can be handed out with them applied.
//!
//! A stored list is a walk's, and a walk is a moment. After `snob unfollow
//! someone`, the stored following list went on naming them until the next
//! walk, so `snob unfollowers --offline`, an agent's stored list and every
//! list a cooldown served answered as if the unfollow had not happened. The
//! write is a fact this computer knows for certain, having sent it.
//!
//! **Applied and said, never passed off as a walk**: the engine applies what
//! is here to a stored list of the account's own following only
//! (`engine::serve_stored`), the list keeps its walk's date and provenance,
//! and the run says how many of the account's own changes it applied. A
//! list walked after a write already holds it, so only writes newer than
//! the walk are applied.
//!
//! One row per write Instagram took that changed the relationship: a follow
//! that became a request, or one already so, changed no list.

use rusqlite::{Connection, params};

use super::{StoreError, pk_from_sql, pk_to_sql, users};
use snob_core::model::User;
use snob_core::{Epoch, Pk};

/// How long a row is kept. By then a newer walk has read the list, and a
/// list that old is not served without a warning about its age anyway.
const KEPT_SECS: i64 = 30 * 86_400;

/// Which of the two writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Follow,
    Unfollow,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Unfollow => "unfollow",
        }
    }
}

/// A change applied to a stored list: what was done, and to whom, by the
/// name the store has for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub verb: Verb,
    pub pk: Pk,
    pub username: Option<String>,
}

/// Records a write Instagram took, at `at`, and forgets the rows past
/// their keeping. In a transaction of the caller's, or on its own.
pub fn record(conn: &Connection, target: Pk, verb: Verb, at: Epoch) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO own_writes (at, target_pk, verb) VALUES (?1, ?2, ?3)",
        params![at.get(), pk_to_sql(target), verb.as_str()],
    )?;
    conn.execute(
        "DELETE FROM own_writes WHERE at < ?1",
        params![at.get() - KEPT_SECS],
    )?;
    Ok(())
}

/// The last write to each account made after `after`, oldest first: a
/// follow and then an unfollow of one account is the unfollow.
fn since(conn: &Connection, after: Epoch) -> Result<Vec<(Pk, Verb)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT target_pk, verb FROM own_writes w
         WHERE at > ?1
           AND id = (SELECT max(id) FROM own_writes v
                     WHERE v.target_pk = w.target_pk AND v.at > ?1)
         ORDER BY at, id",
    )?;
    let rows = stmt.query_map(params![after.get()], |row| {
        let verb: String = row.get(1)?;
        Ok((
            pk_from_sql(row.get(0)?),
            if verb == "follow" {
                Verb::Follow
            } else {
                Verb::Unfollow
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Applies to `members`, the account's own following as a walk that ended
/// at `taken_at` read it, the writes made since, and says which changed it:
/// an unfollowed account taken out, a followed one put first, where
/// Instagram lists the newest. A write the list already shows changes
/// nothing and is not said; a followed account the store has no row for is
/// left out, having no name to show.
pub fn apply(
    conn: &Connection,
    members: &mut Vec<User>,
    taken_at: Epoch,
) -> Result<Vec<Applied>, StoreError> {
    let mut applied = Vec::new();
    for (pk, verb) in since(conn, taken_at)? {
        let at = members.iter().position(|user| user.pk == pk);
        match (verb, at) {
            (Verb::Unfollow, Some(at)) => {
                let user = members.remove(at);
                applied.push(Applied {
                    verb,
                    pk,
                    username: Some(user.username),
                });
            }
            (Verb::Follow, None) => {
                if let Some(user) = users::find(conn, pk)? {
                    applied.push(Applied {
                        verb,
                        pk,
                        username: Some(user.username.clone()),
                    });
                    members.insert(0, user);
                }
            }
            _ => {}
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn user(pk: u64, name: &str) -> User {
        User::named(Pk::new(pk), name)
    }

    #[test]
    fn writes_after_the_walk_are_applied_and_earlier_ones_are_not() {
        let db = Store::in_memory().unwrap();
        for u in [user(1, "ana"), user(2, "bo"), user(3, "cy")] {
            users::upsert(db.conn(), &u).unwrap();
        }
        // Before the walk: the walk already shows it.
        record(db.conn(), Pk::new(1), Verb::Unfollow, Epoch::new(900)).unwrap();
        // After it: bo unfollowed, cy followed.
        record(db.conn(), Pk::new(2), Verb::Unfollow, Epoch::new(1_100)).unwrap();
        record(db.conn(), Pk::new(3), Verb::Follow, Epoch::new(1_200)).unwrap();

        let mut members = vec![user(1, "ana"), user(2, "bo")];
        let applied = apply(db.conn(), &mut members, Epoch::new(1_000)).unwrap();
        let names: Vec<&str> = members.iter().map(|u| u.username.as_str()).collect();
        assert_eq!(names, ["cy", "ana"]);
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].verb, Verb::Unfollow);
        assert_eq!(applied[0].username.as_deref(), Some("bo"));
    }

    #[test]
    fn the_last_write_to_an_account_is_the_one_that_counts() {
        let db = Store::in_memory().unwrap();
        users::upsert(db.conn(), &user(2, "bo")).unwrap();
        record(db.conn(), Pk::new(2), Verb::Unfollow, Epoch::new(1_100)).unwrap();
        record(db.conn(), Pk::new(2), Verb::Follow, Epoch::new(1_200)).unwrap();

        let mut members = vec![user(2, "bo")];
        let applied = apply(db.conn(), &mut members, Epoch::new(1_000)).unwrap();
        assert_eq!(members.len(), 1, "followed again: still there");
        assert!(applied.is_empty(), "and nothing changed to say");
    }
}
