-- How long each answer of Instagram's API took to arrive, and the load it
-- declared: one row per answer read, written by the pacer as it arrives
-- (`snob_core::budget::AnswerTime`, `answers.rs`).
--
-- Two readers. The pace lengthens its waits when the latest healthy answers
-- were slow (`snob_core::budget::SlowRule`), from the rows of the last ten
-- minutes, so every process walking the account is slowed by the answers the
-- others got. And `snob status` shows the last day. A push-back is the moment
-- these rows are for: what the answers looked like before it is what nothing
-- recorded until now.
--
-- `operation` is the call's name in the allowlist, never its path, which
-- carries the account it was about. `waited_ms` is to the answer's headers,
-- from the moment the request left. `load` is `x-ig-capacity-level` and
-- `x-ig-peak-time` as `snob_ig` joins them, NULL when neither came; written
-- down, never acted on. `via` is `browser` or `direct`.
--
-- Rows older than a week are pruned as new ones are written.
CREATE TABLE answers (
  at_ms     INTEGER NOT NULL,
  operation TEXT NOT NULL,
  status    INTEGER NOT NULL,
  waited_ms INTEGER NOT NULL,
  load      TEXT,
  via       TEXT NOT NULL
) STRICT;

CREATE INDEX answers_at ON answers (at_ms);
