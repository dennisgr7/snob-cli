-- The sitting of list pages this account is in, and the rest it owes: one
-- row, rewritten by every page any walk of the account reads
-- (`snob_core::budget::Sitting`, `SqliteRateBudget::sitting_page`).
--
-- Kept here rather than counted in the process because the rest has to
-- outlive it. Counted in the process, a run stopped during its rest, a walk
-- that ended on its page cap at the page that filled a sitting, and an agent
-- reading a list a page at a time each began a new sitting with no rest at
-- all. `rest_until` is written once, by the page that fills the sitting, as
-- when the rest it drew ends; the next page any process walks waits for it,
-- and the sitting starts again after. NULL until the sitting is full.
--
-- `pages` is a count, not a u64 column: a sitting holds forty.
CREATE TABLE sitting (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  pages        INTEGER NOT NULL,
  last_page_at INTEGER NOT NULL,
  rest_until   INTEGER
) STRICT;
