use crate::config::FeedGroup;
use crate::escape::escape_text;
use crate::revert::RevertState;
use blake3::Hash;
use chrono::{DateTime, TimeDelta, Utc};
use color_eyre::Result;
use color_eyre::eyre::{Report, WrapErr, eyre};
use sqlx::postgres::PgQueryResult;
use sqlx::{PgExecutor, PgPool};
use std::collections::HashMap;
use std::str::FromStr;

pub async fn init_db(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .wrap_err("Failed to run database migrations")
}

pub async fn delete_old_groups(
    e: impl PgExecutor<'_>,
    keep_old: TimeDelta,
    groups_in_config: &[Vec<u8>],
) -> Result<()> {
    let cutoff = saturating_sub_datetime(Utc::now(), keep_old);
    let result = sqlx::query!(
        "DELETE FROM feed_groups WHERE last_seen < $1 AND NOT (urls_hash = ANY($2))",
        cutoff,
        groups_in_config,
    )
    .execute(e)
    .await?;
    log_deletion("feed groups", result, cutoff);
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
        "SELECT 1 AS \"waiting!\" FROM feed_groups WHERE urls_hash = $1 AND last_check > $2 AND criteria_hash = $3",
        feed_config.urls_hash.as_bytes(),
        update_cutoff,
        feed_config.criteria_hash.as_bytes(),
    )
    .fetch_optional(e)
    .await?
    .is_some();

    Ok(waiting)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    NewFeed,
    NewCriteria,
    Update,
    Wait,
}

impl FromStr for FeedStatus {
    type Err = color_eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "new_feed" => Ok(FeedStatus::NewFeed),
            "new_criteria" => Ok(FeedStatus::NewCriteria),
            "update" => Ok(FeedStatus::Update),
            "wait" => Ok(FeedStatus::Wait),
            _ => Err(eyre!("Invalid feed status: {s}")),
        }
    }
}

pub async fn try_check_feed_group(
    e: impl PgExecutor<'_>,
    feed_config: &FeedGroup,
) -> Result<FeedStatus> {
    let now = Utc::now();
    let update_cutoff = saturating_sub_datetime(now, feed_config.settings.interval);
    let retry_cutoff = saturating_sub_datetime(now, feed_config.settings.retry_interval);

    sqlx::query_scalar!(
        r#"
        WITH upsert AS (
            INSERT INTO feed_groups (urls_hash, criteria_hash, last_check, last_seen)
            VALUES ($1, $2, $3, $3)
            ON CONFLICT (urls_hash)
            DO UPDATE SET last_check = $3, criteria_hash = $2
                WHERE (
                        feed_groups.last_check < $4
                        AND NOT EXISTS (
                            SELECT 1 FROM failures
                            WHERE failures.urls_hash = $1 AND failures.fail_time > $5
                        )
                    )
                    OR feed_groups.criteria_hash IS DISTINCT FROM $2
            RETURNING
                (xmax = 0) AS new_feed,
                (OLD.criteria_hash IS DISTINCT FROM $2) AS new_criteria
        )
        SELECT
            CASE
                WHEN EXISTS (SELECT 1 FROM upsert WHERE new_feed) THEN 'new_feed'
                WHEN EXISTS (SELECT 1 FROM upsert WHERE new_criteria) THEN 'new_criteria'
                WHEN EXISTS (SELECT 1 FROM upsert) THEN 'update'
                ELSE 'wait'
            END AS "status!"
        "#,
        feed_config.urls_hash.as_bytes(),
        feed_config.criteria_hash.as_bytes(),
        now,
        update_cutoff,
        retry_cutoff,
    )
    .fetch_one(e)
    .await?
    .parse()
}

pub struct ItemUpsert {
    pub new: bool,
    /// When this version of the item was first seen, which for a revert is an older date than the
    /// current cycle.
    pub first_seen: DateTime<Utc>,
}

pub async fn upsert_and_check_item_new(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    update_hash: Hash,
) -> Result<ItemUpsert> {
    // `first_seen` is deliberately absent from the `SET` list, so the conflict path returns the
    // value from the original sighting of this version rather than the current time.
    let row = sqlx::query!(
        r#"
        INSERT INTO feed_items (urls_hash, update_hash, first_seen, last_seen)
        VALUES ($1, $2, $3, $3)
        ON CONFLICT (urls_hash, update_hash) DO UPDATE
            SET last_seen = EXCLUDED.last_seen
        RETURNING (xmax = 0) AS "new!", first_seen AS "first_seen!"
        "#,
        urls_hash.as_bytes(),
        update_hash.as_bytes(),
        Utc::now(),
    )
    .fetch_one(e)
    .await?;
    Ok(ItemUpsert {
        new: row.new,
        first_seen: row.first_seen,
    })
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
    log_deletion("feed items", result, cutoff);
    Ok(())
}

/// The content stored under one `(urls_hash, diff_hash)`, i.e. the "before" side of a diff.
pub struct StoredContent {
    pub content: String,
    /// The version this content came from. `NULL` for rows written before the column existed,
    /// where it reads as "unknown" rather than "a different version".
    pub update_hash: Option<Hash>,
    /// A cache of the `feed_items.first_seen` of [`Self::update_hash`], written in lockstep with
    /// it so it cannot drift. Unlike a join back onto `feed_items`, it survives that row being
    /// pruned by `keep-old`.
    pub first_seen: Option<DateTime<Utc>>,
    pub revert_state: RevertState,
}

pub async fn get_item_content(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    diff_hash: Hash,
) -> Result<Option<StoredContent>> {
    sqlx::query!(
        r#"
        SELECT content, update_hash, first_seen,
            observed_hash, stable_since, stable_count, recent_reverts, suppressing
        FROM feed_item_contents
        WHERE urls_hash = $1 AND diff_hash = $2
        "#,
        urls_hash.as_bytes(),
        diff_hash.as_bytes(),
    )
    .fetch_optional(e)
    .await?
    .map(|row| {
        Ok(StoredContent {
            content: row.content,
            update_hash: row
                .update_hash
                .as_deref()
                .map(Hash::from_slice)
                .transpose()?,
            first_seen: row.first_seen,
            revert_state: RevertState {
                observed_hash: row
                    .observed_hash
                    .as_deref()
                    .map(Hash::from_slice)
                    .transpose()?,
                stable_since: row.stable_since,
                stable_count: row.stable_count,
                recent_reverts: row.recent_reverts,
                suppressing: row.suppressing,
            },
        })
    })
    .transpose()
}

/// Store `content` under `(urls_hash, diff_hash)`, tagged with the version it came from.
///
/// `overwrite = false` only seeds a missing row and touches `last_seen`, so the stored content
/// always stays at whatever was last mailed out. The touch is unconditional because it is what
/// keeps the row alive against `keep-old`.
pub async fn store_item_content(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    diff_hash: Hash,
    content: &str,
    update_hash: Hash,
    first_seen: DateTime<Utc>,
    overwrite: bool,
) -> Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO feed_item_contents
            (urls_hash, diff_hash, content, update_hash, first_seen, last_seen)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (urls_hash, diff_hash) DO UPDATE
            SET content = CASE WHEN $7 THEN EXCLUDED.content ELSE feed_item_contents.content END,
                update_hash =
                    CASE WHEN $7 THEN EXCLUDED.update_hash ELSE feed_item_contents.update_hash END,
                first_seen =
                    CASE WHEN $7 THEN EXCLUDED.first_seen ELSE feed_item_contents.first_seen END,
                last_seen = EXCLUDED.last_seen
        "#,
        urls_hash.as_bytes(),
        diff_hash.as_bytes(),
        content,
        update_hash.as_bytes(),
        first_seen,
        Utc::now(),
        overwrite,
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn delete_old_item_contents(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    keep_old: TimeDelta,
) -> Result<()> {
    let cutoff = saturating_sub_datetime(Utc::now(), keep_old);
    let result = sqlx::query!(
        "DELETE FROM feed_item_contents WHERE urls_hash = $1 AND last_seen < $2",
        urls_hash.as_bytes(),
        cutoff
    )
    .execute(e)
    .await?;
    log_deletion("feed item contents", result, cutoff);
    Ok(())
}

/// Persist observations even when a notification was suppressed. Called in the same transaction
/// as the content write and mail send, so a failed check cannot consume a transition.
pub async fn store_revert_state(
    e: impl PgExecutor<'_>,
    urls_hash: Hash,
    diff_hash: Hash,
    state: &RevertState,
) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE feed_item_contents
        SET observed_hash = $3, stable_since = $4, stable_count = $5,
            recent_reverts = $6, suppressing = $7
        WHERE urls_hash = $1 AND diff_hash = $2
        "#,
        urls_hash.as_bytes(),
        diff_hash.as_bytes(),
        state
            .observed_hash
            .as_ref()
            .map(|hash| hash.as_bytes().as_slice()),
        state.stable_since,
        state.stable_count,
        &state.recent_reverts,
        state.suppressing,
    )
    .execute(e)
    .await?;
    Ok(())
}

pub async fn reset_revert_states(e: impl PgExecutor<'_>, urls_hash: Hash) -> Result<()> {
    sqlx::query!(
        r#"
        UPDATE feed_item_contents
        SET observed_hash = NULL, stable_since = NULL, stable_count = 0,
            recent_reverts = '{}', suppressing = FALSE
        WHERE urls_hash = $1
        "#,
        urls_hash.as_bytes(),
    )
    .execute(e)
    .await?;
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
    let ansi_error = format!("Error at {now}:{report:?}");
    let error = ansi_to_html::convert(&ansi_error).unwrap_or_else(|_| escape_text(&ansi_error));
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
    log_deletion("failures", result, cutoff);
    Ok(())
}

pub async fn get_failing_feeds(
    e: impl PgExecutor<'_>,
    retry_count_by_feed: &HashMap<Hash, usize>,
    default_retry_count: usize,
) -> Result<Vec<(Hash, String)>> {
    let effective_default = default_retry_count.max(1);
    sqlx::query!("SELECT urls_hash, error, fail_count FROM failures")
        .fetch_all(e)
        .await?
        .into_iter()
        .map(|row| {
            let urls_hash = Hash::from_slice(&row.urls_hash)?;
            let retry_count = retry_count_by_feed
                .get(&urls_hash)
                .copied()
                .unwrap_or(effective_default)
                .max(1);
            Ok((row.fail_count as u64 >= retry_count as u64).then_some((urls_hash, row.error)))
        })
        .filter_map(Result::transpose)
        .collect()
}

fn saturating_sub_datetime(dt: DateTime<Utc>, delta: TimeDelta) -> DateTime<Utc> {
    match dt.checked_sub_signed(delta) {
        Some(d) if d.timestamp() > 0 => d,
        _ => DateTime::UNIX_EPOCH,
    }
}

fn log_deletion(name: &str, result: PgQueryResult, cutoff: DateTime<Utc>) {
    let count = result.rows_affected();
    let level = if count == 0 {
        log::Level::Debug
    } else {
        log::Level::Info
    };
    log::log!(level, "Deleted {count} {name} older than {cutoff}",);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Run only against a disposable PostgreSQL database. This exercises the actual migration,
    /// persistence across connections, and transaction boundary used around mail sends.
    #[tokio::test]
    #[ignore = "requires a disposable database in YAF2M_TEST_DATABASE_URL"]
    async fn revert_state_migration_persistence_and_rollback() -> Result<()> {
        let url = std::env::var("YAF2M_TEST_DATABASE_URL")?;
        let pool = PgPool::connect(&url).await?;
        init_db(&pool).await?;
        let urls_hash = blake3::hash(Utc::now().to_rfc3339().as_bytes());
        let other_group = blake3::hash(urls_hash.as_bytes());
        let diff_hash = blake3::hash(b"item");
        let version = blake3::hash(b"A");
        let now = Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap();
        let mut tx = pool.begin().await?;
        for group in [urls_hash, other_group] {
            sqlx::query(
                "INSERT INTO feed_groups (urls_hash, last_check, last_seen) VALUES ($1, $2, $2)",
            )
            .bind(group.as_bytes().as_slice())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            store_item_content(&mut *tx, group, diff_hash, "A", version, now, true).await?;
        }
        // Downgrade and upgrade a populated table: existing notification baselines survive and
        // begin with an empty observation history rather than a fabricated one.
        sqlx::raw_sql(include_str!(
            "../migrations/20261021000000_revert_suppression.down.sql"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(include_str!(
            "../migrations/20261021000000_revert_suppression.up.sql"
        ))
        .execute(&mut *tx)
        .await?;
        let legacy = get_item_content(&mut *tx, urls_hash, diff_hash)
            .await?
            .unwrap();
        assert_eq!(legacy.content, "A");
        assert_eq!(legacy.update_hash, Some(version));
        assert_eq!(legacy.first_seen, Some(now));
        assert_eq!(legacy.revert_state, RevertState::default());

        let state = RevertState {
            observed_hash: Some(blake3::hash(b"B")),
            stable_since: Some(now),
            stable_count: 2,
            recent_reverts: vec![now - TimeDelta::hours(1), now],
            suppressing: true,
        };
        for group in [urls_hash, other_group] {
            store_revert_state(&mut *tx, group, diff_hash, &state).await?;
        }
        // A suppressed observation updates state but leaves all notification metadata intact.
        store_item_content(
            &mut *tx,
            urls_hash,
            diff_hash,
            "B",
            blake3::hash(b"B"),
            now,
            false,
        )
        .await?;
        tx.commit().await?;
        pool.close().await;

        let pool = PgPool::connect(&url).await?;
        let restored = get_item_content(&pool, urls_hash, diff_hash)
            .await?
            .unwrap();
        assert_eq!(restored.revert_state, state);
        assert_eq!(restored.content, "A");
        assert_eq!(restored.update_hash, Some(version));
        assert_eq!(restored.first_seen, Some(now));

        let mut tx = pool.begin().await?;
        let update_hash = blake3::hash(b"uncommitted version");
        assert!(
            upsert_and_check_item_new(&mut *tx, urls_hash, update_hash)
                .await?
                .new
        );
        store_item_content(&mut *tx, urls_hash, diff_hash, "C", update_hash, now, true).await?;
        store_revert_state(&mut *tx, urls_hash, diff_hash, &RevertState::default()).await?;
        // A failed mail send rolls back both state and the baseline, and keeps a new version new.
        tx.rollback().await?;
        let restored = get_item_content(&pool, urls_hash, diff_hash)
            .await?
            .unwrap();
        assert_eq!(restored.content, "A");
        assert_eq!(restored.revert_state, state);
        let mut tx = pool.begin().await?;
        assert!(
            upsert_and_check_item_new(&mut *tx, urls_hash, update_hash)
                .await?
                .new
        );
        tx.rollback().await?;

        reset_revert_states(&pool, urls_hash).await?;
        let reset = get_item_content(&pool, urls_hash, diff_hash)
            .await?
            .unwrap();
        assert_eq!(reset.revert_state, RevertState::default());
        assert_eq!(reset.content, "A");
        assert_eq!(reset.update_hash, Some(version));
        assert_eq!(
            get_item_content(&pool, other_group, diff_hash)
                .await?
                .unwrap()
                .revert_state,
            state,
        );
        for group in [urls_hash, other_group] {
            sqlx::query("DELETE FROM feed_groups WHERE urls_hash = $1")
                .bind(group.as_bytes().as_slice())
                .execute(&pool)
                .await?;
        }
        pool.close().await;
        Ok(())
    }
}
