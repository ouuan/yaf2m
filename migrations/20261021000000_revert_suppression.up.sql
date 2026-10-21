ALTER TABLE feed_item_contents
    ADD COLUMN observed_hash BYTEA,
    ADD COLUMN stable_since TIMESTAMPTZ,
    ADD COLUMN stable_count INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN recent_reverts TIMESTAMPTZ[] NOT NULL DEFAULT '{}',
    ADD COLUMN suppressing BOOLEAN NOT NULL DEFAULT FALSE;
