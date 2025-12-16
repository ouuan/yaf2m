use crate::config::Settings;
use ammonia::{Url, UrlRelative, clean_text};
use color_eyre::{Result, eyre::WrapErr};
use feed_rs::model::{Content, Entry, Feed, Text};
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
    let retry = RetryTransientMiddleware::new_with_policy(retry_policy)
        .with_retry_log_level(tracing::Level::INFO);
    let client = ClientBuilder::new(reqwest::Client::new())
        .with(retry)
        .build();

    let response = client
        .get(url)
        .timeout(settings.timeout)
        .headers(settings.http_headers.as_ref().clone())
        .send()
        .await
        .wrap_err("Failed to fetch feed")?;

    let content = response
        .bytes()
        .await
        .wrap_err("Failed to read response body")?;

    let mut feed = feed_rs::parser::Builder::new()
        .build()
        .parse(&content[..])
        .wrap_err("Failed to parse feed")?;

    if settings.sanitize {
        let mut sanitizer = Sanitizer::new();

        let base = feed.links.first().map_or(&feed.id, |link| &link.href);
        sanitizer.sanitize_text(&mut feed.title, base, false);
        sanitizer.sanitize_text(&mut feed.description, base, true);
        sanitizer.sanitize_text(&mut feed.rights, base, false);

        for entry in &mut feed.entries {
            let base = entry.links.first().map_or(&entry.id, |link| &link.href);
            sanitizer.sanitize_text(&mut entry.title, base, false);
            sanitizer.sanitize_content(&mut entry.content, base);
            sanitizer.sanitize_text(&mut entry.summary, base, true);
            sanitizer.sanitize_text(&mut entry.rights, base, false);
        }
    }

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

#[derive(Default)]
struct Sanitizer(ammonia::Builder<'static>);

impl Sanitizer {
    fn new() -> Self {
        let mut sanitizer = ammonia::Builder::new();
        sanitizer.add_generic_attributes(["style"]);
        Self(sanitizer)
    }

    fn sanitize_text(&mut self, text: &mut Option<Text>, base: &str, sanitize_plain_text: bool) {
        if let Some(text) = text {
            if text.content_type.subty() == "html" {
                if let Some(src) = &text.src {
                    self.register_base(src);
                } else {
                    self.register_base(base);
                }
                text.content = self.0.clean(&text.content).to_string();
            } else if sanitize_plain_text {
                text.content = clean_text(&text.content);
            }
        }
    }

    fn sanitize_content(&mut self, content: &mut Option<Content>, base: &str) {
        if let Some(content) = content
            && let Some(body) = &mut content.body
        {
            if content.content_type.subty() == "html" {
                if let Some(src) = &content.src {
                    self.register_base(&src.href);
                } else {
                    self.register_base(base);
                }
                *body = self.0.clean(body).to_string();
            } else {
                *body = clean_text(body);
            }
        }
    }

    fn register_base(&mut self, url: &str) -> &mut Self {
        let policy = if let Ok(url) = Url::parse(url) {
            UrlRelative::RewriteWithBase(url)
        } else {
            UrlRelative::PassThrough
        };
        self.0.url_relative(policy);
        self
    }
}
