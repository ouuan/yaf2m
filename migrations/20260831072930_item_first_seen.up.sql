-- Existing rows have no record of when their version first showed up, but `last_seen` is the
-- closest thing to it and keeps the column `NOT NULL`.
ALTER TABLE feed_items ADD COLUMN first_seen TIMESTAMPTZ;
UPDATE feed_items SET first_seen = last_seen;
ALTER TABLE feed_items ALTER COLUMN first_seen SET NOT NULL;

-- These two stay nullable: rows stored before this migration genuinely do not know which version
-- their content came from, and `feed_item_contents.last_seen` is bumped every cycle, so
-- backfilling from it would stamp them with a bogus "just now" time. `NULL` reads as "unknown",
-- which suppresses revert detection until the row is next overwritten.
ALTER TABLE feed_item_contents
    ADD COLUMN update_hash BYTEA,
    ADD COLUMN first_seen TIMESTAMPTZ;
