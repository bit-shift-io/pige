//! A scripted, in-memory [`MailTransport`] for sync and mutation tests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::mail::{Flags, Uid};

use super::{
    Capabilities, FlagOp, FolderState, MailTransport, RemoteFolder, RemoteMessage, TransportError,
    UidRange,
};

/// Transport whose behavior is fully driven by its public fields. Messages and
/// folder state are keyed by folder name; calls are recorded so tests can
/// assert exactly what the sync engine asked for.
#[derive(Debug, Clone, Default)]
pub struct FakeTransport {
    pub folders: Vec<RemoteFolder>,
    /// Folder name -> scripted messages (any order; filtered by UID on fetch).
    pub messages: HashMap<String, Vec<RemoteMessage>>,
    /// Folder name -> state reported by `select`.
    pub states: HashMap<String, FolderState>,
    /// Folder name -> UID -> snippet text returned by `fetch_texts`.
    pub texts: HashMap<String, HashMap<Uid, String>>,
    /// Folder name -> UID -> raw body bytes returned by `fetch_body`.
    pub bodies: HashMap<String, HashMap<Uid, Vec<u8>>>,
    pub capabilities: Capabilities,
    /// Folder names whose `select` returns an error.
    pub fail_select: HashSet<String>,
    /// Folder names whose `fetch_texts` returns an error (to simulate a retry).
    pub fail_fetch_texts: HashSet<String>,
    /// Folder names whose `append` returns an error.
    pub fail_append: HashSet<String>,

    pub select_calls: Vec<String>,
    pub fetch_headers_calls: Vec<(String, UidRange)>,
    pub fetch_flags_calls: Vec<(String, Vec<Uid>)>,
    pub fetch_texts_calls: Vec<(String, Vec<Uid>, usize)>,
    pub fetch_body_calls: Vec<(String, Uid)>,
    pub append_calls: Vec<(String, Vec<u8>, Flags)>,

    /// Test hooks for deterministic backfill race tests: `started` receives a
    /// permit when a header fetch begins, and the fetch waits for a permit on
    /// `pause` before returning.
    pub fetch_headers_started: Option<Arc<Semaphore>>,
    pub fetch_headers_pause: Option<Arc<Semaphore>>,

    /// Internal: the folder most recently selected, if any.
    pub current: Option<String>,
}

impl FakeTransport {
    /// Reset call recording without touching the scripted folders/messages.
    pub fn clear_calls(&mut self) {
        self.select_calls.clear();
        self.fetch_headers_calls.clear();
        self.fetch_flags_calls.clear();
        self.fetch_texts_calls.clear();
        self.fetch_body_calls.clear();
        self.append_calls.clear();
    }

    fn current_folder(&self) -> Result<String, TransportError> {
        self.current
            .clone()
            .ok_or_else(|| TransportError("no folder selected".to_string()))
    }
}

#[async_trait]
impl MailTransport for FakeTransport {
    async fn capabilities(&mut self) -> Result<Capabilities, TransportError> {
        Ok(self.capabilities)
    }

    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>, TransportError> {
        Ok(self.folders.clone())
    }

    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState, TransportError> {
        self.select_calls.push(folder.name.clone());
        if self.fail_select.contains(&folder.name) {
            return Err(TransportError(format!("select failed for {}", folder.name)));
        }
        self.current = Some(folder.name.clone());
        Ok(self
            .states
            .get(&folder.name)
            .copied()
            .unwrap_or(FolderState {
                uidvalidity: 1,
                uidnext: 1,
                exists: 0,
            }))
    }

    async fn fetch_headers(
        &mut self,
        range: UidRange,
    ) -> Result<Vec<RemoteMessage>, TransportError> {
        let folder = self.current_folder()?;
        self.fetch_headers_calls.push((folder.clone(), range));
        if let Some(started) = &self.fetch_headers_started {
            started.add_permits(1);
        }
        if let Some(pause) = &self.fetch_headers_pause {
            let _ = pause.acquire().await;
        }
        Ok(self
            .messages
            .get(&folder)
            .map(|messages| {
                messages
                    .iter()
                    .filter(|m| m.uid >= range.from && m.uid <= range.to)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn fetch_texts(
        &mut self,
        uids: &[Uid],
        max_bytes: usize,
    ) -> Result<Vec<(Uid, String)>, TransportError> {
        let folder = self.current_folder()?;
        self.fetch_texts_calls
            .push((folder.clone(), uids.to_vec(), max_bytes));
        if self.fail_fetch_texts.contains(&folder) {
            return Err(TransportError(format!("fetch_texts failed for {folder}")));
        }
        let texts = self.texts.get(&folder);
        Ok(uids
            .iter()
            .map(|uid| {
                let text = texts
                    .and_then(|map| map.get(uid))
                    .cloned()
                    .unwrap_or_default();
                (*uid, truncate(&text, max_bytes))
            })
            .collect())
    }

    async fn fetch_body(&mut self, uid: Uid) -> Result<Vec<u8>, TransportError> {
        let folder = self.current_folder()?;
        self.fetch_body_calls.push((folder.clone(), uid));
        Ok(self
            .bodies
            .get(&folder)
            .and_then(|map| map.get(&uid))
            .cloned()
            .unwrap_or_default())
    }

    async fn fetch_flags(&mut self, uids: &[Uid]) -> Result<Vec<(Uid, Flags)>, TransportError> {
        let folder = self.current_folder()?;
        self.fetch_flags_calls.push((folder.clone(), uids.to_vec()));
        let mut flags: HashMap<Uid, Flags> = self
            .messages
            .get(&folder)
            .map(|messages| messages.iter().map(|m| (m.uid, m.flags)).collect())
            .unwrap_or_default();
        Ok(uids
            .iter()
            .filter_map(|uid| flags.remove(uid).map(|flags| (*uid, flags)))
            .collect())
    }

    async fn search(&mut self, _query: &str) -> Result<Vec<Uid>, TransportError> {
        Ok(Vec::new())
    }

    async fn store_flags(&mut self, _uids: &[Uid], _flags: FlagOp) -> Result<(), TransportError> {
        Ok(())
    }

    async fn move_messages(&mut self, _uids: &[Uid], _dest: &str) -> Result<(), TransportError> {
        Ok(())
    }

    async fn archive(
        &mut self,
        _uids: &[Uid],
        _folder: &RemoteFolder,
    ) -> Result<(), TransportError> {
        Ok(())
    }

    async fn trash(&mut self, _uids: &[Uid], _folder: &RemoteFolder) -> Result<(), TransportError> {
        Ok(())
    }

    async fn append(
        &mut self,
        folder: &str,
        message: &[u8],
        flags: Flags,
    ) -> Result<(), TransportError> {
        self.append_calls
            .push((folder.to_string(), message.to_vec(), flags));
        if self.fail_append.contains(folder) {
            return Err(TransportError(format!("append failed for {folder}")));
        }
        Ok(())
    }
}

/// Truncate `text` to at most `max_bytes` bytes on a UTF-8 character boundary.
fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
