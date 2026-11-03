//! Allowlist HTML sanitization via `ammonia`, plus an input-size cap.
//!
//! The sanitizer is the only place raw HTML enters the pipeline. It keeps a
//! small set of structural and formatting elements, strips every attribute
//! except `href` (links), `src` and `alt` (images), and removes scripts,
//! styles, forms, iframes, and other dangerous subtrees entirely. Because
//! attributes are stripped first, event handlers, inline styles, and `data:`
//! payloads cannot reach the parser.

use std::collections::{HashMap, HashSet};

use ammonia::Builder;

/// The largest accepted HTML input, in bytes. Larger inputs sanitize to an
/// empty string rather than being parsed.
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

/// Elements the sanitizer keeps. Unknown elements are flattened to children.
const ALLOWED_TAGS: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "hr",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "caption",
    "tr",
    "td",
    "th",
    "b",
    "strong",
    "i",
    "em",
    "u",
    "s",
    "strike",
    "small",
    "sub",
    "sup",
    "a",
    "img",
    "br",
    "div",
    "span",
];

/// Elements whose entire subtree is dropped from the output.
const CLEAN_CONTENT_TAGS: &[&str] = &[
    "script", "style", "iframe", "form", "object", "embed", "link", "meta", "base", "title",
    "noscript", "template", "frame", "frameset", "applet",
];

/// Build the allowlist sanitizer. Reused across parses so the rule set lives in
/// exactly one place.
fn builder() -> Builder<'static> {
    let tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();
    let clean_content: HashSet<&str> = CLEAN_CONTENT_TAGS.iter().copied().collect();
    let url_schemes: HashSet<&str> = ["http", "https", "mailto", "cid"].into_iter().collect();

    let mut builder = Builder::default();
    builder.tags(tags);
    // Strip every default attribute: only href/src/alt survive below.
    builder.tag_attributes(HashMap::new());
    builder.generic_attributes(HashSet::new());
    builder.url_schemes(url_schemes);
    // No `rel` rewriting; links are re-checked by the parser anyway.
    builder.link_rel(None);
    builder.clean_content_tags(clean_content);
    builder.add_tag_attributes("a", ["href"]);
    builder.add_tag_attributes("img", ["src", "alt"]);
    builder
}

/// Sanitize `input`, returning an empty string when it exceeds the size cap.
pub fn sanitize(input: &str) -> String {
    if input.len() > MAX_INPUT_BYTES {
        return String::new();
    }
    builder().clean(input).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_event_handlers_and_styles() {
        let clean = sanitize("<p onclick='x()' style='color:red'>hi</p>");
        assert_eq!(clean, "<p>hi</p>");
    }

    #[test]
    fn sanitize_drops_scripts_styles_and_forms() {
        let clean =
            sanitize("<p>hi</p><script>alert(1)</script><style>.a{}</style><form><input></form>");
        assert_eq!(clean, "<p>hi</p>");
    }

    #[test]
    fn sanitize_keeps_only_safe_link_schemes() {
        assert_eq!(
            sanitize("<a href='https://example.com'>ok</a>"),
            "<a href=\"https://example.com\">ok</a>"
        );
        // `javascript:` is stripped, leaving a link with no href.
        assert_eq!(sanitize("<a href='javascript:alert(1)'>x</a>"), "<a>x</a>");
    }

    #[test]
    fn sanitize_preserves_cid_images() {
        assert_eq!(
            sanitize("<img src='cid:logo@x' alt='logo'>"),
            "<img src=\"cid:logo@x\" alt=\"logo\">"
        );
    }

    #[test]
    fn oversized_input_sanitizes_to_empty() {
        let big = "a".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(sanitize(&big), "");
    }
}
