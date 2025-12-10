use crate::config::FeedGroup;
use ammonia::clean_text;
use blake3::Hash;
use chrono::{DateTime, TimeDelta, Utc};
use color_eyre::{
    Result,
    eyre::{Report, WrapErr},
};
use sqlx::{PgExecutor, PgPool};

pub async fn init_db(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .wrap_err("Failed to run database migrations")
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

pub async fn is_feed_group_waiting(
    e: impl PgExecutor<'_>,
    feed_config: &FeedGroup,
) -> Result<bool> {
    let now = Utc::now();
    let update_cutoff = saturating_sub_datetime(now, feed_config.settings.interval);

    let waiting = sqlx::query_scalar!(
        "SELECT 1 AS \"waiting!\" FROM feed_groups WHERE urls_hash = $1 AND last_check > $2",
        feed_config.urls_hash.as_bytes(),
        update_cutoff,
    )
    .fetch_optional(e)
    .await?
    .is_some();

    Ok(waiting)
}

pub async fn try_check_feed_group(
    e: impl PgExecutor<'_>,
    feed_config: &FeedGroup,
) -> Result<String> {
    let now = Utc::now();
    let update_cutoff = saturating_sub_datetime(now, feed_config.settings.interval);

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

pub async fn clear_failure(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<()> {
    sqlx::query!(
        "DELETE FROM failures WHERE urls_hash = $1",
        urls_hash.as_bytes()
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn record_failure(e: impl PgExecutor<'_>, urls_hash: Hash, report: Report) -> Result<()> {
    let now = Utc::now();
    let ansi_error = format!("{report} ({now})\n{report:?}");
    let error = ansi_to_html::convert(&ansi_error).unwrap_or_else(|_| clean_text(&ansi_error));
    sqlx::query!(
        r#"
        INSERT INTO failures (urls_hash, fail_count, error, fail_time)
        VALUES ($1, 1, $2, $3)
        ON CONFLICT (urls_hash) DO UPDATE
            SET fail_count = failures.fail_count + 1, error = $2, fail_time = $3
        "#,
        urls_hash.as_bytes(),
        error,
        now,
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn delete_old_failures(e: impl PgExecutor<'_>, keep_old: TimeDelta) -> Result<()> {
    let cutoff = saturating_sub_datetime(Utc::now(), keep_old);
    let result = sqlx::query!("DELETE FROM failures WHERE fail_time < $1", cutoff)
        .execute(e)
        .await?;
    log::debug!(
        "Deleted {} failures older than {}",
        result.rows_affected(),
        cutoff,
    );
    Ok(())
}

pub async fn get_failing_feeds(e: impl PgExecutor<'_>) -> Result<Vec<(Hash, String)>> {
    sqlx::query!("SELECT urls_hash, error FROM failures WHERE fail_count >= 2")
        .fetch_all(e)
        .await?
        .into_iter()
        .map(|row| Ok((Hash::from_slice(&row.urls_hash)?, row.error)))
        .collect()
}

fn saturating_sub_datetime(dt: DateTime<Utc>, delta: TimeDelta) -> DateTime<Utc> {
    match dt.checked_sub_signed(delta) {
        Some(d) if d.timestamp() > 0 => d,
        _ => DateTime::UNIX_EPOCH,
    }
}
