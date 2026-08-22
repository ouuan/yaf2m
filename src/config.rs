use crate::diff::{DiffContext, DiffContextKeyword, DiffGranularity, DiffOptions};
use blake3::{Hash, Hasher, hash};
use chrono::TimeDelta;
use color_eyre::eyre::eyre;
use color_eyre::{Result, eyre::WrapErr};
use lettre::message::Mailbox;
use minijinja::Value;
use minijinja::value::merge_maps;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_with::{OneOrMany, serde_as, serde_conv};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_DIGEST: bool = false;
const DEFAULT_ITEM_SUBJECT: &str = include_str!("templates/item-subject.txt");
const DEFAULT_DIGEST_SUBJECT: &str = include_str!("templates/digest-subject.txt");
const DEFAULT_ITEM_BODY: &str = include_str!("templates/item-body.html");
const DEFAULT_DIGEST_BODY: &str = include_str!("templates/digest-body.html");
const DEFAULT_UPDATE_KEY: &str = "item.id";
// The title leads the diffed text so that a retitled item shows the rename rather than an
// unexplained mail. A titleless item leaves `item.title.content` undefined, which `~` concatenates
// as an empty string, so it needs no guard of its own.
pub const DEFAULT_DIFF_CONTENT: &str =
    r"item.title.content ~ '\n' ~ (item.content.body or item.summary.content or '')";
const DEFAULT_DIFF_GRANULARITY: DiffGranularity = DiffGranularity::Word;
const DEFAULT_DIFF_CONTEXT: DiffContext = DiffContext::Keyword(DiffContextKeyword::Auto);
const DEFAULT_DIFF_STRIP_TAGS: bool = false;
const DEFAULT_INTERVAL: TimeDelta = TimeDelta::hours(1);
const DEFAULT_KEEP_OLD: TimeDelta = TimeDelta::weeks(1);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_MAILS_PER_CHECK: usize = 5;
const DEFAULT_RETRY_COUNT: usize = 2;
const DEFAULT_RETRY_INTERVAL: TimeDelta = TimeDelta::zero();
const DEFAULT_SANITIZE: bool = true;
const DEFAULT_SORT_BY_LAST_MODIFIED: bool = false;

#[derive(Debug)]
pub struct Config {
    pub error_report_to: Vec<Mailbox>,
    pub global_settings: Settings,
    pub feeds: Vec<FeedGroup>,
}

pub async fn load_config(path: &Path) -> Result<Config> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .wrap_err_with(|| format!("Failed to read config file at {}", path.display()))?;

    let config: ConfigFile = toml::from_str(&raw)
        .wrap_err_with(|| format!("Failed to parse config file at {}", path.display()))?;

    let global_settings = config.settings.with_default();

    let feeds = config
        .feeds
        .into_iter()
        .map(|fc| fc.resolve(&global_settings))
        .collect::<Vec<_>>();

    let mut url_hash_set = HashSet::new();

    for feed in &feeds {
        if !url_hash_set.insert(feed.urls_hash) {
            return Err(eyre!(
                "Duplicate feed URLs detected in config file: {:?}",
                feed.urls
            ));
        }
    }

    Ok(Config {
        error_report_to: config.error_report_to,
        global_settings,
        feeds,
    })
}

#[derive(Debug)]
pub struct Settings {
    pub to: Arc<[Mailbox]>,
    pub cc: Arc<[Mailbox]>,
    pub bcc: Arc<[Mailbox]>,
    pub digest: bool,
    pub item_subject: Arc<TemplateSource>,
    pub digest_subject: Arc<TemplateSource>,
    pub item_body: Arc<TemplateSource>,
    pub digest_body: Arc<TemplateSource>,
    pub template_args: Arc<Value>,
    pub update_keys: Arc<[String]>,
    pub diff_keys: Arc<[String]>,
    pub diff_content: Arc<str>,
    pub diff_options: DiffOptions,
    pub interval: TimeDelta,
    pub keep_old: TimeDelta,
    pub timeout: Duration,
    pub max_mails_per_check: usize,
    pub retry_count: usize,
    pub retry_interval: TimeDelta,
    pub sanitize: bool,
    pub sort_by_last_modified: bool,
    pub http_headers: Arc<HeaderMap>,
}

#[derive(Debug)]
pub struct FeedGroup {
    pub urls_hash: Hash,
    pub criteria_hash: Hash,
    pub urls: Vec<String>,
    pub filter: Option<Filter>,
    pub settings: Settings,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemplateSource {
    Inline(String),
    File(PathBuf),
}

impl TemplateSource {
    pub fn load(&self) -> Result<Option<String>, minijinja::Error> {
        Ok(Some(match self {
            TemplateSource::Inline(s) => s.clone(),
            TemplateSource::File(path) => std::fs::read_to_string(path).map_err(|e| {
                minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("failed to read template at {}", path.display()),
                )
                .with_source(e)
            })?,
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Filter {
    #[serde(alias = "all")]
    And(Vec<Self>),
    #[serde(alias = "any")]
    Or(Vec<Self>),
    Not(Box<Self>),
    TitleRegex(String),
    BodyRegex(String),
    Regex(String),
    JinjaExpr(String),
}

impl Filter {
    fn hash(&self) -> Hash {
        let mut hasher = Hasher::new();
        match self {
            Filter::And(subfilters) => {
                hasher.update(b"And");
                for subfilter in subfilters {
                    hasher.update(subfilter.hash().as_bytes());
                }
            }
            Filter::Or(subfilters) => {
                hasher.update(b"Or");
                for subfilter in subfilters {
                    hasher.update(subfilter.hash().as_bytes());
                }
            }
            Filter::Not(subfilter) => {
                hasher.update(b"Not");
                hasher.update(subfilter.hash().as_bytes());
            }
            Filter::TitleRegex(pattern) => {
                hasher.update(b"TitleRegex");
                hasher.update(hash(pattern.as_bytes()).as_bytes());
            }
            Filter::BodyRegex(pattern) => {
                hasher.update(b"BodyRegex");
                hasher.update(hash(pattern.as_bytes()).as_bytes());
            }
            Filter::Regex(pattern) => {
                hasher.update(b"Regex");
                hasher.update(hash(pattern.as_bytes()).as_bytes());
            }
            Filter::JinjaExpr(expr) => {
                hasher.update(b"JinjaExpr");
                hasher.update(hash(expr.as_bytes()).as_bytes());
            }
        }
        hasher.finalize()
    }
}

#[serde_as]
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct OptionalSettings {
    #[serde_as(as = "Option<OneOrMany<_>>")]
    to: Option<Vec<Mailbox>>,
    #[serde_as(as = "Option<OneOrMany<_>>")]
    cc: Option<Vec<Mailbox>>,
    #[serde_as(as = "Option<OneOrMany<_>>")]
    bcc: Option<Vec<Mailbox>>,
    digest: Option<bool>,
    item_subject: Option<TemplateSource>,
    digest_subject: Option<TemplateSource>,
    item_body: Option<TemplateSource>,
    digest_body: Option<TemplateSource>,
    template_args: Option<HashMap<String, Value>>,
    #[serde_as(as = "Option<OneOrMany<_>>")]
    #[serde(alias = "update-key")]
    update_keys: Option<Vec<String>>,
    #[serde_as(as = "Option<OneOrMany<_>>")]
    #[serde(alias = "diff-key")]
    diff_keys: Option<Vec<String>>,
    // Flat rather than a nested `[settings.diff]` table, so that a feed overriding one diff knob
    // inherits the rest from `[settings]` instead of resetting them to the built-in defaults.
    diff_content: Option<String>,
    diff_granularity: Option<DiffGranularity>,
    diff_context: Option<DiffContext>,
    diff_strip_tags: Option<bool>,
    #[serde_as(as = "Option<HumanTimeDelta>")]
    interval: Option<TimeDelta>,
    #[serde_as(as = "Option<HumanTimeDelta>")]
    keep_old: Option<TimeDelta>,
    #[serde(default, with = "humantime_serde")]
    timeout: Option<Duration>,
    #[serde(alias = "max_mail_per_check")]
    max_mails_per_check: Option<usize>,
    #[serde(alias = "failure-retry-count", alias = "failure-retry-times")]
    retry_count: Option<usize>,
    #[serde_as(as = "Option<HumanTimeDelta>")]
    retry_interval: Option<TimeDelta>,
    sanitize: Option<bool>,
    sort_by_last_modified: Option<bool>,
    #[serde_as(as = "Option<AsHeaderMap>")]
    http_headers: Option<HeaderMap>,
}

impl OptionalSettings {
    fn with_default(self) -> Settings {
        Settings {
            to: self.to.unwrap_or_default().into(),
            cc: self.cc.unwrap_or_default().into(),
            bcc: self.bcc.unwrap_or_default().into(),
            digest: self.digest.unwrap_or(DEFAULT_DIGEST),
            item_subject: self
                .item_subject
                .unwrap_or(TemplateSource::Inline(DEFAULT_ITEM_SUBJECT.into()))
                .into(),
            digest_subject: self
                .digest_subject
                .unwrap_or(TemplateSource::Inline(DEFAULT_DIGEST_SUBJECT.into()))
                .into(),
            item_body: self
                .item_body
                .unwrap_or(TemplateSource::Inline(DEFAULT_ITEM_BODY.into()))
                .into(),
            digest_body: self
                .digest_body
                .unwrap_or(TemplateSource::Inline(DEFAULT_DIGEST_BODY.into()))
                .into(),
            template_args: Arc::new(self.template_args.unwrap_or_default().into()),
            update_keys: self
                .update_keys
                .unwrap_or_else(|| vec![DEFAULT_UPDATE_KEY.to_string()])
                .into(),
            diff_keys: self.diff_keys.unwrap_or_default().into(),
            diff_content: self
                .diff_content
                .unwrap_or_else(|| DEFAULT_DIFF_CONTENT.to_string())
                .into(),
            diff_options: DiffOptions {
                granularity: self.diff_granularity.unwrap_or(DEFAULT_DIFF_GRANULARITY),
                context: self.diff_context.unwrap_or(DEFAULT_DIFF_CONTEXT),
                strip_tags: self.diff_strip_tags.unwrap_or(DEFAULT_DIFF_STRIP_TAGS),
            },
            interval: self.interval.unwrap_or(DEFAULT_INTERVAL),
            keep_old: self.keep_old.unwrap_or(DEFAULT_KEEP_OLD),
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
            max_mails_per_check: self
                .max_mails_per_check
                .unwrap_or(DEFAULT_MAX_MAILS_PER_CHECK),
            retry_count: self.retry_count.unwrap_or(DEFAULT_RETRY_COUNT),
            retry_interval: self.retry_interval.unwrap_or(DEFAULT_RETRY_INTERVAL),
            sanitize: self.sanitize.unwrap_or(DEFAULT_SANITIZE),
            sort_by_last_modified: self
                .sort_by_last_modified
                .unwrap_or(DEFAULT_SORT_BY_LAST_MODIFIED),
            http_headers: self.http_headers.unwrap_or_default().into(),
        }
    }
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    #[serde_as(as = "OneOrMany<_>")]
    error_report_to: Vec<Mailbox>,
    #[serde(default)]
    settings: OptionalSettings,
    #[serde(default)]
    feeds: Vec<FeedConfig>,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct FeedConfig {
    #[serde_as(as = "OneOrMany<_>")]
    #[serde(alias = "url")]
    urls: Vec<String>,
    #[serde(flatten)]
    settings: OptionalSettings,
    #[serde(default)]
    filter: Option<Filter>,
}

impl FeedConfig {
    fn resolve(self, global: &Settings) -> FeedGroup {
        let to = pick(self.settings.to, &global.to);
        let cc = pick(self.settings.cc, &global.cc);
        let bcc = pick(self.settings.bcc, &global.bcc);
        let digest = self.settings.digest.unwrap_or(global.digest);
        let item_subject = pick(self.settings.item_subject, &global.item_subject);
        let digest_subject = pick(self.settings.digest_subject, &global.digest_subject);
        let item_body = pick(self.settings.item_body, &global.item_body);
        let digest_body = pick(self.settings.digest_body, &global.digest_body);
        let template_args = match self.settings.template_args {
            Some(args) => merge_maps([args.into(), Value::clone(&global.template_args)]).into(),
            None => Arc::clone(&global.template_args),
        };
        let update_keys = pick(self.settings.update_keys, &global.update_keys);
        let diff_keys = pick(self.settings.diff_keys, &global.diff_keys);
        let diff_content = pick(self.settings.diff_content, &global.diff_content);
        let diff_options = DiffOptions {
            granularity: self
                .settings
                .diff_granularity
                .unwrap_or(global.diff_options.granularity),
            context: self
                .settings
                .diff_context
                .unwrap_or(global.diff_options.context),
            strip_tags: self
                .settings
                .diff_strip_tags
                .unwrap_or(global.diff_options.strip_tags),
        };
        let interval = self.settings.interval.unwrap_or(global.interval);
        let keep_old = self.settings.keep_old.unwrap_or(global.keep_old);
        let timeout = self.settings.timeout.unwrap_or(global.timeout);
        let max_mails_per_check = self
            .settings
            .max_mails_per_check
            .unwrap_or(global.max_mails_per_check);
        let retry_count = self.settings.retry_count.unwrap_or(global.retry_count);
        let retry_interval = self
            .settings
            .retry_interval
            .unwrap_or(global.retry_interval);
        let sanitize = self.settings.sanitize.unwrap_or(global.sanitize);
        let sort_by_last_modified = self
            .settings
            .sort_by_last_modified
            .unwrap_or(global.sort_by_last_modified);
        let http_headers = pick(self.settings.http_headers, &global.http_headers);

        let urls_hash = {
            let mut hasher = Hasher::new();
            for url in &self.urls {
                hasher.update(hash(url.as_bytes()).as_bytes());
            }
            hasher.finalize()
        };

        let criteria_hash = {
            let mut hasher = Hasher::new();
            hasher.update(urls_hash.as_bytes());
            let update_key_hash = {
                let mut hasher = Hasher::new();
                for key in update_keys.iter() {
                    hasher.update(hash(key.as_bytes()).as_bytes());
                }
                hasher.finalize()
            };
            hasher.update(update_key_hash.as_bytes());
            let filter_hash = self
                .filter
                .as_ref()
                .map_or_else(|| Hash::from_bytes(Default::default()), |f| f.hash());
            hasher.update(filter_hash.as_bytes());
            hasher.finalize()
        };

        FeedGroup {
            urls_hash,
            criteria_hash,
            urls: self.urls,
            filter: self.filter,
            settings: Settings {
                to,
                cc,
                bcc,
                digest,
                item_subject,
                digest_subject,
                item_body,
                digest_body,
                template_args,
                update_keys,
                diff_keys,
                diff_content,
                diff_options,
                interval,
                keep_old,
                timeout,
                max_mails_per_check,
                retry_count,
                retry_interval,
                sanitize,
                sort_by_last_modified,
                http_headers,
            },
        }
    }
}

serde_conv!(
    HumanTimeDelta,
    TimeDelta,
    |_| { "serialization unimplemented" },
    |s: String| -> Result<_> {
        let duration = humantime::parse_duration(&s)?;
        Ok(TimeDelta::from_std(duration)?)
    }
);

serde_conv!(
    AsHeaderMap,
    HeaderMap,
    |_| { "serialization unimplemented" },
    |map: HashMap<String, String>| HeaderMap::try_from(&map)
);

fn pick<T, U>(local: Option<T>, global: &Arc<U>) -> Arc<U>
where
    Arc<U>: From<T>,
    U: ?Sized,
{
    match local {
        Some(owned) => Arc::from(owned),
        None => Arc::clone(global),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_one(toml_src: &str) -> Settings {
        let config: ConfigFile = toml::from_str(toml_src).expect("failed to parse config");
        let global = config.settings.with_default();
        config
            .feeds
            .into_iter()
            .next()
            .expect("expected a feed group")
            .resolve(&global)
            .settings
    }

    #[test]
    fn diff_context_accepts_an_integer_and_the_keywords() {
        let settings = resolve_one(
            r#"
            [[feeds]]
            url = 'https://example.com/feed.xml'
            diff-context = 3
            "#,
        );
        assert!(matches!(
            settings.diff_options.context,
            DiffContext::Count(3)
        ));

        let settings = resolve_one(
            r#"
            [[feeds]]
            url = 'https://example.com/feed.xml'
            diff-context = 'full'
            "#,
        );
        assert!(matches!(
            settings.diff_options.context,
            DiffContext::Keyword(DiffContextKeyword::Full)
        ));

        let settings = resolve_one(
            r#"
            [[feeds]]
            url = 'https://example.com/feed.xml'
            "#,
        );
        assert!(matches!(
            settings.diff_options.context,
            DiffContext::Keyword(DiffContextKeyword::Auto)
        ));
    }

    #[test]
    fn diff_context_rejects_garbage_with_a_usable_message() {
        let error = toml::from_str::<ConfigFile>(
            r#"
            [[feeds]]
            url = 'https://example.com/feed.xml'
            diff-context = 'lots'
            "#,
        )
        .expect_err("expected a parse error");
        assert!(
            error
                .to_string()
                .contains(r#"a non-negative integer, "full", or "auto""#),
            "{error}"
        );
    }

    #[test]
    fn diff_settings_override_one_knob_at_a_time() {
        let settings = resolve_one(
            r#"
            [settings]
            diff-key = 'item.id'
            diff-context = 7
            diff-granularity = 'line'
            diff-strip-tags = true

            [[feeds]]
            url = 'https://example.com/feed.xml'
            diff-content = 'item.title.content'
            "#,
        );

        assert_eq!(&*settings.diff_content, "item.title.content");
        assert_eq!(&*settings.diff_keys, ["item.id".to_string()]);
        assert!(matches!(
            settings.diff_options.context,
            DiffContext::Count(7)
        ));
        assert_eq!(settings.diff_options.granularity, DiffGranularity::Line);
        assert!(settings.diff_options.strip_tags);
    }

    #[test]
    fn diff_keys_is_empty_by_default() {
        let settings = resolve_one(
            r#"
            [[feeds]]
            url = 'https://example.com/feed.xml'
            "#,
        );
        assert!(settings.diff_keys.is_empty());
        assert_eq!(&*settings.diff_content, DEFAULT_DIFF_CONTENT);
    }

    #[test]
    fn diff_settings_are_not_part_of_criteria_hash() {
        let hash_of = |extra: &str| {
            let config: ConfigFile = toml::from_str(&format!(
                "[[feeds]]\nurl = 'https://example.com/feed.xml'\n{extra}\n"
            ))
            .expect("failed to parse config");
            let global = config.settings.with_default();
            config
                .feeds
                .into_iter()
                .next()
                .expect("expected a feed group")
                .resolve(&global)
                .criteria_hash
        };

        assert_eq!(hash_of(""), hash_of("diff-key = 'item.id'"));
        assert_eq!(hash_of(""), hash_of("diff-context = 1"));
        assert_ne!(hash_of(""), hash_of("update-key = 'item.title'"));
    }
}
