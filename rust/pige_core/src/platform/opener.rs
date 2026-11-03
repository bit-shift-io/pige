//! Small, platform-neutral helpers shared by platform openers.
//!
//! The Android bridge hands a URL to an `ACTION_VIEW` intent and a file to a
//! `FileProvider` URI. Both are capability-bearing operations, so the target is
//! checked here before it crosses the JNI boundary: URLs are restricted to the
//! same schemes the HTML sanitizer allows, and files must resolve below the
//! attachment cache.

use std::path::{Path, PathBuf};

/// Keep only schemes the mail UI may hand to the OS. The HTML sanitizer already
/// restricts rendered links to these; this is the second line of defence for
/// other callers (for example a future "open in browser" action).
pub fn checked_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(format!("refusing to open a URL without a scheme: {url}"));
    };
    if rest.is_empty() {
        return Err(format!("refusing to open an empty {scheme}: URL"));
    }
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => {
            let authority = rest
                .strip_prefix("//")
                .ok_or_else(|| format!("refusing to open a malformed {scheme}: URL: {url}"))?;
            let host = authority.split(['/', '?', '#']).next().unwrap_or_default();
            if host.is_empty() {
                return Err(format!("refusing to open {scheme}: URL without a host"));
            }
            Ok(url)
        }
        "mailto" => Ok(url),
        other => Err(format!("refusing to open unsupported URL scheme: {other}")),
    }
}

/// Canonicalize `path` and require it to resolve to a regular file below
/// `root`. This keeps a stale or tampered cache row from handing an arbitrary
/// file to the OS handler.
pub fn file_below(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| format!("resolve {}: {error}", root.display()))?;
    let canonical_path = std::fs::canonicalize(path)
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(format!(
            "refusing to open a file outside {}: {}",
            canonical_root.display(),
            canonical_path.display()
        ));
    }
    if !canonical_path.is_file() {
        return Err(format!(
            "refusing to open a non-file: {}",
            canonical_path.display()
        ));
    }
    Ok(canonical_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_limited_to_safe_schemes() {
        for url in [
            "https://example.com/mail",
            "http://example.com",
            "mailto:ada@example.com",
        ] {
            assert_eq!(checked_url(url), Ok(url));
        }
        for url in [
            "javascript:alert(1)",
            "file:///data/data/bitshift.pige/pige.db",
            "content://com.example/file",
            "https://",
            "example.com",
        ] {
            assert!(checked_url(url).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn files_must_resolve_below_the_attachment_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("attachments");
        let nested = root.join("message").join("part");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        let inside = nested.join("report.pdf");
        std::fs::write(&inside, b"pdf").expect("write inside");

        assert_eq!(
            file_below(&root, &inside).expect("inside file"),
            std::fs::canonicalize(&inside).unwrap()
        );

        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, b"secret").expect("write outside");
        assert!(file_below(&root, &outside).is_err());

        // A symlink that points out of the cache is rejected after resolution.
        #[cfg(unix)]
        {
            let link = root.join("escape");
            std::os::unix::fs::symlink(&outside, &link).expect("symlink");
            assert!(file_below(&root, &link).is_err());
        }

        assert!(file_below(&root, &root.join("missing")).is_err());
    }
}
