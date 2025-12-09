use crate::config::{FeedGroup, load_config};
use crate::db;
use crate::email::{Mail, Mailer, send_email_with_backoff};
use crate::feed::fetch_feed;
use crate::render::{Renderer, TemplateName};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use sqlx::PgPool;
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
        let mut last_modified = SystemTime::UNIX_EPOCH;

        loop {
            let modified = tokio::fs::metadata(&this.config_path)
                .await
                .wrap_err("failed to get config file metadata")?
                .modified()?;
            if modified != last_modified {
                log::info!("Config file modified, reloading");
                let config = load_config(&this.config_path).await?;
                feeds = config.feeds.into_iter().map(Arc::new).collect();
                last_modified = modified;
            }

            let mut set = JoinSet::new();

            for feed in feeds.iter().map(Arc::clone) {
                let worker = Arc::clone(&this);
                set.spawn(async move {
                    if let Err(e) = worker.process_feed(&feed).await {
                        log::error!("Error processing feed: {e:?}");
                        if let Err(e) =
                            db::increment_feed_group_fail_count(&worker.pool, feed.urls_hash).await
                        {
                            log::warn!("Failed to increment fail count: {e:?}");
                        }
                    }
                });
            }

            while let Some(res) = set.join_next().await {
                if let Err(e) = res {
                    log::error!("Task panicked: {e:?}");
                }
            }

            log::debug!("Worker cycle completed, sleeping for 1 minute");

            tokio::time::sleep(Duration::from_mins(1)).await;
        }
    }

    async fn process_feed(&self, feed_group: &FeedGroup) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        let fail_count = db::get_feed_group_fail_count(&mut *tx, feed_group.urls_hash).await?;

        let status = db::try_check_feed_group(&mut *tx, fail_count, feed_group).await?;

        if status == "wait" {
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
            all_feeds.push(feed);
        }

        let mut new_items = Vec::new();

        // reverse back
        for item in all_feeds.iter().rev().flat_map(|feed| feed.borrow_items()) {
            if !renderer.filter(item)? {
                continue;
            }

            let update_hash = renderer.update_hash(item)?;

            let new = db::upsert_and_check_item_new(
                &mut *tx,
                feed_group.urls_hash,
                &item.item.id,
                update_hash,
            )
            .await?;

            if new {
                new_items.push(item);
            }
        }

        log::info!(
            "Feed group {:?}: {} new items found",
            feed_group.urls,
            new_items.len()
        );

        // Send emails
        if !new_items.is_empty() {
            let mails = if status == "new"
                || feed_group.settings.digest
                || new_items.len() > feed_group.settings.max_mail_per_check
            {
                let feeds = all_feeds
                    .iter()
                    .map(|feed| feed.borrow_feed())
                    .collect::<Vec<_>>();
                let ctx = minijinja::context! { feeds => feeds, items => new_items };
                let subject = renderer.render(TemplateName::DigestSubject, &ctx)?;
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

            send_email_with_backoff(&self.mailer, feed_group, mails).await?;

            log::info!("Feed group {:?}: Sent {mail_count} emails", feed_group.urls);

            db::set_feed_group_update_time(&mut *tx, feed_group.urls_hash).await?;
        }

        db::reset_feed_group_fail_count(&mut *tx, feed_group.urls_hash).await?;

        db::delete_old_items(&mut *tx, feed_group.urls_hash, feed_group.settings.keep_old).await?;

        tx.commit().await?;

        Ok(())
    }
}
