use crate::escape::push_escaped_text;
use minijinja_contrib::filters::striptags;
use regex::Regex;
use serde::Deserialize;
use similar::{ChangeTag, DiffTag, TextDiff};
use std::borrow::Cow;
use std::fmt::Write;
use std::sync::LazyLock;
use std::time::Duration;

/// Skip diffing when the two sides together exceed this, since `render_diff` runs on a tokio
/// worker while a transaction is open.
const MAX_DIFF_BYTES: usize = 1 << 20;
/// The diff algorithms approximate instead of running forever once this elapses.
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

const AUTO_CONTEXT_WORDS: usize = 30;
const AUTO_CONTEXT_CHARS: usize = 120;
const AUTO_CONTEXT_LINES: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct DiffOptions {
    pub granularity: DiffGranularity,
    pub context: DiffContext,
    pub strip_tags: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffGranularity {
    Word,
    Char,
    Line,
}

/// How much unchanged context to keep around each change, in units of the granularity.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged, expecting = "a non-negative integer, \"full\", or \"auto\"")]
pub enum DiffContext {
    Count(usize),
    Keyword(DiffContextKeyword),
}

/// Unit variants deserialize from strings, which an untagged variant would not.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffContextKeyword {
    Auto,
    Full,
}

impl DiffContext {
    /// `None` means unlimited context.
    fn resolve(self, granularity: DiffGranularity) -> Option<usize> {
        match self {
            Self::Count(n) => Some(n),
            Self::Keyword(DiffContextKeyword::Full) => None,
            Self::Keyword(DiffContextKeyword::Auto) => Some(match granularity {
                DiffGranularity::Word => AUTO_CONTEXT_WORDS,
                DiffGranularity::Char => AUTO_CONTEXT_CHARS,
                DiffGranularity::Line => AUTO_CONTEXT_LINES,
            }),
        }
    }
}

const CONTAINER_OPEN: &str = r#"<div class="diff" style="border: 1px solid #e5e5e5; border-radius: 0.375rem; background: #f8f9fa; padding: 0.75rem; margin: 0.75rem 0; white-space: pre-wrap; word-break: break-word;">"#;
const CONTAINER_CLOSE: &str = "</div>";
const PRE_OPEN: &str = r#"<pre style="margin: 0; white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.9em;">"#;
const PRE_CLOSE: &str = "</pre>";
const DEL_OPEN: &str =
    r#"<del style="background: #ffebe9; color: #82071e; text-decoration: line-through;">"#;
const INS_OPEN: &str =
    r#"<ins style="background: #e6ffec; color: #0a3622; text-decoration: underline;">"#;
const DEL_LINE_OPEN: &str =
    r#"<del style="display: block; background: #ffebe9; color: #82071e; text-decoration: none;">"#;
const INS_LINE_OPEN: &str =
    r#"<ins style="display: block; background: #e6ffec; color: #0a3622; text-decoration: none;">"#;
const DEL_CLOSE: &str = "</del>";
const INS_CLOSE: &str = "</ins>";
const HUNK_OPEN: &str = r#"<span style="color: #6f42c1;">"#;
const HUNK_CLOSE: &str = "</span>";
const ELLIPSIS: &str = r#"<span style="color: #999;"> … </span>"#;

/// Render the change from `old` to `new` as a self-contained HTML fragment.
///
/// Returns `None` when there is nothing worth showing: the two sides are equal, no change survived
/// the requested context, or the input is too large to diff.
///
/// Every byte of `old`/`new` that reaches the output goes through [`push_escaped_text`]; the only
/// markup emitted is this function's own wrappers. Callers render the result with `| safe`, so
/// this is the sole trust boundary.
pub fn render_diff(old: &str, new: &str, opts: DiffOptions) -> Option<String> {
    let (old, new) = if opts.strip_tags {
        (Cow::Owned(html_to_text(old)), Cow::Owned(html_to_text(new)))
    } else {
        (Cow::Borrowed(old), Cow::Borrowed(new))
    };

    if old == new {
        return None;
    }

    if old.len() + new.len() > MAX_DIFF_BYTES {
        log::debug!(
            "Skipping diff of {} + {} bytes, over the {MAX_DIFF_BYTES} byte limit",
            old.len(),
            new.len(),
        );
        return None;
    }

    let context = opts.context.resolve(opts.granularity);
    let config = {
        let mut config = TextDiff::configure();
        config.timeout(DIFF_TIMEOUT);
        config
    };

    let body = match opts.granularity {
        DiffGranularity::Word => inline_redline(
            &config.diff_unicode_words(old.as_ref(), new.as_ref()),
            context,
        ),
        DiffGranularity::Char => {
            inline_redline(&config.diff_graphemes(old.as_ref(), new.as_ref()), context)
        }
        DiffGranularity::Line => {
            unified_hunks(&config.diff_lines(old.as_ref(), new.as_ref()), context)
        }
    }?;

    Some(format!("{CONTAINER_OPEN}{body}{CONTAINER_CLOSE}"))
}

/// The text in reading order, with insertions and deletions marked in place.
fn inline_redline(diff: &TextDiff<'_, '_, str>, context: Option<usize>) -> Option<String> {
    // One tag per token produces enormous HTML, so consecutive same-tag changes share a wrapper.
    let mut runs: Vec<(ChangeTag, Vec<&str>)> = Vec::new();
    for change in diff.iter_all_changes() {
        match runs.last_mut() {
            Some((tag, tokens)) if *tag == change.tag() => tokens.push(change.value()),
            _ => runs.push((change.tag(), vec![change.value()])),
        }
    }

    if !runs.iter().any(|(tag, _)| *tag != ChangeTag::Equal) {
        return None;
    }

    let last_index = runs.len() - 1;
    let mut out = String::new();

    for (i, (tag, tokens)) in runs.iter().enumerate() {
        match tag {
            ChangeTag::Delete => {
                out.push_str(DEL_OPEN);
                push_escaped(&mut out, tokens);
                out.push_str(DEL_CLOSE);
            }
            ChangeTag::Insert => {
                out.push_str(INS_OPEN);
                push_escaped(&mut out, tokens);
                out.push_str(INS_CLOSE);
            }
            ChangeTag::Equal => match context {
                None => push_escaped(&mut out, tokens),
                Some(context) => {
                    // Context is only needed on the sides that face a change.
                    let head = if i == 0 { 0 } else { context };
                    let tail = if i == last_index { 0 } else { context };
                    if tokens.len() > head + tail {
                        push_escaped(&mut out, &tokens[..head]);
                        out.push_str(ELLIPSIS);
                        push_escaped(&mut out, &tokens[tokens.len() - tail..]);
                    } else {
                        push_escaped(&mut out, tokens);
                    }
                }
            },
        }
    }

    Some(out)
}

/// Only the changed regions, as a unified diff.
fn unified_hunks(diff: &TextDiff<'_, '_, str>, context: Option<usize>) -> Option<String> {
    // `grouped_ops` computes `n * 2`, so the radius must stay well away from `usize::MAX`.
    let radius = context.unwrap_or_else(|| diff.old_len().max(diff.new_len()));
    let groups = diff.grouped_ops(radius);

    if !groups.iter().flatten().any(|op| op.tag() != DiffTag::Equal) {
        return None;
    }

    let mut out = String::from(PRE_OPEN);

    for group in &groups {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old = first.old_range().start..last.old_range().end;
        let new = first.new_range().start..last.new_range().end;
        // A zero-length side is addressed by the line it follows, per unified diff convention.
        let old_start = if old.is_empty() {
            old.start
        } else {
            old.start + 1
        };
        let new_start = if new.is_empty() {
            new.start
        } else {
            new.start + 1
        };
        let _ = writeln!(
            out,
            "{HUNK_OPEN}@@ -{old_start},{} +{new_start},{} @@{HUNK_CLOSE}",
            old.len(),
            new.len(),
        );

        for change in group.iter().flat_map(|op| diff.iter_changes(op)) {
            let line = change.value();
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (open, prefix, close) = match change.tag() {
                ChangeTag::Equal => ("", ' ', ""),
                ChangeTag::Delete => (DEL_LINE_OPEN, '-', DEL_CLOSE),
                ChangeTag::Insert => (INS_LINE_OPEN, '+', INS_CLOSE),
            };
            out.push_str(open);
            out.push(prefix);
            push_escaped_text(&mut out, line);
            out.push_str(close);
            out.push('\n');
        }
    }

    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out.push_str(PRE_CLOSE);

    Some(out)
}

fn push_escaped(out: &mut String, tokens: &[&str]) {
    for token in tokens {
        push_escaped_text(out, token);
    }
}

static BLOCK_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)</?\s*(p|div|br|li|ul|ol|tr|table|thead|tbody|h[1-6]|blockquote|pre|section|article|header|footer|hr|figure|figcaption|dl|dt|dd)\b[^>]*>",
    )
    .expect("invalid block tag regex")
});

/// Turn HTML into the readable text it renders as, one line per block-level element.
///
/// The block split has to come first: `striptags` collapses every whitespace run, newlines
/// included, into a single space.
fn html_to_text(s: &str) -> String {
    let mut out = String::new();
    for chunk in BLOCK_TAGS.split(s) {
        let text = striptags(chunk.to_string());
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(granularity: DiffGranularity) -> DiffOptions {
        DiffOptions {
            granularity,
            context: DiffContext::Keyword(DiffContextKeyword::Auto),
            strip_tags: false,
        }
    }

    #[test]
    fn identical_input_produces_no_diff() {
        assert!(render_diff("same text", "same text", options(DiffGranularity::Word)).is_none());
        assert!(render_diff("same text", "same text", options(DiffGranularity::Char)).is_none());
        assert!(render_diff("same\ntext", "same\ntext", options(DiffGranularity::Line)).is_none());
    }

    #[test]
    fn input_differing_only_in_stripped_tags_produces_no_diff() {
        let opts = DiffOptions {
            strip_tags: true,
            ..options(DiffGranularity::Word)
        };
        assert!(render_diff("<p>hello</p>", "<div>hello</div>", opts).is_none());
    }

    #[test]
    fn oversized_input_is_skipped() {
        let old = "a ".repeat(MAX_DIFF_BYTES);
        let new = "b ".repeat(MAX_DIFF_BYTES);
        assert!(render_diff(&old, &new, options(DiffGranularity::Word)).is_none());
    }

    #[test]
    fn word_granularity_coalesces_runs_into_one_tag_each() {
        let diff = render_diff(
            "Requires Node 18 and npm 9",
            "Requires Node 20 or later and npm 9",
            options(DiffGranularity::Word),
        )
        .expect("expected a diff");

        // A single contiguous replacement, however many tokens it spans, gets one tag each.
        assert_eq!(diff.matches("<del ").count(), 1, "{diff}");
        assert_eq!(diff.matches("<ins ").count(), 1, "{diff}");
        assert!(diff.contains(&format!("{DEL_OPEN}18{DEL_CLOSE}")), "{diff}");
        assert!(
            diff.contains(&format!("{INS_OPEN}20 or later{INS_CLOSE}")),
            "{diff}"
        );
        assert!(diff.contains("Requires Node "), "{diff}");
        assert!(diff.contains(" and npm 9"), "{diff}");
    }

    #[test]
    fn output_escapes_feed_html() {
        for granularity in [
            DiffGranularity::Word,
            DiffGranularity::Char,
            DiffGranularity::Line,
        ] {
            let diff = render_diff(
                "harmless",
                "<script>alert(1)</script>",
                options(granularity),
            )
            .expect("expected a diff");

            // The security-critical property, given the templates render this with `| safe`.
            assert!(!diff.contains("<script"), "{granularity:?}: {diff}");
            assert!(!diff.contains("alert(1)<"), "{granularity:?}: {diff}");
            assert!(diff.contains("&lt;"), "{granularity:?}: {diff}");
        }

        // Word and line granularity keep the payload in one run, so it survives verbatim-escaped.
        for granularity in [DiffGranularity::Word, DiffGranularity::Line] {
            let diff = render_diff(
                "harmless",
                "<script>alert(1)</script>",
                options(granularity),
            )
            .expect("expected a diff");
            assert!(
                diff.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
                "{granularity:?}: {diff}"
            );
        }
    }

    #[test]
    fn char_granularity_marks_single_characters() {
        let diff = render_diff(
            "version 1.2.3",
            "version 1.2.4",
            options(DiffGranularity::Char),
        )
        .expect("expected a diff");

        assert!(diff.contains(&format!("{DEL_OPEN}3{DEL_CLOSE}")));
        assert!(diff.contains(&format!("{INS_OPEN}4{INS_CLOSE}")));
    }

    #[test]
    fn line_granularity_produces_hunks_honouring_the_context_radius() {
        let old = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let new = old.replace("line 10\n", "line ten\n");

        let diff = render_diff(
            &old,
            &new,
            DiffOptions {
                context: DiffContext::Count(2),
                ..options(DiffGranularity::Line)
            },
        )
        .expect("expected a diff");

        // 2 lines of context on each side of the single changed line.
        assert!(diff.contains("@@ -8,5 +8,5 @@"), "{diff}");
        assert!(diff.contains("-line 10"), "{diff}");
        assert!(diff.contains("+line ten"), "{diff}");
        assert!(diff.contains(" line 8"), "{diff}");
        assert!(!diff.contains("line 7"), "{diff}");
        assert!(diff.contains(" line 12"), "{diff}");
        assert!(!diff.contains("line 13"), "{diff}");
    }

    #[test]
    fn context_elides_long_equal_runs_and_full_does_not() {
        let filler = (1..=100).map(|i| format!("word{i} ")).collect::<String>();
        let old = format!("{filler}alpha {filler}");
        let new = format!("{filler}beta {filler}");

        let elided = render_diff(
            &old,
            &new,
            DiffOptions {
                context: DiffContext::Count(3),
                ..options(DiffGranularity::Word)
            },
        )
        .expect("expected a diff");
        assert!(elided.contains(ELLIPSIS));
        assert!(elided.contains("word100"));
        assert!(!elided.contains("word50"));

        let full = render_diff(
            &old,
            &new,
            DiffOptions {
                context: DiffContext::Keyword(DiffContextKeyword::Full),
                ..options(DiffGranularity::Word)
            },
        )
        .expect("expected a diff");
        assert!(!full.contains(ELLIPSIS));
        assert!(full.contains("word50"));
    }

    #[test]
    fn zero_context_keeps_only_the_changes() {
        let diff = render_diff(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown cat jumps over the lazy dog",
            DiffOptions {
                context: DiffContext::Count(0),
                ..options(DiffGranularity::Word)
            },
        )
        .expect("expected a diff");

        assert!(!diff.contains("quick"));
        assert!(!diff.contains("lazy"));
        assert!(diff.contains(&format!("{DEL_OPEN}fox{DEL_CLOSE}")));
        assert!(diff.contains(&format!("{INS_OPEN}cat{INS_CLOSE}")));
    }

    #[test]
    fn strip_tags_diffs_readable_text() {
        let diff = render_diff(
            "<p>Requires <b>Node 18</b></p>",
            "<p>Requires <b>Node 20</b></p>",
            DiffOptions {
                strip_tags: true,
                ..options(DiffGranularity::Word)
            },
        )
        .expect("expected a diff");

        assert!(!diff.contains("&lt;b&gt;"), "{diff}");
        assert!(diff.contains("Requires Node "), "{diff}");
    }

    #[test]
    fn html_to_text_splits_blocks_decodes_entities_and_collapses_inline_tags() {
        assert_eq!(html_to_text("<p>first</p><p>second</p>"), "first\nsecond");
        assert_eq!(html_to_text("a<br>b"), "a\nb");
        assert_eq!(
            html_to_text("<ul><li>one</li><li>two</li></ul>"),
            "one\ntwo"
        );
        assert_eq!(html_to_text("Tom &amp; Jerry"), "Tom & Jerry");
        // `html_entities` is what makes `&nbsp;` resolve at all instead of surviving verbatim.
        assert_eq!(html_to_text("a&nbsp;b"), "a b");
        assert_eq!(
            html_to_text("<p>an <b>inline</b> <i>tag</i></p>"),
            "an inline tag"
        );
        // <p> must not be matched inside <pre>
        assert_eq!(html_to_text("<pre>code</pre>"), "code");
    }
}
