/// Escape text so that it is inert when rendered as HTML.
///
/// This is the set MiniJinja's own autoescape uses, which is what makes it the right level here:
/// feed text that goes into a template *without* `| safe` already gets exactly this, so text that
/// has to be pre-escaped (because a template renders it with `| safe`) is protected identically.
///
/// `ammonia::clean_text` additionally escapes space, `` ` ``, `/`, `=`, and the other ASCII
/// whitespace, purely so the result also survives an *unquoted* attribute value. Nothing here ever
/// lands in one, and the extra escaping inflates ordinary prose by well over half while making the
/// text unreadable in the database and in trace logs.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped_text(&mut out, s);
    out
}

/// [`escape_text`], appending to an existing buffer.
pub fn push_escaped_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_the_html_metacharacters_and_nothing_else() {
        assert_eq!(
            escape_text(r#"<a href="x">Tom & Jerry's</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; Jerry&#39;s&lt;/a&gt;",
        );
        // Ordinary prose passes through untouched.
        assert_eq!(
            escape_text("Requires Node 20 / npm 10."),
            "Requires Node 20 / npm 10.",
        );
        assert_eq!(escape_text("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn push_escaped_text_appends() {
        let mut out = String::from("prefix:");
        push_escaped_text(&mut out, "<x>");
        assert_eq!(out, "prefix:&lt;x&gt;");
    }
}
