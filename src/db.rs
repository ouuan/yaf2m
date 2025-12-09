use crate::config::FeedGroup;
use blake3::Hash;
use chrono::{DateTime, TimeDelta, Utc};
use color_eyre::Result;
use sqlx::{PgExecutor, PgPool};

pub async fn init_db(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;

    sqlx::query!(
        r#"
        CREATE TABLE IF NOT EXISTS feed_groups (
            urls_hash BYTEA PRIMARY KEY,
            last_check TIMESTAMPTZ NOT NULL,
            last_update TIMESTAMPTZ,
            last_seen TIMESTAMPTZ NOT NULL,
            fail_count INT NOT NULL DEFAULT 0
        )
        "#,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"
        CREATE TABLE IF NOT EXISTS feed_items (
            id BIGSERIAL PRIMARY KEY,
            urls_hash BYTEA NOT NULL REFERENCES feed_groups(urls_hash) ON DELETE CASCADE,
            update_hash BYTEA NOT NULL,
            last_seen TIMESTAMPTZ NOT NULL,
            UNIQUE(urls_hash, update_hash)
        )
        "#,
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn delete_old_groups(e: impl PgExecutor<'_>, keep_old: TimeDelta) -> Result<()> {
    let cutoff = saturating_sub_datetime(Utc::now(), keep_old);
    let result = sqlx::query!("DELETE FROM feed_groups WHERE last_seen < $1", cutoff)
        .execute(e)
        .await?;
    log::debug!(
        "Deleted {} feed groups older than {}",
        result.rows_affected(),
        cutoff,
    );
    Ok(())
}

pub async fn touch_feed_group_last_seen(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<()> {
    sqlx::query!(
        "UPDATE feed_groups SET last_seen = $1 WHERE urls_hash = $2",
        Utc::now(),
        urls_hash.as_bytes()
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn get_feed_group_fail_count(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<i32> {
    let count = sqlx::query_scalar!(
        "SELECT fail_count FROM feed_groups WHERE urls_hash = $1",
        urls_hash.as_bytes(),
    )
    .fetch_optional(e)
    .await?;
    Ok(count.unwrap_or(0))
}

pub async fn try_check_feed_group(
    e: impl PgExecutor<'_>,
    fail_count: i32,
    feed_config: &FeedGroup,
) -> Result<String> {
    let now = Utc::now();
    let mut update_cutoff = saturating_sub_datetime(now, feed_config.settings.interval);
    if fail_count > 0 {
        let wait = TimeDelta::minutes(1 << fail_count.min(12));
        update_cutoff = saturating_sub_datetime(update_cutoff, wait);
    }

    let status = sqlx::query_scalar!(
        r#"
        WITH upsert AS (
            INSERT INTO feed_groups (urls_hash, last_check, last_seen)
            VALUES ($1, $2, $2)
            ON CONFLICT (urls_hash)
            DO UPDATE SET last_check = $2
                WHERE feed_groups.last_check < $3
            RETURNING
                (xmax = 0) AS new
        )
        SELECT
            CASE
                WHEN EXISTS (SELECT 1 FROM upsert WHERE new) THEN 'new'
                WHEN EXISTS (SELECT 1 FROM upsert) THEN 'update'
                ELSE 'wait'
            END AS "status!"
        "#,
        feed_config.urls_hash.as_bytes(),
        now,
        update_cutoff,
    )
    .fetch_one(e)
    .await?;

    Ok(status)
}

pub async fn increment_feed_group_fail_count(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
) -> Result<()> {
    sqlx::query!(
        "UPDATE feed_groups SET fail_count = fail_count + 1 WHERE urls_hash = $1",
        urls_hash.as_bytes(),
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn reset_feed_group_fail_count(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<()> {
    sqlx::query!(
        "UPDATE feed_groups SET fail_count = 0 WHERE urls_hash = $1",
        urls_hash.as_bytes()
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn upsert_and_check_item_new(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    update_hash: Hash,
) -> Result<bool> {
    let new = sqlx::query_scalar!(
        r#"
        INSERT INTO feed_items (urls_hash, update_hash, last_seen)
        VALUES ($1, $2, $3)
        ON CONFLICT (urls_hash, update_hash) DO UPDATE
            SET last_seen = EXCLUDED.last_seen
        RETURNING (xmax = 0) as "new!"
        "#,
        urls_hash.as_bytes(),
        update_hash.as_bytes(),
        Utc::now(),
    )
    .fetch_one(e)
    .await?;
    Ok(new)
}

pub async fn delete_old_items(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    keep_old: TimeDelta,
) -> Result<()> {
    let cutoff = saturating_sub_datetime(Utc::now(), keep_old);
    let result = sqlx::query!(
        "DELETE FROM feed_items WHERE urls_hash = $1 AND last_seen < $2",
        urls_hash.as_bytes(),
        cutoff
    )
    .execute(e)
    .await?;
    log::debug!(
        "Deleted {} items older than {}",
        result.rows_affected(),
        cutoff,
    );
    Ok(())
}

pub async fn set_feed_group_update_time(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<()> {
    sqlx::query!(
        "UPDATE feed_groups SET last_update = $1 WHERE urls_hash = $2",
        Utc::now(),
        urls_hash.as_bytes()
    )
    .execute(e)
    .await?;

    Ok(())
}

fn saturating_sub_datetime(dt: DateTime<Utc>, delta: TimeDelta) -> DateTime<Utc> {
    match dt.checked_sub_signed(delta) {
        Some(d) if d.timestamp() > 0 => d,
        _ => DateTime::UNIX_EPOCH,
    }
}
