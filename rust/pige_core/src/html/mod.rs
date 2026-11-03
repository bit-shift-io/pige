//! Safe HTML handling for mail bodies.
//!
//! Raw HTML is allowlist-sanitized, parsed with `html5ever`, and turned into a
//! small native [`MailDocument`] AST that the UI can render without a WebView.
//! No scripts, remote fetches, or external styles survive this pipeline.

mod ast;
mod parse;
mod sanitize;

pub use ast::{Block, ImageSource, Inline, InlineStyle, MailDocument};
pub use parse::{parse_html, plain_text_document};
pub use sanitize::MAX_INPUT_BYTES;
