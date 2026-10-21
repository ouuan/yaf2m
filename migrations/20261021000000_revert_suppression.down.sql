ALTER TABLE feed_item_contents
    DROP COLUMN observed_hash,
    DROP COLUMN stable_since,
    DROP COLUMN stable_count,
    DROP COLUMN recent_reverts,
    DROP COLUMN suppressing;
