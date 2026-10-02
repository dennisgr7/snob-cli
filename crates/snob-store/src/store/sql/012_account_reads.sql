-- Accounts read off follower and following lists: one row per page, when it
-- arrived and how many accounts it carried.
--
-- The daily ceiling promises at most so many accounts in any 24 hours, and
-- that is a sum over the last day of this log. A GCRA bucket, the shape
-- `rate_budget` gives the request buckets, admits a whole day's burst and then
-- keeps admitting at the daily rate, which is about twice the ceiling in the
-- first day; and it stores time, so halving the ceiling rescaled what had
-- already been read the wrong way. Rows older than a day are pruned as new
-- ones are written.
CREATE TABLE account_reads (
  at_ms    INTEGER NOT NULL,
  accounts INTEGER NOT NULL
) STRICT;

CREATE INDEX account_reads_at ON account_reads (at_ms);

-- The bucket this replaces.
DELETE FROM rate_budget WHERE bucket = 'accounts';
