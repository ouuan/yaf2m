CREATE TABLE feed_groups (
    urls_hash BYTEA PRIMARY KEY,
    last_check TIMESTAMPTZ NOT NULL,
    last_update TIMESTAMPTZ,
    last_seen TIMESTAMPTZ NOT NULL
);

CREATE TABLE feed_items (
    id BIGSERIAL PRIMARY KEY,
    urls_hash BYTEA NOT NULL REFERENCES feed_groups(urls_hash) ON DELETE CASCADE,
    update_hash BYTEA NOT NULL,
    last_seen TIMESTAMPTZ NOT NULL,
    UNIQUE(urls_hash, update_hash)
);

CREATE TABLE failures (
    urls_hash BYTEA PRIMARY KEY,
    fail_count BIGINT NOT NULL,
    error TEXT NOT NULL
);
