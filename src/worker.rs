use crate::config::{FeedGroup, load_config};
use crate::db::{self, FeedStatus, StoredContent};
use crate::diff::{DiffOptions, render_diff};
use crate::email::{Mail, Mailer, send_email_with_backoff};
use crate::feed::{FeedItemContext, ItemRenderContext, ItemWithDiff, fetch_feed};
use crate::render::{Renderer, TemplateName};
use blake3::{Hash, Hasher};
use chrono::{TimeDelta, Utc};
use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use lettre::message::Mailbox;
use minijinja::{Environment, render};
use minijinja_contrib::add_to_environment;
use serde::Serialize;
use sqlx::PgPool;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::task::JoinSet;

pub struct Worker {
    pool: PgPool,
    config_path: PathBuf,
    mailer: Mailer,
}

impl Worker {
    pub fn new<P: Into<PathBuf>>(pool: PgPool, config_path: P, mailer: Mailer) -> Self {
        Self {
            pool,
            config_path: config_path.into(),
            mailer,
        }
    }

    pub async fn run(self) -> Result<()> {
        let this = Arc::new(self);
        let mut feeds = Vec::new();
        let mut feed_map = HashMap::new();
        let mut retry_count_by_feed = HashMap::new();
        let mut feed_hashes = Vec::new();
        let mut keep_old = TimeDelta::default();
        let mut default_retry_count = 2usize;
        let mut last_modified = SystemTime::UNIX_EPOCH;
        let mut failure_tracker = FailureTracker::new();

        loop {
            let modified = tokio::fs::metadata(&this.config_path)
                .await
                .wrap_err("failed to get config file metadata")?
                .modified()?;
            if modified != last_modified {
                let config = load_config(&this.config_path).await?;
                log::info!("Config file update reloaded");
                feeds = config.feeds.into_iter().map(Arc::new).collect();
                feed_map = feeds.iter().map(|feed| (feed.urls_hash, feed)).collect();
                retry_count_by_feed = feeds
                    .iter()
                    .map(|feed| (feed.urls_hash, feed.settings.retry_count))
                    .collect();
                feed_hashes = feeds
                    .iter()
                    .map(|feed| feed.urls_hash.as_bytes().to_vec())
                    .collect();
                keep_old = config.global_settings.keep_old;
                default_retry_count = config.global_settings.retry_count;
                failure_tracker.set_report_to(config.error_report_to);
                last_modified = modified;
            }

            let mut set = JoinSet::new();

            for feed in feeds.iter().map(Arc::clone) {
                let worker = Arc::clone(&this);
                set.spawn(async move {
                    if let Err(e) = worker.process_feed(&feed).await {
                        log::warn!("Error processing feed group {:?}: {e}", feed.urls);
                        log::debug!("Error details: {}", format!("{e:?}").replace('\n', "\\n"));
                        match db::is_feed_group_waiting(&worker.pool, &feed).await {
                            Err(e) => log::error!(
                                "Failed to check if feed group {:?} is waiting: {e:?}",
                                feed.urls
                            ),
                            Ok(true) => log::info!(
                                "Error happened while feed group {:?} is still waiting: {e:?}",
                                feed.urls
                            ),
                            Ok(false) => {
                                if let Err(e) =
                                    db::record_failure(&worker.pool, feed.urls_hash, e).await
                                {
                                    log::error!("Failed to record error: {e:?}");
                                }
                            }
                        }
                    }
                });
            }

            while let Some(res) = set.join_next().await {
                if let Err(e) = res {
                    log::error!("Task panicked: {e:?}");
                }
            }

            match db::get_failing_feeds(&this.pool, &retry_count_by_feed, default_retry_count).await
            {
                Ok(failures) => {
                    let failures = failures
                        .into_iter()
                        .filter_map(|(urls_hash, error)| {
                            feed_map
                                .get(&urls_hash)
                                .map(|feed| (Arc::clone(feed), error))
                        })
                        .collect::<Vec<_>>();
                    log::log!(
                        if failures.is_empty() {
                            log::Level::Debug
                        } else {
                            log::Level::Warn
                        },
                        "{} feeds are failing",
                        failures.len()
                    );
                    failure_tracker.record(failures, &this.mailer).await;
                }
                Err(e) => log::error!("Failed to get failing feeds: {e:?}"),
            }

            db::delete_old_groups(&this.pool, keep_old, &feed_hashes)
                .await
                .inspect_err(|e| {
                    log::error!("Failed to delete old feed groups: {e:?}");
                })
                .ok();

            db::delete_old_failures(&this.pool, keep_old)
                .await
                .inspect_err(|e| {
                    log::error!("Failed to delete old failures: {e:?}");
                })
                .ok();

            log::debug!("Worker cycle completed, sleeping for 1 minute");

            tokio::time::sleep(Duration::from_mins(1)).await;
        }
    }

    async fn process_feed(&self, feed_group: &FeedGroup) -> Result<()> {
        log::debug!("Feed group {:?} started", feed_group.urls);

        db::touch_feed_group_last_seen(&self.pool, feed_group.urls_hash).await?;

        let mut tx = self.pool.begin().await?;

        let status = db::try_check_feed_group(&mut *tx, feed_group).await?;
        log::debug!("Feed group {:?} status: {status:?}", feed_group.urls);

        if status == FeedStatus::Wait {
            return Ok(());
        }

        let renderer = Renderer::from_feed(feed_group)?;

        let mut all_feeds = Vec::new();

        // reverse order to prioritize earlier URLs
        // otherwise, if the feeds update during fetching, later URLs may override earlier ones
        for url in feed_group.urls.iter().rev() {
            let feed = fetch_feed(url, &feed_group.settings)
                .await
                .wrap_err_with(|| format!("failed to fetch feed from {url}"))?;
            log::trace!("Fetched feed from {url}: {:?}", feed.borrow_feed());
            all_feeds.push(feed);
        }
        all_feeds.reverse();

        let mut new_items = Vec::new();
        let mut items_by_diff_hash = HashMap::new();

        for item in all_feeds.iter().flat_map(|feed| feed.borrow_items()) {
            if !renderer.filter(item)? {
                log::trace!(
                    "Item filtered out:\n{}",
                    render!("{{ item }}", item => item.item)
                );
                continue;
            }

            let update_hash = renderer.update_hash(item)?;
            let diff_input = renderer.diff_input(item)?;

            // Checked before anything is written: the rollback would undo the writes anyway, but
            // this also skips rendering a diff for an item that is about to be discarded.
            if let Some((diff_hash, _)) = &diff_input {
                check_diff_key_collision(&mut items_by_diff_hash, *diff_hash, update_hash, *item)?;
            }

            let upsert =
                db::upsert_and_check_item_new(&mut *tx, feed_group.urls_hash, update_hash).await?;

            let mut decision = ItemDecision::not_notified(upsert.new);
            let mut diff_from = None;

            if let Some((diff_hash, content)) = diff_input {
                let stored =
                    db::get_item_content(&mut *tx, feed_group.urls_hash, diff_hash).await?;
                decision = decide_item(
                    upsert.new,
                    update_hash,
                    stored.as_ref(),
                    &content,
                    feed_group.settings.diff_options,
                );
                if decision.diff.is_some() {
                    diff_from = stored.as_ref().and_then(|s| s.first_seen);
                }
                // The stored content is overwritten only when the item is actually notified, so a
                // diff always covers everything since the last mail about this item. The overwrite
                // rides on this transaction, so a failed send leaves it for the next cycle.
                db::store_item_content(
                    &mut *tx,
                    feed_group.urls_hash,
                    diff_hash,
                    &content,
                    update_hash,
                    upsert.first_seen,
                    decision.notify,
                )
                .await?;
            }

            log::trace!(
                "hash: {}, new: {}, reverted: {}, item:\n{}",
                update_hash,
                upsert.new,
                decision.reverted,
                render!("{{ item }}", item => item.item)
            );

            if decision.notify {
                let diff_to = decision.diff.is_some().then_some(upsert.first_seen);
                new_items.push(ItemRenderContext {
                    feed: item.feed,
                    item: ItemWithDiff {
                        entry: item.item,
                        diff: decision.diff,
                        diff_from,
                        diff_to,
                        reverted: decision.reverted,
                    },
                });
            }
        }

        if feed_group.settings.sort_by_last_modified {
            new_items.sort_by_key(|c| Reverse(c.item.entry.updated.or(c.item.entry.published)));
        }

        let reverted_count = new_items.iter().filter(|c| c.item.reverted).count();

        log::log!(
            if new_items.is_empty() {
                log::Level::Debug
            } else {
                log::Level::Info
            },
            "Feed group {:?}: {} new items and {} reverted items found",
            feed_group.urls,
            new_items.len() - reverted_count,
            reverted_count,
        );

        // Send emails
        if !new_items.is_empty() {
            let mails = if matches!(status, FeedStatus::NewFeed | FeedStatus::NewCriteria)
                || feed_group.settings.digest
                || new_items.len() > feed_group.settings.max_mails_per_check
            {
                let feeds = all_feeds
                    .iter()
                    .map(|feed| feed.borrow_feed())
                    .collect::<Vec<_>>();
                let ctx = minijinja::context! { feeds => feeds, items => &new_items };
                let subject_prefix = match status {
                    FeedStatus::NewFeed => "[New Feed] ",
                    FeedStatus::NewCriteria => "[New Criteria] ",
                    _ => "",
                };
                let subject = format!(
                    "{subject_prefix}{}",
                    renderer.render(TemplateName::DigestSubject, &ctx)?
                );
                let body = renderer.render(TemplateName::DigestBody, &ctx)?;
                vec![Mail { subject, body }]
            } else {
                new_items
                    .iter()
                    .map(|item| {
                        let subject_prefix = if item.item.reverted {
                            "[Reverted] "
                        } else {
                            ""
                        };
                        let subject = format!(
                            "{subject_prefix}{}",
                            renderer.render(TemplateName::ItemSubject, item)?
                        );
                        let body = renderer.render(TemplateName::ItemBody, item)?;
                        Ok(Mail { subject, body })
                    })
                    .collect::<Result<_>>()?
            };

            let mail_count = mails.len();

            if feed_group.settings.to.is_empty()
                && feed_group.settings.cc.is_empty()
                && feed_group.settings.bcc.is_empty()
            {
                log::warn!(
                    "No recipients specified for feed group {:?}",
                    feed_group.urls
                );
            } else {
                send_email_with_backoff(
                    &self.mailer,
                    &feed_group.settings.to,
                    &feed_group.settings.cc,
                    &feed_group.settings.bcc,
                    mails,
                )
                .await?;
                log::info!("Feed group {:?}: Sent {mail_count} emails", feed_group.urls);
            }

            db::set_feed_group_update_time(&mut *tx, feed_group.urls_hash).await?;
        }

        db::clear_failure(&mut *tx, feed_group.urls_hash).await?;

        db::delete_old_items(&mut *tx, feed_group.urls_hash, feed_group.settings.keep_old).await?;

        db::delete_old_item_contents(&mut *tx, feed_group.urls_hash, feed_group.settings.keep_old)
            .await?;

        tx.commit().await?;

        Ok(())
    }
}

/// What one item is worth reporting, once its stored content has been read back.
struct ItemDecision {
    diff: Option<String>,
    reverted: bool,
    notify: bool,
}

impl ItemDecision {
    /// The verdict for an item with no `diff-keys` configured, where nothing but a new
    /// `update_hash` can trigger a mail.
    fn not_notified(new: bool) -> Self {
        Self {
            diff: None,
            reverted: false,
            notify: new,
        }
    }
}

/// Decide whether to mail about an item and what diff to show, without touching the database.
///
/// A revert is version-based, not content-based: the stored content has to have come from a
/// *different* `update_hash` than the current one. Content-based detection would flag every change
/// that `update-keys` deliberately ignores — a title-only edit under the canonical
/// `update-keys = ['item.id', 'item.content.body']`, say — and would break the invariant that the
/// stored content is whatever was last mailed out.
fn decide_item(
    new: bool,
    update_hash: Hash,
    stored: Option<&StoredContent>,
    content: &str,
    opts: DiffOptions,
) -> ItemDecision {
    // The diff is rendered only for a notification candidate, so the common "nothing to report"
    // path keeps costing nothing.
    let candidate = new || stored.is_some_and(|s| s.update_hash.is_some_and(|h| h != update_hash));
    let diff = candidate
        .then(|| stored.and_then(|s| render_diff(&s.content, content, opts)))
        .flatten();
    // Gating the revert on a rendered diff is load-bearing: `render_diff` also returns `None` for
    // equal sides, oversized input, and — under `diff-strip-tags` — changes that were pure markup.
    // Mailing "[Reverted]" with no diff would show the full current content of an item the user
    // has already seen, with no explanation. A new `update_hash` still always mails, diff or not.
    let reverted = !new && diff.is_some();
    ItemDecision {
        diff,
        reverted,
        notify: new || reverted,
    }
}

/// Fail the whole feed group when two distinct items collapse onto one `diff_hash`.
///
/// They would otherwise overwrite each other's stored content every cycle and diff one item
/// against the other, silently. Fed from every filtered item rather than only the ones that reach
/// the database, so which cycle first observes the clash does not change the outcome.
fn check_diff_key_collision<'a>(
    seen: &mut HashMap<Hash, (Hash, FeedItemContext<'a>)>,
    diff_hash: Hash,
    update_hash: Hash,
    item: FeedItemContext<'a>,
) -> Result<()> {
    // The same item repeated across the URLs of a group hashes to the same pair, which is fine.
    let Some((other_update_hash, other)) = seen.insert(diff_hash, (update_hash, item)) else {
        return Ok(());
    };
    if other_update_hash == update_hash {
        return Ok(());
    }
    let title = |ctx: &FeedItemContext| ctx.item.title.as_ref().map(|t| t.content.clone());
    Err(eyre!(
        "Two items share the diff key hash {diff_hash}:\n\
         - update hash {other_update_hash}, id {other_id:?}, title {other_title:?}\n\
         - update hash {update_hash}, id {id:?}, title {title:?}\n\
         `diff-keys` has to identify one item uniquely across every URL of the group, otherwise \
         the two would overwrite each other's stored content and be diffed against each other.",
        other_id = other.item.id,
        other_title = title(&other),
        id = item.item.id,
        title = title(&item),
    ))
}

struct FailureTracker {
    failing_hash: Hash,
    debouncing_hash: Hash,
    debounce_count: u8,
    report_to: Vec<Mailbox>,
    minijinja_env: Environment<'static>,
}

const FAILURE_REPORT_TEMPLATE: &str = r#"
<div>🔴 {{ failures | length }} feed{{ failures | pluralize(" is", "s are") }} not working ({{ now() | datetimeformat(format="iso") }}):
<ul>
  {% for failure in failures %}
  <li>
    URL{{ failure.urls | pluralize }}: {{ failure.urls | join(", ") }}<br>
    <blockquote><pre>{{ failure.error | safe }}</pre></blockquote>
  </li>
  {% endfor %}
</ul>
</div>
"#;
const FAILURE_REPORT_TEMPLATE_NAME: &str = "failure-report.html";

impl FailureTracker {
    const DEBOUNCE_TIMES: u8 = 5;

    fn new() -> Self {
        let mut minijinja_env = Environment::new();
        add_to_environment(&mut minijinja_env);
        minijinja_env
            .add_template(FAILURE_REPORT_TEMPLATE_NAME, FAILURE_REPORT_TEMPLATE)
            .expect("failed to add failure report template");
        let empty_hash = Hasher::new().finalize();
        Self {
            failing_hash: empty_hash,
            debouncing_hash: empty_hash,
            debounce_count: 0,
            report_to: Vec::new(),
            minijinja_env,
        }
    }

    fn set_report_to(&mut self, report_to: Vec<Mailbox>) {
        self.report_to = report_to;
    }

    async fn record(&mut self, mut failures: Vec<(Arc<FeedGroup>, String)>, mailer: &Mailer) {
        failures.sort_unstable_by_key(|(feed, _)| *feed.urls_hash.as_bytes());
        let failing_hash = failures
            .iter()
            .fold(Hasher::new(), |mut hasher, (feed, _)| {
                hasher.update(feed.urls_hash.as_bytes());
                hasher
            })
            .finalize();
        if failing_hash == self.debouncing_hash {
            if self.debounce_count == 1 && failing_hash != self.failing_hash {
                if let Err(e) = self.send_failure_report(failures, mailer).await {
                    log::error!("Failed to send failure report email: {e:?}");
                    return;
                }
                self.failing_hash = failing_hash;
            }
            self.debounce_count = self.debounce_count.saturating_sub(1);
        } else {
            log::info!("Failing feed groups changed ({} failures)", failures.len(),);
            self.debouncing_hash = failing_hash;
            self.debounce_count = Self::DEBOUNCE_TIMES;
        }
    }

    async fn send_failure_report(
        &self,
        failures: Vec<(Arc<FeedGroup>, String)>,
        mailer: &Mailer,
    ) -> Result<()> {
        if self.report_to.is_empty() {
            return Ok(());
        }
        log::info!(
            "Sending failure report email for {} failing feed groups",
            failures.len(),
        );
        let mail = if failures.is_empty() {
            Mail {
                subject: "✅ All feeds are working".to_string(),
                body: format!(
                    "All feeds are back to normal now ({}).",
                    Utc::now().to_rfc3339()
                ),
            }
        } else {
            let failure_ctx = failures
                .iter()
                .map(|failure| FailureCtx {
                    urls: &failure.0.urls,
                    error: failure.1.to_string(),
                })
                .collect::<Vec<_>>();
            let body = self
                .minijinja_env
                .get_template(FAILURE_REPORT_TEMPLATE_NAME)
                .expect("failed to load failure report template")
                .render(minijinja::context! { failures => failure_ctx })
                .expect("failed to render failure report");
            Mail {
                subject: "🔴 Error processing feeds".into(),
                body,
            }
        };
        send_email_with_backoff(mailer, &self.report_to, &[], &[], vec![mail]).await
    }
}

#[derive(Serialize)]
struct FailureCtx<'a> {
    urls: &'a [String],
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffContext, DiffContextKeyword, DiffGranularity};
    use chrono::{DateTime, TimeZone};
    use feed_rs::model::{Entry, Feed, FeedType, Text};
    use minijinja::Value;
    use std::collections::BTreeMap;

    fn hash_of(s: &str) -> Hash {
        blake3::hash(s.as_bytes())
    }

    fn time(month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, month, day, 5, 6, 0).unwrap()
    }

    fn options(strip_tags: bool) -> DiffOptions {
        DiffOptions {
            granularity: DiffGranularity::Word,
            context: DiffContext::Keyword(DiffContextKeyword::Auto),
            strip_tags,
        }
    }

    fn stored(content: &str, update_hash: Option<Hash>) -> StoredContent {
        StoredContent {
            content: content.into(),
            update_hash,
            first_seen: Some(time(1, 2)),
        }
    }

    fn sample_feed() -> Feed {
        Feed {
            feed_type: FeedType::RSS2,
            id: "https://example.com/feed.xml".into(),
            title: None,
            updated: None,
            authors: Vec::new(),
            description: None,
            links: Vec::new(),
            categories: Vec::new(),
            contributors: Vec::new(),
            generator: None,
            icon: None,
            language: None,
            logo: None,
            published: None,
            rating: None,
            rights: None,
            ttl: None,
            entries: Vec::new(),
        }
    }

    fn sample_entry(id: &str, title: &str) -> Entry {
        Entry {
            id: id.into(),
            title: Some(Text {
                content_type: "text/plain".parse().unwrap(),
                src: None,
                content: title.into(),
            }),
            updated: None,
            authors: Vec::new(),
            content: None,
            links: Vec::new(),
            summary: None,
            categories: Vec::new(),
            contributors: Vec::new(),
            published: None,
            source: None,
            rights: None,
            media: Vec::new(),
            language: None,
            base: None,
        }
    }

    #[test]
    fn a_new_version_mails_with_a_diff_against_the_stored_content() {
        let decision = decide_item(
            true,
            hash_of("v2"),
            Some(&stored("old text", Some(hash_of("v1")))),
            "new text",
            options(false),
        );

        assert!(decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_some());
    }

    #[test]
    fn a_new_version_mails_even_with_nothing_stored_to_diff_against() {
        let decision = decide_item(true, hash_of("v1"), None, "new text", options(false));

        assert!(decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_none());
    }

    #[test]
    fn an_unchanged_known_version_is_skipped() {
        let v1 = hash_of("v1");
        let decision = decide_item(
            false,
            v1,
            Some(&stored("same text", Some(v1))),
            "same text",
            options(false),
        );

        assert!(!decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_none());
    }

    /// A title-only edit under body-based `update-keys`: the content moved but the version did
    /// not. Skipping it also leaves the stored content alone, so the rename accumulates into the
    /// diff of the next real update.
    #[test]
    fn a_change_the_update_keys_ignore_is_skipped() {
        let v1 = hash_of("v1");
        let decision = decide_item(
            false,
            v1,
            Some(&stored("Old title\nbody", Some(v1))),
            "New title\nbody",
            options(false),
        );

        assert!(!decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_none());
    }

    #[test]
    fn going_back_to_an_already_known_version_is_a_revert() {
        let decision = decide_item(
            false,
            hash_of("v1"),
            Some(&stored("v2 text", Some(hash_of("v2")))),
            "v1 text",
            options(false),
        );

        assert!(decision.notify);
        assert!(decision.reverted);
        assert!(decision.diff.is_some());
    }

    /// Rows stored before the version was recorded alongside them cannot prove a revert, so they
    /// stay silent until the next notification overwrites them.
    #[test]
    fn a_stored_row_of_unknown_version_is_skipped() {
        let decision = decide_item(
            false,
            hash_of("v1"),
            Some(&stored("v2 text", None)),
            "v1 text",
            options(false),
        );

        assert!(!decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_none());
    }

    /// Without the "only when a diff renders" gate, this would mail a `[Reverted]` whose body is
    /// the full current content with no diff and no explanation.
    #[test]
    fn a_revert_with_nothing_to_show_is_skipped() {
        let decision = decide_item(
            false,
            hash_of("v1"),
            Some(&stored("<p>same</p>", Some(hash_of("v2")))),
            "<div>same</div>",
            options(true),
        );

        assert!(!decision.notify);
        assert!(!decision.reverted);
        assert!(decision.diff.is_none());
    }

    #[test]
    fn one_item_repeated_across_urls_is_not_a_diff_key_collision() {
        let feed = sample_feed();
        let entry = sample_entry("item-1", "Title");
        let item = FeedItemContext {
            feed: &feed,
            item: &entry,
        };
        let mut seen = HashMap::new();

        check_diff_key_collision(&mut seen, hash_of("key"), hash_of("v1"), item).unwrap();
        check_diff_key_collision(&mut seen, hash_of("key"), hash_of("v1"), item).unwrap();
    }

    #[test]
    fn two_items_sharing_a_diff_key_fail_the_group() {
        let feed = sample_feed();
        let first = sample_entry("item-1", "First");
        let second = sample_entry("item-2", "Second");
        let mut seen = HashMap::new();

        check_diff_key_collision(
            &mut seen,
            hash_of("key"),
            hash_of("v1"),
            FeedItemContext {
                feed: &feed,
                item: &first,
            },
        )
        .unwrap();
        let error = check_diff_key_collision(
            &mut seen,
            hash_of("key"),
            hash_of("v2"),
            FeedItemContext {
                feed: &feed,
                item: &second,
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("item-1"), "{error}");
        assert!(error.contains("item-2"), "{error}");
        assert!(error.contains("First"), "{error}");
        assert!(error.contains("Second"), "{error}");
        assert!(error.contains("diff-keys"), "{error}");
    }

    fn render_default_item_body(feed: &Feed, item: ItemWithDiff) -> String {
        let mut env = Environment::new();
        add_to_environment(&mut env);
        env.add_global(
            "template_args",
            Value::from_serialize(BTreeMap::<&str, &str>::new()),
        );
        env.add_template("item-body.html", include_str!("templates/item-body.html"))
            .expect("failed to add the default item body template");
        env.get_template("item-body.html")
            .expect("failed to load the default item body template")
            .render(ItemRenderContext { feed, item })
            .expect("failed to render the default item body template")
    }

    /// The span runs backwards for a revert, which is the whole signal.
    #[test]
    fn the_default_item_body_marks_a_revert_and_spans_the_two_versions() {
        let feed = sample_feed();
        let entry = sample_entry("item-1", "Title");

        let rendered = render_default_item_body(
            &feed,
            ItemWithDiff {
                entry: &entry,
                diff: Some("<div>the diff</div>".into()),
                diff_from: Some(time(3, 4)),
                diff_to: Some(time(1, 2)),
                reverted: true,
            },
        );

        assert!(rendered.contains("⏪ Reverted"), "{rendered}");
        assert!(
            rendered.contains("(2026-03-04 05:06 → 2026-01-02 05:06)"),
            "{rendered}"
        );
        assert!(rendered.contains("<div>the diff</div>"), "{rendered}");
    }

    #[test]
    fn the_default_item_body_omits_the_span_when_the_stored_version_is_unknown() {
        let feed = sample_feed();
        let entry = sample_entry("item-1", "Title");

        let rendered = render_default_item_body(
            &feed,
            ItemWithDiff {
                entry: &entry,
                diff: Some("<div>the diff</div>".into()),
                diff_from: None,
                diff_to: Some(time(1, 2)),
                reverted: false,
            },
        );

        assert!(
            rendered.contains("Changes since the last email"),
            "{rendered}"
        );
        assert!(!rendered.contains('→'), "{rendered}");
        assert!(!rendered.contains('⏪'), "{rendered}");
    }

    /// `#[serde(flatten)]` has to keep the `Entry` fields reachable alongside the added ones.
    #[test]
    fn the_item_context_exposes_the_entry_fields_next_to_the_diff_fields() {
        let entry = sample_entry("item-1", "Title");
        let value = Value::from_serialize(ItemWithDiff {
            entry: &entry,
            diff: Some("<div>the diff</div>".into()),
            diff_from: Some(time(3, 4)),
            diff_to: Some(time(1, 2)),
            reverted: true,
        });

        let attr = |name: &str| value.get_attr(name).expect("missing attribute");
        assert_eq!(attr("id").as_str(), Some("item-1"));
        assert_eq!(attr("diff").as_str(), Some("<div>the diff</div>"));
        assert_eq!(attr("diff_from").as_str(), Some("2026-03-04T05:06:00Z"));
        assert_eq!(attr("diff_to").as_str(), Some("2026-01-02T05:06:00Z"));
        assert!(attr("reverted").is_true());
    }
}
