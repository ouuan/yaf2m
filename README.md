# yaf2m (Yet Another Feed to Mail)

Send email alerts or digests when your RSS/Atom feeds update.

## Quick Start

-   Write a config file with your feeds.
-   Set the environment variables described in the next section.
-   Start the service with Docker Compose: `docker compose up -d` (see [`docker-compose.yml`](./docker-compose.yml)).

## Environment Variables

-   `YAF2M_CONFIG_PATH`: path to the config file (default: `config.toml` in PWD).
-   `POSTGRES_URL`: database connection string; see [sqlx::postgres::PgConnectOptions](https://docs.rs/sqlx/latest/sqlx/postgres/struct.PgConnectOptions.html).
-   `SMTP_FROM`: sender address, e.g. `"yaf2m" <yaf2m@example.com>`.
-   `SMTP_URL`: SMTP transport URL; see [lettre::transport::smtp::SmtpTransport::from_url](https://docs.rs/lettre/latest/lettre/transport/smtp/struct.SmtpTransport.html#method.from_url).

## Config File

Note: The config file is auto-reloaded. There is no need to restart the service.

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
[settings]
to = []
cc = []
bcc = []
digest = false
item-subject = <src/templates/item-subject.txt>
digest-subject = <src/templates/digest-subject.txt>
item-body = <src/templates/item-body.html>
digest-body = <src/templates/digest-body.html>
template-args = {}
update-key = 'item.id'
interval = '1h'
keep-old = '1w'
timeout = '30s'
max-mail-per-check = 5
sanitize = true

[[feeds]]
url = "https://blog.rust-lang.org/feed.xml"
# urls = ["https://example.org/feed.atom", "https://example.net/feed.json"]
# To override [settings]:
# to = ["Alice <alice@example.com>", "bob@example.org"]
# cc = "john@example.com" is the same as cc = ["john@example.com"]
# bcc = []
# digest = true
# item-subject.inline = "{{ item.title.content }}"
# digest-subject.inline = "My daily feed on {{ now() | dateformat(tz=template_args.tz) }}"
# item-body.file = "/path/to/item-template.html"
# digest-body.file = "/path/to/item-template.html"
# template-args.tz = "Asia/Shanghai"
# update-keys = ['item.summary', 'item.content']
# interval = '1d'
# keep-old = '2w'
# timeout = '1m'
# max-mail-per-check = 1
# sanitize = false
[[feeds.filter.or]]
title-regex = '^Announcing'
[[feeds.filter.or]]
and = [
    { not.body-regex = 'foo' },
    { jinja-expr = 'item.author.name is matches("John")' },
]
```

### Structure

-   Feeds are organized as groups (`[[feeds]]`). One group may contain one or more feed URLs. Feeds in the same group are combined together and items are deduplicated.
-   `urls` and `filter` are group-specific. Other settings may have a global default value in `[settings]`. Settings resolve in order: value on the feed group -> value in `[settings]` -> built-in default.

### Fields

-   `to`, `cc`, `bcc`: Mail recipients. Each can be a single string or an array of strings.
-   `digest`: Whether to send all updates in a single digest mail or to send one mail per item.
-   `item-subject`, `digest-subject`, `item-body`, `digest-body`: [MiniJinja](https://docs.rs/minijinja) templates for mail contents.
    -   Can be `{ inline = "{{ template }}" }` or `{ file = "/path/to/template" }`.
    -   Default templates: [`src/templates`](./src/templates).
    -   Context for single item: `{ feed => Feed, item => Entry }`, see [`feed_rs::model::Feed`](https://docs.rs/feed-rs/latest/feed_rs/model/struct.Feed.html) and [`feed_rs::model::Entry`](https://docs.rs/feed-rs/latest/feed_rs/model/struct.Entry.html).
    -   Context for digest: `{ feeds => [Feed], items => [{ feed => Feed, item => Entry }] }`, where `feeds` are all feeds in the group (no matter updated or not), and `items` are updated items.
    -   Custom args: `template-args`.
    -   More features: [`filters`](https://docs.rs/minijinja/latest/minijinja/filters/index.html), [`tests`](https://docs.rs/minijinja/latest/minijinja/tests/index.html), [`minijinja-contrib`](https://docs.rs/minijinja-contrib/latest/minijinja_contrib/), and a custom test `matches(regex)`.
-   `template-args`: Custom args that are passed to the MiniJinja templates. Template args set on each feed are merged with the global setting.
-   `update-keys`/`update-key`: Keys that are used to check whether a feed item is updated or not. Each key is a MiniJinja expression. This can be used to control whether to notify feed content update.
-   `interval`: Check feed update once per interval.
-   `keep-old`: Prune old data in the database.
-   `timeout`: Timeout when fetching the feed.
-   `max-mail-per-check`: Send digest if there are too many updates, even if `digest = false`.
-   `sanitize`: Whether to sanitize HTML in feed contents.

---

-   `url`/`urls`: Feed URLs in the group.
-   `filter`: Filter feed items. Can be one of:
    -   `title-regex` / `body-regex`: Regular expression match.
    -   `jinja-expr`: Evaluated as MiniJinja expression to see if it's true.
    -   `and: [..]` / `or: [..]` / `not: {..}`: Logic combination.

## Security

-   Do not load untrusted config files. The config is designed to be flexible but insecure. Untrusted config may lead to SSTI, DoS attacks, and email bombs. This is out of the threat model for this project.
-   See [Security](https://github.com/ouuan/yaf2m/security) for the security policy.
