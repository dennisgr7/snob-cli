-- The follows and unfollows this account made through snob, one row per
-- write Instagram took and that changed the relationship (`own_writes.rs`).
--
-- A stored following list is a walk's, and a walk is a moment: after
-- `snob unfollow someone`, `snob unfollowers --offline` (or an agent's stored
-- list) went on naming them until the next walk. A stored list read for the
-- account's own following is now handed out with the writes made since its
-- walk applied, and said to be so, never as if it had been walked again.
--
-- `verb` is `follow` or `unfollow`. `target_pk` names the account and is no
-- reference to `users`, so a row never stops anything being pruned; the name
-- shown is read from `users` when there is one. Rows older than a month are
-- pruned as new ones are written: by then a newer walk has read the list.
CREATE TABLE own_writes (
  id        INTEGER PRIMARY KEY,
  at        INTEGER NOT NULL,
  target_pk INTEGER NOT NULL,
  verb      TEXT NOT NULL CHECK (verb IN ('follow', 'unfollow'))
) STRICT;

CREATE INDEX own_writes_at ON own_writes (at);
