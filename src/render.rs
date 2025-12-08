use crate::config::{FeedGroup, Filter, TemplateSource};
use crate::feed::FeedItemContext;
use blake3::{Hash, Hasher};
use color_eyre::{Result, eyre::Context};
use minijinja::{Environment, Expression, Value};
use minijinja_contrib::add_to_environment;
use ouroboros::self_referencing;
use regex::Regex;
use serde::Serialize;
use std::sync::Arc;
use strum::{AsRefStr, Display};

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

#[derive(AsRefStr, Display)]
#[strum(serialize_all = "kebab-case")]
pub enum TemplateName {
    ItemSubject,
    DigestSubject,
    ItemBody,
    DigestBody,
}

impl<'a> Renderer<'a> {
    pub fn from_feed(feed: &'a FeedGroup) -> Result<Self> {
        let mut env = Environment::new();

        add_to_environment(&mut env);

        fn regex_matches(value: &str, pattern: &str) -> bool {
            Regex::new(pattern).is_ok_and(|re| re.is_match(value))
        }
        env.add_test("matches", regex_matches);

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
            "item-subject" => templates.item_subject.load(),
            "digest-subject" => templates.digest_subject.load(),
            "item-body" => templates.item_body.load(),
            "digest-body" => templates.digest_body.load(),
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
            match value.as_bytes() {
                Some(bytes) => hasher.update(bytes),
                None => hasher.update(value.to_string().as_bytes()),
            };
        }
        Ok(hasher.finalize())
    }

    pub fn filter(&self, ctx: &FeedItemContext) -> Result<bool> {
        self.borrow_filter()
            .as_ref()
            .map_or(Ok(true), |f| f.evaluate(ctx))
    }
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
                Ok(re.is_match(ctx.item.title.as_ref().map_or("", |t| &t.content)))
            }
            Self::BodyRegex(re) => Ok(ctx
                .item
                .summary
                .as_ref()
                .is_some_and(|t| re.is_match(&t.content))
                || ctx
                    .item
                    .content
                    .as_ref()
                    .is_some_and(|c| c.body.as_ref().is_some_and(|b| re.is_match(b)))),
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
                max_mail_per_check: 5,
                sanitize: true,
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
            hasher.update(b"item-42");
            hasher.update(b"feed-id");
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
}
