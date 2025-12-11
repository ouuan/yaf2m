use crate::config::{FeedGroup, load_config};
use crate::db::{self, FeedStatus};
use crate::email::{Mail, Mailer, send_email_with_backoff};
use crate::feed::fetch_feed;
use crate::render::{Renderer, TemplateName};
use blake3::{Hash, Hasher};
use chrono::{TimeDelta, Utc};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
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
        let mut feed_hashes = Vec::new();
        let mut keep_old = TimeDelta::default();
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
                feed_hashes = feeds
                    .iter()
                    .map(|feed| feed.urls_hash.as_bytes().to_vec())
                    .collect();
                keep_old = config.global_settings.keep_old;
                failure_tracker.set_report_to(config.error_report_to);
                last_modified = modified;
            }

            let mut set = JoinSet::new();

            for feed in feeds.iter().map(Arc::clone) {
                let worker = Arc::clone(&this);
                set.spawn(async move {
                    if let Err(e) = worker.process_feed(&feed).await {
                        log::error!("Error processing feed group {:?}: {e:?}", feed.urls);
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

            match db::get_failing_feeds(&this.pool).await {
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

        if status == FeedStatus::Wait {
            log::debug!("Feed group {:?} waiting", feed_group.urls);
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

        for item in all_feeds.iter().flat_map(|feed| feed.borrow_items()) {
            if !renderer.filter(item)? {
                log::trace!(
                    "Item filtered out:\n{}",
                    render!("{{ item }}", item => item.item)
                );
                continue;
            }

            let update_hash = renderer.update_hash(item)?;

            let new =
                db::upsert_and_check_item_new(&mut *tx, feed_group.urls_hash, update_hash).await?;

            log::trace!(
                "hash: {}, new: {}, item:\n{}",
                update_hash,
                new,
                render!("{{ item }}", item => item.item)
            );

            if new {
                new_items.push(item);
            }
        }

        if feed_group.settings.sort_by_last_modified {
            new_items.sort_by_key(|item| Reverse(item.item.updated.or(item.item.published)));
        }

        log::info!(
            "Feed group {:?}: {} new items found",
            feed_group.urls,
            new_items.len()
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
                let ctx = minijinja::context! { feeds => feeds, items => new_items };
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
                    .into_iter()
                    .map(|item| {
                        let subject = renderer.render(TemplateName::ItemSubject, item)?;
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

        tx.commit().await?;

        Ok(())
    }
}

struct FailureTracker {
    failing_hash: Hash,
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
        Self {
            failing_hash: Hasher::new().finalize(),
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
        if failing_hash == self.failing_hash {
            match self.debounce_count {
                0 => {}
                1 => match self.send_failure_report(failures, mailer).await {
                    Err(e) => log::error!("Failed to send failure report email: {e:?}"),
                    Ok(_) => self.debounce_count = 0,
                },
                _ => self.debounce_count -= 1,
            }
        } else {
            log::info!("Failing feed groups changed ({} failures)", failures.len(),);
            self.failing_hash = failing_hash;
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
