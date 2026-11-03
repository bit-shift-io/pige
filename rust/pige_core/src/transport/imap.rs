//! Real IMAP transport over rustls TLS, implementing [`MailTransport`].

use async_imap::Session;
use async_imap::types::{Fetch, Flag, NameAttribute};
use async_trait::async_trait;
use base64::Engine as _;
use futures::TryStreamExt;

use crate::account::Account;
use crate::mail::{Flags, FolderKind, Uid};
use crate::transport::{
    Capabilities, FlagOp, FolderState, MailTransport, RemoteFolder, RemoteMessage, TransportError,
    UidRange,
};

use super::tls::{self, TlsStream};

/// A [`MailTransport`] backed by a live IMAP connection.
pub struct ImapTransport {
    session: Session<TlsStream>,
    /// Lazily-fetched raw capability set, cached so commands that need to
    /// branch on capabilities don't re-send `CAPABILITY` every time.
    capabilities: Option<async_imap::types::Capabilities>,
}

impl ImapTransport {
    /// Fetch the server capabilities once and cache them for the connection.
    async fn ensure_capabilities(&mut self) -> Result<(), TransportError> {
        if self.capabilities.is_none() {
            let capabilities = self.session.capabilities().await.map_err(transport_err)?;
            self.capabilities = Some(capabilities);
        }
        Ok(())
    }

    /// Whether the cached capability set contains `name`.
    fn has_capability(&self, name: &str) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|caps| caps.has_str(name))
    }
}

/// Connect to `account`'s IMAP server using password authentication and the
/// account's configured security mode.
pub async fn connect(account: &Account, password: &str) -> Result<ImapTransport, TransportError> {
    let client = tls::open(&account.imap.host, account.imap.port, account.imap.security).await?;
    let username = if account.username.is_empty() {
        &account.email
    } else {
        &account.username
    };
    let session = client
        .login(username, password)
        .await
        .map_err(|(e, _)| TransportError(format!("login {}: {e}", account.imap.host)))?;
    Ok(ImapTransport {
        session,
        capabilities: None,
    })
}

/// Classify a mailbox from its LIST name and attributes.
///
/// RFC 6154 SPECIAL-USE attributes win when present; otherwise the mailbox
/// name's final path component is matched against well-known Gmail and generic
/// server names. Anything unrecognized is [`FolderKind::Other`].
pub fn classify_folder(name: &str, attributes: &[&str]) -> FolderKind {
    for attribute in attributes {
        let attribute = attribute.trim().trim_start_matches('\\');
        match attribute.to_ascii_lowercase().as_str() {
            "inbox" => return FolderKind::Inbox,
            "sent" => return FolderKind::Sent,
            "drafts" => return FolderKind::Drafts,
            "trash" => return FolderKind::Trash,
            "archive" | "all" => return FolderKind::Archive,
            "junk" => return FolderKind::Junk,
            _ => {}
        }
    }

    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
    match leaf.trim().to_ascii_lowercase().as_str() {
        "inbox" => FolderKind::Inbox,
        "sent" | "sent mail" | "sent items" | "sent messages" => FolderKind::Sent,
        "drafts" => FolderKind::Drafts,
        "trash" | "deleted" | "deleted items" | "deleted messages" | "bin" => FolderKind::Trash,
        "archive" | "all mail" => FolderKind::Archive,
        "junk" | "spam" | "junk mail" | "junk e-mail" => FolderKind::Junk,
        _ => FolderKind::Other,
    }
}

#[async_trait]
impl MailTransport for ImapTransport {
    async fn capabilities(&mut self) -> Result<Capabilities, TransportError> {
        self.ensure_capabilities().await?;
        Ok(Capabilities {
            gmail: self.has_capability("X-GM-EXT-1"),
            move_ext: self.has_capability("MOVE"),
        })
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>, TransportError> {
        let names = self
            .session
            .list(None, Some("*"))
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(names);

        let mut folders = Vec::new();
        while let Some(name) = names.try_next().await.map_err(transport_err)? {
            // Folder names arrive modified-UTF-7 encoded; decode before
            // classifying or persisting so display and matching are correct.
            let decoded = decode_modified_utf7(name.name());
            let attributes: Vec<&str> = name.attributes().iter().map(attribute_str).collect();
            let kind = classify_folder(&decoded, &attributes);
            folders.push(RemoteFolder {
                name: decoded,
                delimiter: name.delimiter().map(str::to_string),
                kind,
            });
        }
        Ok(folders)
    }

    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState, TransportError> {
        let mailbox = self
            .session
            .select(encode_modified_utf7(&folder.name))
            .await
            .map_err(transport_err)?;
        Ok(FolderState {
            uidvalidity: mailbox.uid_validity.unwrap_or(0),
            uidnext: mailbox.uid_next.unwrap_or(0),
            exists: mailbox.exists,
        })
    }

    async fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        let set = format!("{}:{}", range.from.0, range.to.0);
        let stream = self
            .session
            .uid_fetch(set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(stream);

        let mut messages = Vec::new();
        while let Some(fetch) = stream.try_next().await.map_err(transport_err)? {
            let Some(uid) = fetch.uid else { continue };
            let Some(header_bytes) = fetch.header() else {
                continue;
            };
            // One malformed header must not abort the whole batch.
            let Ok(mut header) = crate::mail::parse_header_block(header_bytes) else {
                continue;
            };
            let size = fetch.size.unwrap_or(0);
            header.size = size;
            messages.push(RemoteMessage {
                uid: Uid(uid),
                header,
                flags: flags_from_fetch(&fetch),
                size,
            });
        }
        Ok(messages)
    }

    async fn fetch_texts(
        &mut self,
        uids: &[Uid],
        max_bytes: usize,
    ) -> Result<Vec<(Uid, String)>, TransportError> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let query = format!("(BODY.PEEK[TEXT]<0.{max_bytes}>)");
        let stream = self
            .session
            .uid_fetch(uid_set(uids), query)
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(stream);

        let mut texts = Vec::new();
        while let Some(fetch) = stream.try_next().await.map_err(transport_err)? {
            let Some(uid) = fetch.uid else { continue };
            if let Some(text) = fetch.text() {
                texts.push((Uid(uid), String::from_utf8_lossy(text).into_owned()));
            }
        }
        Ok(texts)
    }

    async fn fetch_body(&mut self, uid: Uid) -> Result<Vec<u8>, TransportError> {
        let stream = self
            .session
            .uid_fetch(uid.0.to_string(), "(BODY.PEEK[])")
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(stream);

        while let Some(fetch) = stream.try_next().await.map_err(transport_err)? {
            if let Some(body) = fetch.body() {
                return Ok(body.to_vec());
            }
        }
        Ok(Vec::new())
    }

    async fn fetch_flags(&mut self, uids: &[Uid]) -> Result<Vec<(Uid, Flags)>, TransportError> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let stream = self
            .session
            .uid_fetch(uid_set(uids), "(UID FLAGS)")
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(stream);

        let mut flags = Vec::new();
        while let Some(fetch) = stream.try_next().await.map_err(transport_err)? {
            if let Some(uid) = fetch.uid {
                flags.push((Uid(uid), flags_from_fetch(&fetch)));
            }
        }
        Ok(flags)
    }

    async fn search(&mut self, query: &str) -> Result<Vec<Uid>, TransportError> {
        let uids = self
            .session
            .uid_search(query)
            .await
            .map_err(transport_err)?;
        Ok(uids.into_iter().map(Uid).collect())
    }

    async fn store_flags(&mut self, uids: &[Uid], flags: FlagOp) -> Result<(), TransportError> {
        if uids.is_empty() {
            return Ok(());
        }
        let op = match flags {
            FlagOp::Add(f) => format!("+FLAGS.SILENT ({})", flags_to_imap(f)),
            FlagOp::Remove(f) => format!("-FLAGS.SILENT ({})", flags_to_imap(f)),
            FlagOp::Replace(f) => format!("FLAGS.SILENT ({})", flags_to_imap(f)),
        };
        let stream = self
            .session
            .uid_store(uid_set(uids), op)
            .await
            .map_err(transport_err)?;
        futures::pin_mut!(stream);
        while stream.try_next().await.map_err(transport_err)?.is_some() {}
        Ok(())
    }

    async fn move_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), TransportError> {
        if uids.is_empty() {
            return Ok(());
        }
        let set = uid_set(uids);
        let dest = encode_modified_utf7(dest);
        self.ensure_capabilities().await?;
        if self.has_capability("MOVE") {
            self.session
                .uid_mv(&set, &dest)
                .await
                .map_err(transport_err)?;
        } else {
            // Fall back to COPY + mark deleted + EXPUNGE for servers without MOVE.
            self.session
                .uid_copy(&set, &dest)
                .await
                .map_err(transport_err)?;
            {
                let store = self
                    .session
                    .uid_store(&set, "+FLAGS.SILENT (\\Deleted)")
                    .await
                    .map_err(transport_err)?;
                futures::pin_mut!(store);
                while store.try_next().await.map_err(transport_err)?.is_some() {}
            }
            // Only a plain EXPUNGE removes every \Deleted message in the
            // mailbox; with UIDPLUS, scope it to the UIDs we actually moved.
            match expunge_set(self.has_capability("UIDPLUS"), uids) {
                Some(set) => {
                    let expunge = self
                        .session
                        .uid_expunge(&set)
                        .await
                        .map_err(transport_err)?;
                    futures::pin_mut!(expunge);
                    while expunge.try_next().await.map_err(transport_err)?.is_some() {}
                }
                None => {
                    let expunge = self.session.expunge().await.map_err(transport_err)?;
                    futures::pin_mut!(expunge);
                    while expunge.try_next().await.map_err(transport_err)?.is_some() {}
                }
            }
        }
        Ok(())
    }

    async fn archive(
        &mut self,
        uids: &[Uid],
        _folder: &RemoteFolder,
    ) -> Result<(), TransportError> {
        // Generic destination; Task 14 adds the Gmail-specific rules.
        self.move_messages(uids, "Archive").await
    }

    async fn trash(&mut self, uids: &[Uid], _folder: &RemoteFolder) -> Result<(), TransportError> {
        // Generic destination; Task 14 adds the Gmail-specific rules.
        self.move_messages(uids, "Trash").await
    }

    async fn append(
        &mut self,
        folder: &str,
        message: &[u8],
        flags: Flags,
    ) -> Result<(), TransportError> {
        let flag_string = flags_to_imap(flags);
        let flag_argument = (!flag_string.is_empty()).then(|| format!("({flag_string})"));
        self.session
            .append(
                encode_modified_utf7(folder),
                flag_argument.as_deref(),
                None,
                message,
            )
            .await
            .map_err(transport_err)?;
        Ok(())
    }
}

/// Map an async-imap error (or any displayable error) into a [`TransportError`].
fn transport_err<E: std::fmt::Display>(e: E) -> TransportError {
    TransportError(e.to_string())
}

/// Format a slice of UIDs as a comma-separated IMAP sequence set.
fn uid_set(uids: &[Uid]) -> String {
    uids.iter()
        .map(|uid| uid.0.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Choose the EXPUNGE to use after a copy-then-delete fallback move.
///
/// With UIDPLUS the expunge is scoped to exactly the moved UIDs (returned as a
/// sequence set); without it we must fall back to a plain `EXPUNGE`, which
/// removes every `\Deleted` message in the mailbox.
fn expunge_set(uidplus: bool, uids: &[Uid]) -> Option<String> {
    if uidplus { Some(uid_set(uids)) } else { None }
}

/// Map async-imap [`Flag`]s into the packed [`Flags`] domain type, ignoring
/// `\Recent` and custom keywords exactly like `mail::parse_flags`.
fn flags_from_fetch(fetch: &Fetch) -> Flags {
    let mut flags = Flags::default();
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => flags.seen = true,
            Flag::Answered => flags.answered = true,
            Flag::Flagged => flags.flagged = true,
            Flag::Deleted => flags.deleted = true,
            Flag::Draft => flags.draft = true,
            _ => {}
        }
    }
    flags
}

/// Render [`Flags`] as an IMAP flag-list body (space-separated, unparenthesized).
fn flags_to_imap(flags: Flags) -> String {
    let mut parts = Vec::new();
    if flags.seen {
        parts.push("\\Seen");
    }
    if flags.answered {
        parts.push("\\Answered");
    }
    if flags.flagged {
        parts.push("\\Flagged");
    }
    if flags.deleted {
        parts.push("\\Deleted");
    }
    if flags.draft {
        parts.push("\\Draft");
    }
    parts.join(" ")
}

/// Render a LIST [`NameAttribute`] back to its raw attribute string so it can
/// be fed to [`classify_folder`]. Unknown/extension attributes are returned
/// verbatim (including the leading backslash).
fn attribute_str<'a>(attribute: &'a NameAttribute<'_>) -> &'a str {
    match attribute {
        NameAttribute::NoInferiors => "\\NoInferiors",
        NameAttribute::NoSelect => "\\NoSelect",
        NameAttribute::Marked => "\\Marked",
        NameAttribute::Unmarked => "\\Unmarked",
        NameAttribute::All => "\\All",
        NameAttribute::Archive => "\\Archive",
        NameAttribute::Drafts => "\\Drafts",
        NameAttribute::Flagged => "\\Flagged",
        NameAttribute::Junk => "\\Junk",
        NameAttribute::Sent => "\\Sent",
        NameAttribute::Trash => "\\Trash",
        NameAttribute::Extension(s) => s.as_ref(),
        _ => "",
    }
}

/// Decode a mailbox name from RFC 3501/6855 modified UTF-7.
///
/// Printable ASCII passes through unchanged; `&-` is a literal `&`; each
/// `&...-` run is modified BASE64 (`,` for `/`) encoding UTF-16BE code units.
/// Invalid sequences are kept verbatim rather than corrupting the name.
fn decode_modified_utf7(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match after.find('-') {
            Some(0) => {
                out.push('&');
                rest = &after[1..];
            }
            Some(end) => {
                let encoded = &after[..end];
                match decode_modified_base64(encoded) {
                    Some(decoded) => out.push_str(&decoded),
                    None => {
                        out.push('&');
                        out.push_str(encoded);
                        out.push('-');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Encode a mailbox name into RFC 3501/6855 modified UTF-7 for the wire.
fn encode_modified_utf7(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut run: Vec<u16> = Vec::new();
    for ch in input.chars() {
        if ch == '&' {
            flush_modified_base64(&mut run, &mut out);
            out.push_str("&-");
        } else if matches!(ch, ' '..='~') {
            flush_modified_base64(&mut run, &mut out);
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    flush_modified_base64(&mut run, &mut out);
    out
}

/// Decode one modified BASE64 run into a UTF-8 string.
fn decode_modified_base64(encoded: &str) -> Option<String> {
    let normalized = encoded.replace(',', "/");
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normalized.as_bytes())
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = (0..bytes.len())
        .step_by(2)
        .map(|i| u16::from_be_bytes([bytes[i], bytes[i + 1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Emit any pending UTF-16 units as a modified BASE64 `&...-` run.
fn flush_modified_base64(run: &mut Vec<u16>, out: &mut String) {
    if run.is_empty() {
        return;
    }
    let bytes: Vec<u8> = run.iter().flat_map(|unit| unit.to_be_bytes()).collect();
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes);
    out.push('&');
    out.push_str(&encoded.replace('/', ","));
    out.push('-');
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_modified_utf7_decodes_german_and_passes_ascii_through() {
        assert_eq!(decode_modified_utf7("Gel&APY-scht"), "Gelöscht");
        assert_eq!(decode_modified_utf7("INBOX"), "INBOX");
        assert_eq!(decode_modified_utf7("A&-B"), "A&B");
    }

    #[test]
    fn modified_utf7_round_trips() {
        for name in ["Gelöscht", "Entwürfe", "🗑️ Trash", "INBOX", "A&B/Ordner"] {
            assert_eq!(decode_modified_utf7(&encode_modified_utf7(name)), name);
        }
    }

    #[test]
    fn expunge_is_scoped_to_moved_uids_only_with_uidplus() {
        assert_eq!(
            expunge_set(true, &[Uid(3), Uid(7)]),
            Some("3,7".to_string())
        );
        assert_eq!(expunge_set(false, &[Uid(3), Uid(7)]), None);
    }
}
