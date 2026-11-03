//! Real IMAP transport over rustls TLS, implementing [`MailTransport`].

use async_imap::Session;
use async_imap::types::{Fetch, Flag, NameAttribute};
use async_trait::async_trait;
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
    Ok(ImapTransport { session })
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
        let capabilities = self.session.capabilities().await.map_err(transport_err)?;
        Ok(Capabilities {
            gmail: capabilities.has_str("X-GM-EXT-1"),
            move_ext: capabilities.has_str("MOVE"),
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
            let attributes: Vec<&str> = name.attributes().iter().map(attribute_str).collect();
            let kind = classify_folder(name.name(), &attributes);
            folders.push(RemoteFolder {
                name: name.name().to_string(),
                delimiter: name.delimiter().map(str::to_string),
                kind,
            });
        }
        Ok(folders)
    }

    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState, TransportError> {
        let mailbox = self
            .session
            .select(&folder.name)
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
            let uid = fetch
                .uid
                .map(Uid)
                .ok_or_else(|| TransportError("FETCH response missing UID".to_string()))?;
            let header_bytes = fetch.header().ok_or_else(|| {
                TransportError(format!("FETCH response for UID {uid} missing header"))
            })?;
            let mut header = crate::mail::parse_header_block(header_bytes)
                .map_err(|e| TransportError(format!("header parse for UID {uid}: {e}")))?;
            let size = fetch.size.unwrap_or(0);
            header.size = size;
            messages.push(RemoteMessage {
                uid,
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
        if self.capabilities().await?.move_ext {
            self.session
                .uid_mv(&set, dest)
                .await
                .map_err(transport_err)?;
        } else {
            // Fall back to COPY + mark deleted + EXPUNGE for servers without MOVE.
            self.session
                .uid_copy(&set, dest)
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
            let expunge = self.session.expunge().await.map_err(transport_err)?;
            futures::pin_mut!(expunge);
            while expunge.try_next().await.map_err(transport_err)?.is_some() {}
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
            .append(folder, flag_argument.as_deref(), None, message)
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
