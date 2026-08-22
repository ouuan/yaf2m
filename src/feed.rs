use crate::config::Settings;
use crate::escape::escape_text;
use ammonia::{Url, UrlRelative};
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

/// What the templates see: the same `{ feed, item }` shape, with `item.diff` grafted on.
#[derive(Debug, Serialize)]
pub struct ItemRenderContext<'a> {
    pub feed: &'a Feed,
    pub item: ItemWithDiff<'a>,
}

/// `Entry` has no rename/skip attributes, so flattening keeps every `item.*` field intact.
#[derive(Debug, Serialize)]
pub struct ItemWithDiff<'a> {
    #[serde(flatten)]
    pub entry: &'a Entry,
    pub diff: Option<String>,
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
        sanitize_feed(&mut feed);
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

/// Clean every feed-controlled `Text` in place.
///
/// Anything reachable from a template has to be covered here: fields rendered with `| safe` need
/// to arrive already safe, and fields rendered without it still want their HTML cleaned rather
/// than shown as escaped source.
fn sanitize_feed(feed: &mut Feed) {
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

        for media in &mut entry.media {
            sanitizer.sanitize_text(&mut media.title, base, false);
            // The default `item-body.html` renders this one with `| safe`.
            sanitizer.sanitize_text(&mut media.description, base, true);
            for text in &mut media.texts {
                sanitizer.sanitize_one(&mut text.text, base, true);
            }
        }
    }
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
            self.sanitize_one(text, base, sanitize_plain_text);
        }
    }

    fn sanitize_one(&mut self, text: &mut Text, base: &str, sanitize_plain_text: bool) {
        if text.content_type.subty() == "html" {
            if let Some(src) = &text.src {
                self.register_base(src);
            } else {
                self.register_base(base);
            }
            text.content = self.0.clean(&text.content).to_string();
        } else if sanitize_plain_text {
            text.content = escape_text(&text.content);
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
                *body = escape_text(body);
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

#[cfg(test)]
mod tests {
    use super::*;
    use feed_rs::model::{FeedType, MediaObject, MediaText};

    fn text(content_type: &str, content: &str) -> Text {
        Text {
            content_type: content_type.parse().unwrap(),
            src: None,
            content: content.to_string(),
        }
    }

    fn feed_with(entry: Entry) -> Feed {
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
            entries: vec![entry],
        }
    }

    fn entry_with(f: impl FnOnce(&mut Entry)) -> Entry {
        let mut entry = Entry {
            id: "item-1".into(),
            title: None,
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
        };
        f(&mut entry);
        entry
    }

    /// `media.description` is rendered with `| safe` by the default `item-body.html`.
    #[test]
    fn sanitizes_media_text() {
        let mut feed = feed_with(entry_with(|entry| {
            entry.media.push(MediaObject {
                title: Some(text("text/html", "<b>Clip</b><script>alert(1)</script>")),
                description: Some(text(
                    "text/html",
                    r#"<p onclick="evil()">Watch</p><script>alert(1)</script>"#,
                )),
                texts: vec![MediaText {
                    text: text("text/html", "<i>Captions</i><script>alert(1)</script>"),
                    start_time: None,
                    end_time: None,
                }],
                ..Default::default()
            });
        }));

        sanitize_feed(&mut feed);
        let media = &feed.entries[0].media[0];

        let description = &media.description.as_ref().unwrap().content;
        assert!(!description.contains("script"), "{description}");
        assert!(!description.contains("onclick"), "{description}");
        assert!(description.contains("Watch"), "{description}");

        let title = &media.title.as_ref().unwrap().content;
        assert!(!title.contains("script"), "{title}");
        assert!(title.contains("Clip"), "{title}");

        let captions = &media.texts[0].text.content;
        assert!(!captions.contains("script"), "{captions}");
        assert!(captions.contains("Captions"), "{captions}");
    }

    /// A plain-text summary is rendered with `| safe`, so it has to arrive pre-escaped.
    #[test]
    fn escapes_plain_text_summary_and_body() {
        let mut feed = feed_with(entry_with(|entry| {
            entry.summary = Some(text("text/plain", "<script>alert(1)</script> Tom & Jerry"));
            entry.content = Some(Content {
                body: Some("2 < 3 && 4 > 1".into()),
                content_type: "text/plain".parse().unwrap(),
                length: None,
                src: None,
            });
        }));

        sanitize_feed(&mut feed);
        let entry = &feed.entries[0];

        assert_eq!(
            entry.summary.as_ref().unwrap().content,
            "&lt;script&gt;alert(1)&lt;/script&gt; Tom &amp; Jerry",
        );
        assert_eq!(
            entry.content.as_ref().unwrap().body.as_deref(),
            Some("2 &lt; 3 &amp;&amp; 4 &gt; 1"),
        );
    }

    /// Titles render without `| safe`, so MiniJinja escapes them; pre-escaping would double up.
    #[test]
    fn leaves_plain_text_titles_alone() {
        let mut feed = feed_with(entry_with(|entry| {
            entry.title = Some(text("text/plain", "Tom & Jerry"));
        }));

        sanitize_feed(&mut feed);

        assert_eq!(
            feed.entries[0].title.as_ref().unwrap().content,
            "Tom & Jerry",
        );
    }
}
