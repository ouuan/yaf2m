ALTER TABLE feed_item_contents
    DROP COLUMN update_hash,
    DROP COLUMN first_seen;

ALTER TABLE feed_items DROP COLUMN first_seen;
