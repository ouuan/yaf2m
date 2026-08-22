CREATE TABLE feed_item_contents (
    urls_hash BYTEA NOT NULL REFERENCES feed_groups(urls_hash) ON DELETE CASCADE,
    diff_hash BYTEA NOT NULL,
    content TEXT NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (urls_hash, diff_hash)
);
