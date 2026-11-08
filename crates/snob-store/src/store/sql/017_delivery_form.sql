-- What a queued report is, beside its bytes: the event its header names and
-- the type its body is sent as.
--
-- Until now every body was snob's JSON, and the event was read back out of
-- it (`event_of` in `commands/watch/delivery.rs`). A webhook can now be sent
-- the report's summary in the shape a chat service takes (`format` under
-- `[webhook]`): Slack's and Discord's JSON, or plain text for ntfy, none of
-- which names the event, and the plain text is no JSON at all. The body is
-- still stored as the exact bytes that were signed, so a retry sends the
-- shape it was queued in whatever the file says by then.
--
-- NULL in both is a row queued before this, which is snob's JSON: sent as
-- `application/json`, its event read from the body as before.
ALTER TABLE watch_deliveries ADD COLUMN event TEXT;
ALTER TABLE watch_deliveries ADD COLUMN content_type TEXT;
