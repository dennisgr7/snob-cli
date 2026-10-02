-- What every account shares, in data/shared.db: its own file and its own
-- migration chain, apart from the accounts' databases.
--
-- One row per push-back any account received. Each account keeps its own
-- cooldown in its own database; these rows are how each one sees the others',
-- so push-backs on two accounts close together stop all of them
-- (`budget::common_brake`). Rows older than two days are pruned as new ones
-- are written: no cooldown lasts longer than a day.
CREATE TABLE pushbacks (
  pk       INTEGER NOT NULL,   -- the account pushed back on
  at_ms    INTEGER NOT NULL,
  until_ms INTEGER NOT NULL,   -- when the cooldown it started there ends
  reason   TEXT    NOT NULL
) STRICT;

CREATE INDEX pushbacks_at ON pushbacks (at_ms);

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
