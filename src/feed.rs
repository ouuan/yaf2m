use crate::config::Settings;
use color_eyre::{Result, eyre::WrapErr};
use feed_rs::model::{Entry, Feed};
use ouroboros::self_referencing;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FeedItemContext<'a> {
    pub feed: &'a Feed,
    pub item: &'a Entry,
}

#[self_referencing]
#[derive(Debug)]
pub struct FetchedFeed {
    pub feed: Feed,
    #[borrows(feed)]
    #[covariant]
    pub items: Vec<FeedItemContext<'this>>,
}

pub async fn fetch_feed(url: &str, settings: &Settings) -> Result<FetchedFeed> {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    let client = ClientBuilder::new(reqwest::Client::new())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    let response = client
        .get(url)
        .timeout(settings.timeout)
        .send()
        .await
        .wrap_err_with(|| format!("Failed to fetch feed from {}", url))?;

    let content = response
        .bytes()
        .await
        .wrap_err("Failed to read response body")?;

    let feed = feed_rs::parser::Builder::new()
        .sanitize_content(settings.sanitize)
        .build()
        .parse(&content[..])
        .wrap_err("Failed to parse feed")?;

    Ok(FetchedFeedBuilder {
        feed,
        items_builder: |feed: &Feed| {
            feed.entries
                .iter()
                .map(|item| FeedItemContext { feed, item })
                .collect()
        },
    }
    .build())
}
