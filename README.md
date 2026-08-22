# yaf2m (Yet Another Feed to Mail)

Send email alerts or digests when your RSS/Atom feeds update.

## Features

- **Feed Grouping & Deduplication**: Combine multiple feed URLs into a single group. Items from all feeds in a group are deduplicated and processed together.
- **Digest or Individual Emails**: Choose between sending a single digest email for multiple updates or separate emails for each new item. Automatic digesting if too many updates.
- **Flexible Per-Feed Settings**: Settings (recipients, templates, update keys, etc.) can be set globally or overridden for each feed group.
- **Custom Update Keys**: Detect updates using traditional GUIDs or any custom content via MiniJinja expressions, allowing notification on any change you care about.
- **Content Diffs**: When a page you follow is edited, show what actually changed instead of the whole content again, as an inline redline or a unified diff.
- **Customizable Email Templates**: Use MiniJinja templates for email subject and body.
- **Advanced Filtering**: Filter feed items using logical combinations (`and`/`all`, `or`/`any`, `not`), regular expressions, or MiniJinja expressions for fine-grained control.
- **Notification On Error**: Send notifications when feeds are not working.
- **HTML Sanitization**: Sanitize feed HTML content for safer emails.

## Quick Start

-   Write a config file with your feeds.
-   Set the environment variables described in the next section.
-   Start the service with Docker Compose: `docker compose up -d` (see [`docker-compose.yml`](./docker-compose.yml)).

## Environment Variables

-   `YAF2M_CONFIG_PATH`: path to the config file (default: `config/config.toml`).
-   `POSTGRES_URL`: database connection string; see [sqlx::postgres::PgConnectOptions](https://docs.rs/sqlx/latest/sqlx/postgres/struct.PgConnectOptions.html).
-   `SMTP_FROM`: sender address, e.g. `"yaf2m" <yaf2m@example.com>`.
-   `SMTP_URL`: SMTP transport URL; see [lettre::transport::smtp::SmtpTransport::from_url](https://docs.rs/lettre/latest/lettre/transport/smtp/struct.SmtpTransport.html#method.from_url).

## Config File

Note: The config file is auto-reloaded. There is no need to restart the service.

### Import

You can use [`opml-to-config.py`](./opml-to-config.py) to convert an OPML file to yaf2m config.

### Examples

Minimal:

```toml
[settings]
to = 'you@example.com'

[[feeds]]
url = 'https://example.com/feed.xml'

[[feeds]]
url = 'https://example.org/feed.atom'
```

With default values:

```toml
error-report-to = [] # error-report-to = "admin@example.com"

[settings]
to = []
cc = []
bcc = []
digest = false
max-mails-per-check = 5
item-subject = <src/templates/item-subject.txt>
digest-subject = <src/templates/digest-subject.txt>
item-body = <src/templates/item-body.html>
digest-body = <src/templates/digest-body.html>
template-args = {}
update-key = 'item.id'
diff-keys = []
diff-content = "item.content.body or item.summary.content or ''"
diff-granularity = 'word'
diff-context = 'auto'
diff-strip-tags = false
interval = '1h'
keep-old = '1w'
timeout = '30s'
retry-count = 2
retry-interval = '0s'
sanitize = true
sort-by-last-modified = false
http-headers = {}

[[feeds]]
url = "https://blog.rust-lang.org/feed.xml"
# urls = ["https://example.org/feed.atom", "https://example.net/feed.json"]
# To override [settings]:
# to = ["Alice <alice@example.com>", "bob@example.org"]
# cc = "john@example.com" is the same as cc = ["john@example.com"]
# bcc = []
# digest = true
# max-mails-per-check = 1
# item-subject.inline = "{{ item.title.content }}"
# digest-subject.inline = "My daily feed on {{ now() | dateformat(tz=template_args.tz) }}"
# item-body.file = "/path/to/item-template.html"
# digest-body.file = "/path/to/item-template.html"
# template-args.tz = "Asia/Shanghai"
# update-keys = ['item.title', 'item.content | capture("<main>([\\s\\S]*?)</main>", 1)']
# diff-key = 'item.id' is the same as diff-keys = ['item.id']
# diff-content = 'item.title.content'
# diff-granularity = 'line'
# diff-context = 5
# diff-strip-tags = true
# interval = '1d'
# keep-old = '2w'
# timeout = '1m'
# retry-count = 3
# retry-interval = '10m'
# sanitize = false
# sort-by-last-modified = true
# http-headers.user-agent = "xxx"
feeds.filter.any = [
  { title-regex = '^Announcing' },
  {
    all = [
      { not.body-regex = 'foo' },
      { jinja-expr = "item.authors | selectattr('name', 'equalto', 'John') | list | length > 0" },
    ],
  },
]
```

### Structure

-   Feeds are organized as groups (`[[feeds]]`). One group may contain one or more feed URLs. Feeds in the same group are combined together and items are deduplicated.
-   `urls` and `filter` are group-specific. Other settings may have a global default value in `[settings]`. Settings resolve in order: value on the feed group -> value in `[settings]` -> built-in default.

### Fields

-   `to`, `cc`, `bcc`: Mail recipients. Each can be a single string or an array of strings.
-   `digest`: Whether to send all updates in a single digest mail or to send one mail per item. Newly added feeds and updates triggered by configuration changes (e.g. `update-keys` or `filter`) are always sent in digests.
-   `max-mails-per-check`: Send digest if there are too many updates, even if `digest = false`.
-   `item-subject`, `digest-subject`, `item-body`, `digest-body`: [MiniJinja](https://docs.rs/minijinja) templates for mail contents.
    -   Can be `{ inline = "{{ template }}" }` or `{ file = "/path/to/template" }`.
    -   Default templates: [`src/templates`](./src/templates).
    -   Context for single item: `{ feed => Feed, item => Entry }`, see [`feed_rs::model::Feed`](https://docs.rs/feed-rs/latest/feed_rs/model/struct.Feed.html) and [`feed_rs::model::Entry`](https://docs.rs/feed-rs/latest/feed_rs/model/struct.Entry.html).
    -   Context for digest: `{ feeds => [Feed], items => [{ feed => Feed, item => Entry }] }`, where `feeds` are all feeds in the group (no matter updated or not), and `items` are updated items.
    -   `item.diff` is an extra field alongside the `Entry` fields: a pre-escaped HTML string when `diff-keys` is set and the stored content changed, `none` otherwise. It is already escaped, so render it with `{{ item.diff | safe }}`.
    -   Custom args: `template-args`.
    -   Can include each other, e.g. `{% include "item-body.html" %}`, `{% include "digest-subject.txt" %}`.
    -   More features:
        -   builtin [`filters`](https://docs.rs/minijinja/latest/minijinja/filters/index.html) and [`tests`](https://docs.rs/minijinja/latest/minijinja/tests/index.html)
        -   [`minijinja-contrib`](https://docs.rs/minijinja-contrib/latest/minijinja_contrib/) [`filters`](https://docs.rs/minijinja-contrib/latest/minijinja_contrib/filters/index.html) and [`globals`](https://docs.rs/minijinja-contrib/latest/minijinja_contrib/globals/index.html)
        -   Regular expressions: `str is match(regex)`, `str | capture(regex[, group])`, `str | regex_replace(regex, replacement)`.
-   `template-args`: Custom args that are passed to the MiniJinja templates. Template args set on each feed are merged with the global setting. Args used by the default templates:   
    -   `tz`: timezone
    -   `group_title`: used by the default `digest-subject` template to display the title for the entire feed group (useful when there are multiple URLs in a feed group)
-   `update-keys`/`update-key`: Keys that are used to check whether a feed item is updated or not. Each key is a MiniJinja expression. This can be used to control whether to notify feed content update.
-   `diff-keys`/`diff-key`: Keys that identify the *same item across content updates*, so that a mail can show what changed instead of only the new content. Each key is a MiniJinja expression, evaluated against the same `{ feed, item }` context as `update-keys` and `filter`. Empty by default, which disables the feature entirely.
    -   The canonical recipe is to notify on content change and diff against the previous content:

        ```toml
        [[feeds]]
        url = 'https://example.com/changelog.xml'
        update-keys = ['item.id', 'item.content.body'] # re-notify when the content changes
        diff-key = 'item.id'                           # ...and diff against the previous content
        diff-strip-tags = true
        ```

    -   The stored content is overwritten only when the item is actually notified, so `item.diff` always covers everything since the last mail about that item.
    -   `diff-keys` should be stable across all URLs of a feed group, since items are deduplicated across the group.
    -   Changing `diff-content` or `diff-keys` makes the previously stored content unreachable, so the next notification shows no diff rather than a bogus one. The stale rows expire under `keep-old`.
-   `diff-content`: MiniJinja expression for the text that is stored and diffed. An absent value is stored as an empty string. Note that MiniJinja's lenient undefined behavior only tolerates one level, so `item.content.body` is fine when `item.content` is `none`, but `item.content.body.foo` errors.
-   `diff-granularity`: The unit changes are computed on, which also determines the layout.
    -   `word` (default) / `char`: an inline redline, i.e. the text in reading order with insertions and deletions marked in place.
    -   `line`: a unified diff, i.e. only the changed regions, with `@@` headers and `-`/`+` lines.
-   `diff-context`: How much unchanged context to keep around each change, in units of `diff-granularity`; longer unchanged runs are elided with `…`. Can be a non-negative integer, `'full'` (no elision), or `'auto'` (the default: 30 words / 120 characters / 3 lines).
-   `diff-strip-tags`: Whether to strip HTML tags to readable text before diffing. This only affects the display; the stored content is unaffected, so this can be flipped at any time. Off by default, which diffs the HTML source.
-   `interval`: Check feed update once per interval.
-   `keep-old`: Prune old data in the database. This includes the item contents stored for `diff-keys` — the only table that holds feed content rather than hashes, which is why `diff-keys` is off by default.
-   `timeout`: Timeout when fetching the feed.
-   `retry-count`: Number of consecutive failures before a feed group is considered failing.
-   `retry-interval`: Minimum wait after a failure before retrying a failing feed group. By default (`0s`), failing feeds are retried every check cycle (about once per minute).
-   `sanitize`: Whether to sanitize HTML in feed contents or keep the HTML as it is.
-   `sort-by-last-modified`: Whether to sort items in a digest by their last modified time.
-   `http-headers`: HTTP header map when fetching the feed.

---

-   `url`/`urls`: Feed URLs in the group.
-   `filter`: Filter feed items. Can be one of:
    -   `title-regex` / `body-regex` / `regex`: Regular expression match for title / body / both.
    -   `jinja-expr`: Evaluated as MiniJinja expression to see if it's true.
    -   `and: [..]` (`all: [..]`) / `or: [..]` (`any: [..]`) / `not: {..}`: Logic combination.

---

-   `error-report-to`: Error report recipients when feeds are not working.

## Security

-   Do not load untrusted config files. The config is designed to be flexible but insecure. Untrusted config may lead to SSTI, DoS attacks, and email bombs. This is out of the threat model for this project.
-   See [Security](https://github.com/ouuan/yaf2m/security) for the security policy.
