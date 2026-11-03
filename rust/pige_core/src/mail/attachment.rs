//! MIME attachment metadata, filename hardening, and the on-disk attachment
//! cache.
//!
//! Nothing in this module trusts the message: part ids and filenames are
//! sanitized before they become path components, and attachment bytes are only
//! ever written below `cache_dir/attachments`.

use std::path::{Path, PathBuf};

use mail_parser::{MessageParser, MessagePart};
use serde::{Deserialize, Serialize};

use super::MessageId;

/// The largest attachment pige will write to the cache. A larger part is
/// rejected with [`AttachmentError::TooLarge`] instead of filling the disk.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

/// How a MIME part was presented by the sender. Inline parts with a
/// `Content-ID` are body media, not user-facing attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttachmentDisposition {
    Inline,
    Attachment,
}

impl AttachmentDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Attachment => "attachment",
        }
    }

    pub fn from_db(value: &str) -> Self {
        if value.eq_ignore_ascii_case("inline") {
            Self::Inline
        } else {
            Self::Attachment
        }
    }
}

/// One user-visible attachment, with the stable part id used to fetch its
/// bytes from the raw message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentPart {
    pub part_id: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub disposition: AttachmentDisposition,
}

/// Typed failures from attachment extraction and caching.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("attachment is too large: {size} bytes exceeds the {limit} byte limit")]
    TooLarge { size: u64, limit: u64 },
    #[error("attachment part {0} is not present in the message")]
    UnknownPart(String),
    #[error("attachment path would escape the cache directory")]
    UnsafePath,
    #[error("write attachment: {0}")]
    Io(#[from] std::io::Error),
}

/// Walk the MIME tree and return the parts a user should see as attachments.
///
/// Body text, multiparts, and inline media referenced by a `Content-ID` are
/// skipped. Sizes are the decoded byte counts, not the transfer-encoded size.
pub fn extract_attachments(raw: &[u8]) -> Vec<AttachmentPart> {
    let Some(message) = MessageParser::new().parse(raw) else {
        return Vec::new();
    };

    message
        .attachments()
        .filter(|part| !is_body_media(part))
        .map(|part| AttachmentPart {
            part_id: part.id().to_string(),
            filename: safe_attachment_filename(part.attachment_name().unwrap_or(""), "attachment"),
            content_type: content_type(&part),
            size: part.decoded_len() as u64,
            content_id: part.content_id().map(str::to_string),
            disposition: if part.is_inline() {
                AttachmentDisposition::Inline
            } else {
                AttachmentDisposition::Attachment
            },
        })
        .collect()
}

/// Decode one attachment part by its stable part id. Used after a raw message
/// has been fetched for the reader or for an attachment open.
pub fn attachment_bytes(raw: &[u8], part_id: &str) -> Option<Vec<u8>> {
    let message = MessageParser::new().parse(raw)?;
    message
        .parts()
        .find(|part| part.id().to_string() == part_id)
        .map(|part| part.decoded().into_owned())
}

/// Reject an attachment whose decoded size exceeds [`MAX_ATTACHMENT_BYTES`].
pub fn check_attachment_size(size: u64) -> Result<(), AttachmentError> {
    if size > MAX_ATTACHMENT_BYTES {
        Err(AttachmentError::TooLarge {
            size,
            limit: MAX_ATTACHMENT_BYTES,
        })
    } else {
        Ok(())
    }
}

/// Reduce a sender-supplied filename to one safe path component.
///
/// The last `/` or `\` component is kept, control characters and separators
/// are stripped, and `.`/`..` or an empty result falls back to `fallback`.
/// The fallback is itself sanitized so a broken caller cannot smuggle a path
/// out through it.
pub fn safe_attachment_filename(name: &str, fallback: &str) -> String {
    let candidate = name
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("");
    let cleaned: String = candidate
        .chars()
        .filter(|character| !character.is_control() && *character != '/' && *character != '\\')
        .collect();
    let cleaned = cleaned.trim();

    if is_usable_component(cleaned) {
        return truncate_component(cleaned);
    }

    let fallback: String = fallback
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or("")
        .chars()
        .filter(|character| !character.is_control() && *character != '/' && *character != '\\')
        .collect();
    let fallback = fallback.trim();
    if is_usable_component(fallback) {
        truncate_component(fallback)
    } else {
        "attachment".to_string()
    }
}

/// Write one attachment below `cache_dir/attachments/<message>/<part>/` and
/// return the file path.
///
/// The path is checked both lexically and after resolving symlinks in the
/// parent directory, so a crafted id or an existing symlink cannot redirect
/// the write outside the cache.
pub fn cache_attachment(
    cache_dir: &Path,
    message: &MessageId,
    part: &AttachmentPart,
    bytes: &[u8],
) -> Result<PathBuf, AttachmentError> {
    check_attachment_size(bytes.len() as u64)?;

    let message_component = safe_attachment_filename(&message.0, "message");
    let part_component = safe_attachment_filename(&part.part_id, "part");
    let filename = safe_attachment_filename(&part.filename, "attachment");

    let root = cache_dir.join("attachments");
    let directory = root.join(&message_component).join(&part_component);
    let path = directory.join(&filename);

    if !path.starts_with(&root) {
        return Err(AttachmentError::UnsafePath);
    }

    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&directory)?;

    let canonical_root = std::fs::canonicalize(&root)?;
    let canonical_directory = std::fs::canonicalize(&directory)?;
    if !canonical_directory.starts_with(&canonical_root) {
        return Err(AttachmentError::UnsafePath);
    }

    // Replace rather than follow a pre-existing symlink at the final name.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut file, bytes)?;

    Ok(path)
}

fn is_body_media(part: &MessagePart<'_>) -> bool {
    part.is_inline() && part.content_id().is_some()
}

fn content_type(part: &MessagePart<'_>) -> String {
    let Some(content_type) = part.content_type() else {
        return "application/octet-stream".to_string();
    };
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_string(),
    }
}

fn is_usable_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".."
}

fn truncate_component(value: &str) -> String {
    // Keep the component comfortably below the common 255-byte NAME_MAX while
    // never splitting a UTF-8 character.
    const MAX_BYTES: usize = 200;
    if value.len() <= MAX_BYTES {
        return value.to_string();
    }
    let mut end = MAX_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_keep_only_the_last_component() {
        assert_eq!(safe_attachment_filename("a/b/c.txt", "fallback"), "c.txt");
        assert_eq!(safe_attachment_filename("a\\b\\c.txt", "fallback"), "c.txt");
        assert_eq!(safe_attachment_filename("...", "fallback"), "...");
        assert_eq!(safe_attachment_filename("../", "fallback"), "fallback");
    }

    #[test]
    fn unsafe_fallbacks_become_attachment() {
        assert_eq!(safe_attachment_filename("", ".."), "attachment");
        assert_eq!(safe_attachment_filename("..", "../x"), "x");
    }

    #[test]
    fn long_names_are_truncated_on_a_character_boundary() {
        let name = "é".repeat(300);
        let safe = safe_attachment_filename(&name, "fallback");
        assert!(safe.len() <= 200);
        assert!(safe.is_char_boundary(safe.len()));
    }
}
