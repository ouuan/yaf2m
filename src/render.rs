use crate::config::{FeedGroup, Filter, TemplateSource};
use crate::feed::FeedItemContext;
use blake3::{Hash, Hasher};
use color_eyre::{Result, eyre::WrapErr};
use minijinja::{Environment, Expression, Value};
use minijinja_contrib::add_to_environment;
use ouroboros::self_referencing;
use regex::Regex;
use serde::Serialize;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

#[self_referencing]
pub struct Renderer<'a> {
    env: Environment<'a>,
    #[borrows(env)]
    #[covariant]
    update_key_exprs: Vec<Expression<'this, 'a>>,
    #[borrows(env)]
    #[covariant]
    filter: Option<CompiledFilter<'this>>,
}

pub enum TemplateName {
    ItemSubject,
    DigestSubject,
    ItemBody,
    DigestBody,
}

impl AsRef<str> for TemplateName {
    fn as_ref(&self) -> &str {
        match self {
            Self::ItemSubject => "item-subject.txt",
            Self::DigestSubject => "digest-subject.txt",
            Self::ItemBody => "item-body.html",
            Self::DigestBody => "digest-body.html",
        }
    }
}

impl Display for TemplateName {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

impl<'a> Renderer<'a> {
    pub fn from_feed(feed: &'a FeedGroup) -> Result<Self> {
        let mut env = Environment::new();

        add_to_environment(&mut env);

        env.add_test("match", regex_is_match);
        env.add_test("matches", regex_is_match);
        env.add_filter("capture", regex_capture);
        env.add_filter("regex_replace", regex_replace);

        env.add_global(
            "template_args",
            Value::from_serialize(&feed.settings.template_args),
        );

        let templates = Templates {
            item_subject: Arc::clone(&feed.settings.item_subject),
            digest_subject: Arc::clone(&feed.settings.digest_subject),
            item_body: Arc::clone(&feed.settings.item_body),
            digest_body: Arc::clone(&feed.settings.digest_body),
        };

        env.set_loader(move |name| match name {
            "item-subject.txt" => templates.item_subject.load(),
            "digest-subject.txt" => templates.digest_subject.load(),
            "item-body.html" => templates.item_body.load(),
            "digest-body.html" => templates.digest_body.load(),
            _ => Ok(None),
        });

        Renderer::try_new(
            env,
            |env| {
                feed.settings
                    .update_keys
                    .iter()
                    .map(|key| {
                        env.compile_expression(key)
                            .wrap_err("Failed to compile update key expression")
                    })
                    .collect()
            },
            |env| {
                feed.filter
                    .as_ref()
                    .map(|f| CompiledFilter::compile(f, env))
                    .transpose()
            },
        )
    }

    pub fn render<S: Serialize>(&self, name: TemplateName, ctx: S) -> Result<String> {
        self.borrow_env()
            .get_template(name.as_ref())
            .wrap_err_with(|| format!("Failed to get {name} template"))?
            .render(ctx)
            .wrap_err_with(|| format!("Failed to render {name} template"))
    }

    pub fn update_hash(&self, ctx: &FeedItemContext) -> Result<Hash> {
        let mut hasher = Hasher::new();
        for key in self.borrow_update_key_exprs() {
            let value = key
                .eval(ctx)
                .wrap_err("Failed to evaluate update key expression")?;
            let hash = match value.as_bytes() {
                Some(bytes) => blake3::hash(bytes),
                None => blake3::hash(value.to_string().as_bytes()),
            };
            hasher.update(hash.as_bytes());
        }
        Ok(hasher.finalize())
    }

    pub fn filter(&self, ctx: &FeedItemContext) -> Result<bool> {
        self.borrow_filter()
            .as_ref()
            .map_or(Ok(true), |f| f.evaluate(ctx))
    }
}

fn minijinja_regex(pattern: &str) -> Result<Regex, minijinja::Error> {
    Regex::new(pattern).map_err(|e| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "invalid regular expression",
        )
        .with_source(e)
    })
}

fn regex_is_match(value: &str, pattern: &str) -> Result<bool, minijinja::Error> {
    minijinja_regex(pattern).map(|re| re.is_match(value))
}

fn regex_capture(value: &str, pattern: &str, i: Option<usize>) -> Result<Value, minijinja::Error> {
    minijinja_regex(pattern).map(|re| {
        re.captures(value)
            .and_then(|caps| {
                let index = i.unwrap_or(0);
                caps.get(index).map(|m| m.as_str())
            })
            .into()
    })
}

fn regex_replace(value: &str, pattern: &str, replacement: &str) -> Result<Value, minijinja::Error> {
    minijinja_regex(pattern).map(|re| re.replace_all(value, replacement).into())
}

struct Templates {
    item_subject: Arc<TemplateSource>,
    digest_subject: Arc<TemplateSource>,
    item_body: Arc<TemplateSource>,
    digest_body: Arc<TemplateSource>,
}

enum CompiledFilter<'a> {
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    TitleRegex(Regex),
    BodyRegex(Regex),
    Regex(Regex),
    JinjaExpr(Expression<'a, 'a>),
}

impl<'a> CompiledFilter<'a> {
    fn compile(filter: &'a Filter, env: &'a Environment) -> Result<Self> {
        match filter {
            Filter::And(clauses) => Ok(Self::And(
                clauses
                    .iter()
                    .map(|clause| Self::compile(clause, env))
                    .collect::<Result<_>>()?,
            )),
            Filter::Or(clauses) => Ok(Self::Or(
                clauses
                    .iter()
                    .map(|clause| Self::compile(clause, env))
                    .collect::<Result<_>>()?,
            )),
            Filter::Not(clause) => Ok(Self::Not(Box::new(Self::compile(clause, env)?))),
            Filter::TitleRegex(pattern) => {
                let re = Regex::new(pattern).wrap_err("Failed to complile filter title regex")?;
                Ok(Self::TitleRegex(re))
            }
            Filter::BodyRegex(pattern) => {
                let re = Regex::new(pattern).wrap_err("Failed to complile filter body regex")?;
                Ok(Self::BodyRegex(re))
            }
            Filter::Regex(pattern) => {
                let re = Regex::new(pattern).wrap_err("Failed to complile filter regex")?;
                Ok(Self::Regex(re))
            }
            Filter::JinjaExpr(expr_str) => {
                let expr = env
                    .compile_expression(expr_str)
                    .wrap_err("Failed to compile filter Jinja expression")?;
                Ok(Self::JinjaExpr(expr))
            }
        }
    }

    fn evaluate(&self, ctx: &FeedItemContext) -> Result<bool> {
        match self {
            Self::And(clauses) => {
                for clause in clauses {
                    if !clause.evaluate(ctx)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Or(clauses) => {
                for clause in clauses {
                    if clause.evaluate(ctx)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::Not(clause) => Ok(!clause.evaluate(ctx)?),
            Self::TitleRegex(re) => {
                Ok((ctx.item.title.as_ref()).is_some_and(|t| re.is_match(&t.content)))
            }
            Self::BodyRegex(re) => Ok((ctx.item.summary.as_ref().map(|t| &t.content).into_iter())
                .chain(ctx.item.content.as_ref().and_then(|c| c.body.as_ref()))
                .any(|text| re.is_match(text))),
            Self::Regex(re) => Ok((ctx.item.title.as_ref().map(|t| &t.content).into_iter())
                .chain(ctx.item.summary.as_ref().map(|t| &t.content))
                .chain(ctx.item.content.as_ref().and_then(|c| c.body.as_ref()))
                .any(|text| re.is_match(text))),
            Self::JinjaExpr(expr) => expr
                .eval(ctx)
                .map(|v| v.is_true())
                .wrap_err("Failed to evaluate filter Jinja expression"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FeedGroup, Settings, TemplateSource};
    use crate::feed::FeedItemContext;
    use blake3::hash;
    use chrono::TimeDelta;
    use color_eyre::Result;
    use feed_rs::model::{Content, Entry, Feed, FeedType, Text};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    fn build_feed_group(
        item_subject: TemplateSource,
        update_keys: Vec<String>,
        filter: Option<Filter>,
    ) -> FeedGroup {
        let urls = vec!["https://example.com/rss".to_string()];
        let mut urls_hasher = Hasher::new();
        for url in &urls {
            urls_hasher.update(hash(url.as_bytes()).as_bytes());
        }

        let mut template_args = BTreeMap::new();
        template_args.insert("greeting", "Hello");

        FeedGroup {
            urls_hash: urls_hasher.finalize(),
            urls,
            filter,
            settings: Settings {
                to: Vec::new().into(),
                cc: Vec::new().into(),
                bcc: Vec::new().into(),
                digest: false,
                item_subject: Arc::new(item_subject),
                digest_subject: Arc::new(TemplateSource::Inline("digest-subject".into())),
                item_body: Arc::new(TemplateSource::Inline("item-body".into())),
                digest_body: Arc::new(TemplateSource::Inline("digest-body".into())),
                template_args: Arc::new(Value::from_serialize(&template_args)),
                update_keys: update_keys.into(),
                interval: TimeDelta::hours(1),
                keep_old: TimeDelta::weeks(1),
                timeout: Duration::from_secs(30),
                max_mails_per_check: 5,
                sanitize: true,
                sort_by_last_modified: false,
                http_headers: Default::default(),
            },
        }
    }

    fn sample_feed_and_item(id: &str, title: &str, summary: Option<&str>) -> (Feed, Entry) {
        let feed = Feed {
            feed_type: FeedType::RSS2,
            id: "feed-id".into(),
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
        };

        let text_plain = |content: &str| Text {
            content_type: "text/plain".parse().unwrap(),
            src: None,
            content: content.to_string(),
        };

        let item = Entry {
            id: id.into(),
            title: Some(text_plain(title)),
            updated: None,
            authors: Vec::new(),
            content: Some(Content {
                body: Some("<p>Body</p>".into()),
                content_type: "text/html".parse().unwrap(),
                length: None,
                src: None,
            }),
            links: Vec::new(),
            summary: summary.map(text_plain),
            categories: Vec::new(),
            contributors: Vec::new(),
            published: None,
            source: None,
            rights: None,
            media: Vec::new(),
            language: None,
            base: None,
        };

        (feed, item)
    }

    #[test]
    fn renders_item_template_with_globals_and_custom_test() -> Result<()> {
        let template = TemplateSource::Inline(
            "Subject: {{ template_args.greeting }} {{ item.id }} {% if 'abc' is matches('a.*') %}ok{% endif %}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-1", "Rust", Some("Summary"));
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;

        assert_eq!(rendered, "Subject: Hello item-1 ok");
        Ok(())
    }

    #[test]
    fn update_hash_uses_compiled_expressions_in_order() -> Result<()> {
        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into(), "feed.id".into()],
            None,
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-42", "Title", Some("Body"));
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let expected = {
            let mut hasher = Hasher::new();
            hasher.update(hash(b"item-42").as_bytes());
            hasher.update(hash(b"feed-id").as_bytes());
            hasher.finalize()
        };

        let actual = renderer.update_hash(&ctx)?;

        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn filter_combines_regex_and_jinja_expression() -> Result<()> {
        let filter = Filter::And(vec![
            Filter::TitleRegex("Rust".into()),
            Filter::BodyRegex("Body".into()),
            Filter::JinjaExpr("item.id == 'matchme'".into()),
        ]);

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, matching_item) = sample_feed_and_item("matchme", "Rustacean", Some("Body text"));
        let matching_ctx = FeedItemContext {
            feed: &feed,
            item: &matching_item,
        };

        let (_, non_matching_item) = sample_feed_and_item("other", "Python", Some("Body text"));
        let non_matching_ctx = FeedItemContext {
            feed: &feed,
            item: &non_matching_item,
        };

        assert!(renderer.filter(&matching_ctx)?);
        assert!(!renderer.filter(&non_matching_ctx)?);
        Ok(())
    }

    #[test]
    fn renders_all_template_types() -> Result<()> {
        let feed_group = build_feed_group(
            TemplateSource::Inline("item-subject".into()),
            vec!["item.id".into()],
            None,
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("test-id", "Test Title", Some("Summary"));
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        // Test all template types to cover loader branches
        assert_eq!(
            renderer.render(TemplateName::ItemSubject, ctx)?,
            "item-subject"
        );
        assert_eq!(
            renderer.render(TemplateName::DigestSubject, ctx)?,
            "digest-subject"
        );
        assert_eq!(renderer.render(TemplateName::ItemBody, ctx)?, "item-body");
        assert_eq!(
            renderer.render(TemplateName::DigestBody, ctx)?,
            "digest-body"
        );
        Ok(())
    }

    #[test]
    fn filter_or_returns_true_on_first_match() -> Result<()> {
        let filter = Filter::Or(vec![
            Filter::TitleRegex("Match".into()),
            Filter::TitleRegex("NeverMatch".into()),
        ]);

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("id", "Match This", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        assert!(renderer.filter(&ctx)?);
        Ok(())
    }

    #[test]
    fn filter_or_returns_false_when_all_fail() -> Result<()> {
        let filter = Filter::Or(vec![
            Filter::TitleRegex("NoMatch1".into()),
            Filter::TitleRegex("NoMatch2".into()),
        ]);

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("id", "Different Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        assert!(!renderer.filter(&ctx)?);
        Ok(())
    }

    #[test]
    fn filter_not_inverts_result() -> Result<()> {
        let filter = Filter::Not(Box::new(Filter::TitleRegex("Skip".into())));

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, matching_item) = sample_feed_and_item("id", "Normal Title", None);
        let matching_ctx = FeedItemContext {
            feed: &feed,
            item: &matching_item,
        };

        let (_, skipped_item) = sample_feed_and_item("id2", "Skip This", None);
        let skipped_ctx = FeedItemContext {
            feed: &feed,
            item: &skipped_item,
        };

        assert!(renderer.filter(&matching_ctx)?);
        assert!(!renderer.filter(&skipped_ctx)?);
        Ok(())
    }

    #[test]
    fn body_regex_matches_content_body_when_no_summary() -> Result<()> {
        let filter = Filter::BodyRegex("Body".into());

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        // Item with content body but no summary
        let (feed, item) = sample_feed_and_item("id", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        // Should match the content body which contains "<p>Body</p>"
        assert!(renderer.filter(&ctx)?);
        Ok(())
    }

    #[test]
    fn body_regex_matches_summary_over_content() -> Result<()> {
        let filter = Filter::BodyRegex("SummaryText".into());

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("id", "Title", Some("SummaryText here"));
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        assert!(renderer.filter(&ctx)?);
        Ok(())
    }

    #[test]
    fn update_hash_handles_non_string_values() -> Result<()> {
        // Using numeric expressions to trigger the None branch (non-bytes conversion)
        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["feed.entries | length".into()],
            None,
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("test", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let hash1 = renderer.update_hash(&ctx)?;
        let hash2 = renderer.update_hash(&ctx)?;
        assert_eq!(hash1, hash2);

        Ok(())
    }

    #[test]
    fn regex_capture_extracts_groups() -> Result<()> {
        let template =
            TemplateSource::Inline("Captured: {{ item.id | capture('item-(\\\\d+)') }}".into());
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-123", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Captured: item-123");
        Ok(())
    }

    #[test]
    fn regex_capture_with_group_index() -> Result<()> {
        let template =
            TemplateSource::Inline("Number: {{ item.id | capture('item-(\\\\d+)', 1) }}".into());
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-456", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Number: 456");
        Ok(())
    }

    #[test]
    fn regex_capture_returns_none_when_no_match() -> Result<()> {
        let template = TemplateSource::Inline("Result: {{ item.id | capture('notfound') }}".into());
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-789", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Result: none");
        Ok(())
    }

    #[test]
    fn regex_replace_substitutes_matches() -> Result<()> {
        let template = TemplateSource::Inline(
            "Replaced: {{ item.id | regex_replace('item-', 'item_') }}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-999", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Replaced: item_999");
        Ok(())
    }

    #[test]
    fn regex_replace_with_capture_groups() -> Result<()> {
        let template = TemplateSource::Inline(
            "Swapped: {{ item.id | regex_replace('(\\\\w+)-(\\\\d+)', '$2-$1') }}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-555", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Swapped: 555-item");
        Ok(())
    }

    #[test]
    fn regex_replace_replaces_all_occurrences() -> Result<()> {
        let template =
            TemplateSource::Inline("Result: {{ item.id | regex_replace('a', 'A') }}".into());
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("banana", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "Result: bAnAnA");
        Ok(())
    }

    #[test]
    fn regex_is_match_test() -> Result<()> {
        let template = TemplateSource::Inline(
            "{% if item.id is match('item-\\\\d+') %}matches{% else %}no match{% endif %}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-123", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "matches");
        Ok(())
    }

    #[test]
    fn regex_is_match_test_no_match() -> Result<()> {
        let template = TemplateSource::Inline(
            "{% if item.id is match('\\\\d+') %}matches{% else %}no match{% endif %}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("notanumber", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let rendered = renderer.render(TemplateName::ItemSubject, ctx)?;
        assert_eq!(rendered, "no match");
        Ok(())
    }

    #[test]
    fn regex_returns_error_on_invalid_pattern() -> Result<()> {
        let template = TemplateSource::Inline(
            "{% if item.id is match('[') %}matches{% else %}no match{% endif %}".into(),
        );
        let feed_group = build_feed_group(template, vec!["item.id".into()], None);
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, item) = sample_feed_and_item("item-1", "Title", None);
        let ctx = FeedItemContext {
            feed: &feed,
            item: &item,
        };

        let result = renderer.render(TemplateName::ItemSubject, ctx);
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn filter_title_regex_matches_title() -> Result<()> {
        let filter = Filter::TitleRegex("Rust".into());

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        let (feed, matching_item) =
            sample_feed_and_item("id", "Rust Programming", Some("Rust in summary"));
        let matching_ctx = FeedItemContext {
            feed: &feed,
            item: &matching_item,
        };

        let (_, non_matching_item) =
            sample_feed_and_item("id2", "Python Guide", Some("Rust in summary"));
        let non_matching_ctx = FeedItemContext {
            feed: &feed,
            item: &non_matching_item,
        };

        assert!(renderer.filter(&matching_ctx)?);
        assert!(!renderer.filter(&non_matching_ctx)?);
        Ok(())
    }

    #[test]
    fn filter_body_regex_matches_summary_and_content() -> Result<()> {
        let filter = Filter::BodyRegex("important".into());

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        // Test matching summary
        let (feed, summary_item) = sample_feed_and_item("id1", "Title", Some("important info"));
        let summary_ctx = FeedItemContext {
            feed: &feed,
            item: &summary_item,
        };

        // Test non-matching title (should not match)
        let (_, title_item) =
            sample_feed_and_item("id2", "important Title", Some("irrelevant summary"));
        let title_ctx = FeedItemContext {
            feed: &feed,
            item: &title_item,
        };

        // Test non-matching item
        let (_, non_matching_item) = sample_feed_and_item("id3", "Title", Some("irrelevant"));
        let non_matching_ctx = FeedItemContext {
            feed: &feed,
            item: &non_matching_item,
        };

        assert!(renderer.filter(&summary_ctx)?);
        assert!(!renderer.filter(&title_ctx)?);
        assert!(!renderer.filter(&non_matching_ctx)?);
        Ok(())
    }

    #[test]
    fn filter_regex_matches_title_summary_and_content() -> Result<()> {
        let filter = Filter::Regex("search".into());

        let feed_group = build_feed_group(
            TemplateSource::Inline("unused".into()),
            vec!["item.id".into()],
            Some(filter),
        );
        let renderer = Renderer::from_feed(&feed_group)?;

        // Test matching title
        let (feed, title_item) = sample_feed_and_item("id1", "search term", Some("summary"));
        let title_ctx = FeedItemContext {
            feed: &feed,
            item: &title_item,
        };

        // Test matching summary
        let (_, summary_item) = sample_feed_and_item("id2", "Title", Some("search in summary"));
        let summary_ctx = FeedItemContext {
            feed: &feed,
            item: &summary_item,
        };

        // Test non-matching item
        let (_, non_matching_item) = sample_feed_and_item("id3", "No match", Some("irrelevant"));
        let non_matching_ctx = FeedItemContext {
            feed: &feed,
            item: &non_matching_item,
        };

        assert!(renderer.filter(&title_ctx)?);
        assert!(renderer.filter(&summary_ctx)?);
        assert!(!renderer.filter(&non_matching_ctx)?);
        Ok(())
    }
}
