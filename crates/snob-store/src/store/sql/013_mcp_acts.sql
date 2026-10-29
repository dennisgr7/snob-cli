-- What an agent was told yes to through `snob mcp`: one row per command a
-- child ran on a yes the server obtained (a follow, an unfollow, a walk of
-- another account's lists), written by the child **before its first request**,
-- in a transaction of its own. A write Instagram then refused, or one the
-- command found nothing to do for, still counts: the row is the attempt, and
-- the daily cap on an agent's writes counts attempts.
--
-- `action` is the command (`follow`, `unfollow`, `followers`, `fans`, ...).
-- `target_name` is the username as the agent's call named it, validated by the
-- server; `target_pk` is filled once the command resolved it, and stays NULL
-- for a list, whose account the walk resolves. `via` is how the yes was given
-- (`form`, `elicitation`, `chat`), `client` the client's name and version as it
-- declared them, reduced by the server to a few plain characters.
--
-- Rows older than a week are pruned as new ones are written; `snob status`
-- shows the last day, which is the window the cap counts.
CREATE TABLE mcp_acts (
  id          INTEGER PRIMARY KEY,
  at_ms       INTEGER NOT NULL,
  action      TEXT NOT NULL,
  target_pk   INTEGER,
  target_name TEXT,
  via         TEXT NOT NULL,
  client      TEXT
) STRICT;

CREATE INDEX mcp_acts_at ON mcp_acts (at_ms);
