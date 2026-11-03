//! Gmail-style conversation threading.
//!
//! Messages join a conversation through their `References`/`In-Reply-To`
//! chain. When that chain is broken (the referenced message was never synced or
//! has no id), a normalized subject plus an overlapping participant within a
//! 30-day window provides the fallback. Thread ids are scoped to an account, so
//! the same `Message-ID` arriving in two accounts never merges them.

use std::collections::{HashMap, HashSet};

use crate::account::AccountId;

use super::{Address, MessageHeader, StoredMessage, ThreadId, normalized_subject};

/// Two messages with the same subject and participants farther apart than this
/// are treated as unrelated conversations unless their references connect them.
pub const SUBJECT_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// Everything the assigner remembers about one thread.
#[derive(Debug, Clone)]
struct ThreadRecord {
    account: AccountId,
    participants: HashSet<String>,
    min_date: i64,
    max_date: i64,
}

impl ThreadRecord {
    fn new(account: AccountId, date: i64, participants: &[Address]) -> Self {
        let mut record = Self {
            account,
            participants: HashSet::new(),
            min_date: date,
            max_date: date,
        };
        record.add_participants(participants);
        record
    }

    fn add_participants(&mut self, participants: &[Address]) {
        self.participants
            .extend(participants.iter().map(participant_key));
    }

    fn observe(&mut self, date: i64, participants: &[Address]) {
        self.min_date = self.min_date.min(date);
        self.max_date = self.max_date.max(date);
        self.add_participants(participants);
    }

    fn overlaps(&self, participants: &[Address]) -> bool {
        participants
            .iter()
            .map(participant_key)
            .any(|key| !key.is_empty() && self.participants.contains(&key))
    }

    fn within_window(&self, date: i64) -> bool {
        date >= self.min_date.saturating_sub(SUBJECT_WINDOW_SECS)
            && date <= self.max_date.saturating_add(SUBJECT_WINDOW_SECS)
    }
}

type MessageKey = (String, String);
type SubjectKey = (String, String);

/// In-memory index used while inserting messages during a sync.
///
/// The index is seeded from already-cached headers once per account sync, then
/// updated as new messages are assigned. It never reads message bodies.
#[derive(Debug, Default)]
pub struct ThreadAssigner {
    /// `(account, Message-ID)` -> conversation.
    message_threads: HashMap<MessageKey, ThreadId>,
    /// `(account, normalized subject)` -> conversations with that subject.
    subject_threads: HashMap<SubjectKey, Vec<ThreadId>>,
    /// Conversation metadata keyed by the surviving id.
    threads: HashMap<ThreadId, ThreadRecord>,
    /// DB merges the caller must apply, oldest first.
    pending_merges: Vec<(ThreadId, ThreadId)>,
}

impl ThreadAssigner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the index from cached headers. Bodies are not read.
    pub fn from_messages(messages: &[StoredMessage]) -> Self {
        let mut assigner = Self::new();
        for message in messages {
            assigner.observe_existing(message);
        }
        assigner
    }

    /// Add an existing row to the index, preserving its stored thread id.
    pub fn observe_existing(&mut self, message: &StoredMessage) {
        let subject = normalized_subject(&message.header.subject);
        let participants = participants_of(&message.header);
        let account = message.account_id.0.clone();

        self.threads
            .entry(message.thread_id.clone())
            .and_modify(|record| record.observe(message.header.date, &participants))
            .or_insert_with(|| {
                ThreadRecord::new(
                    message.account_id.clone(),
                    message.header.date,
                    &participants,
                )
            });

        let key = (account.clone(), subject);
        let threads = self.subject_threads.entry(key).or_default();
        if !threads.contains(&message.thread_id) {
            threads.push(message.thread_id.clone());
        }

        if let Some(message_id) = &message.header.message_id {
            self.message_threads
                .insert((account, message_id.clone()), message.thread_id.clone());
        }
    }

    /// Assign a conversation to one message.
    ///
    /// Order: an already-assigned copy of this `Message-ID`, then any thread
    /// reached through the references chain (merging several when they
    /// disagree), then a subject/participant match within 30 days, then a fresh
    /// uuid.
    pub fn assign(
        &mut self,
        account: &AccountId,
        header: &MessageHeader,
        date: i64,
        participants: &[Address],
    ) -> ThreadId {
        let account_key = account.0.clone();
        let subject = normalized_subject(&header.subject);

        // A retry of the same message must keep its original conversation.
        if let Some(message_id) = &header.message_id
            && let Some(thread) = self
                .message_threads
                .get(&(account_key.clone(), message_id.clone()))
                .cloned()
        {
            if let Some(record) = self.threads.get_mut(&thread) {
                record.observe(date, participants);
            }
            return thread;
        }

        let mut referenced: Vec<ThreadId> = Vec::new();
        for message_id in header.references.iter().chain(header.in_reply_to.iter()) {
            if let Some(thread) = self
                .message_threads
                .get(&(account_key.clone(), message_id.clone()))
                .cloned()
                && !referenced.contains(&thread)
            {
                referenced.push(thread);
            }
        }

        let thread = if let Some(survivor) = referenced.first().cloned() {
            for loser in &referenced[1..] {
                self.merge(loser, &survivor);
            }
            self.remember(&survivor, account, date, participants);
            survivor
        } else if let Some(existing) =
            self.subject_match(&account_key, &subject, date, participants)
        {
            self.remember(&existing, account, date, participants);
            existing
        } else {
            let thread = ThreadId::new();
            self.threads.insert(
                thread.clone(),
                ThreadRecord::new(account.clone(), date, participants),
            );
            self.subject_threads
                .entry((account_key.clone(), subject))
                .or_default()
                .push(thread.clone());
            thread
        };

        if let Some(message_id) = &header.message_id {
            self.message_threads
                .insert((account_key, message_id.clone()), thread.clone());
        }
        thread
    }

    /// Merge `from` into `into`, updating every in-memory mapping. The caller
    /// applies the returned pair to the store through [`Self::take_merges`].
    pub fn merge(&mut self, from: &ThreadId, into: &ThreadId) {
        if from == into {
            return;
        }
        let Some(loser) = self.threads.remove(from) else {
            return;
        };

        if let Some(survivor) = self.threads.get_mut(into) {
            survivor.participants.extend(loser.participants);
            survivor.min_date = survivor.min_date.min(loser.min_date);
            survivor.max_date = survivor.max_date.max(loser.max_date);
        } else {
            self.threads.insert(into.clone(), loser.clone());
        }

        for threads in self.subject_threads.values_mut() {
            let had_from = threads.iter().any(|thread| thread == from);
            threads.retain(|thread| thread != from);
            if had_from && !threads.contains(into) {
                threads.push(into.clone());
            }
        }

        for thread in self.message_threads.values_mut() {
            if thread == from {
                *thread = into.clone();
            }
        }

        self.pending_merges.push((from.clone(), into.clone()));
    }

    /// Remove and return merges produced by [`Self::assign`].
    pub fn take_merges(&mut self) -> Vec<(ThreadId, ThreadId)> {
        std::mem::take(&mut self.pending_merges)
    }

    fn remember(
        &mut self,
        thread: &ThreadId,
        account: &AccountId,
        date: i64,
        participants: &[Address],
    ) {
        if let Some(record) = self.threads.get_mut(thread) {
            record.observe(date, participants);
            return;
        }
        self.threads.insert(
            thread.clone(),
            ThreadRecord::new(account.clone(), date, participants),
        );
    }

    fn subject_match(
        &self,
        account: &str,
        subject: &str,
        date: i64,
        participants: &[Address],
    ) -> Option<ThreadId> {
        let threads = self
            .subject_threads
            .get(&(account.to_string(), subject.to_string()))?;
        threads.iter().find_map(|thread| {
            let record = self.threads.get(thread)?;
            (record.account.0 == account
                && record.overlaps(participants)
                && record.within_window(date))
            .then(|| thread.clone())
        })
    }
}

/// Every address that identifies the people in a message, used for the
/// subject/participant fallback.
pub fn participants_of(header: &MessageHeader) -> Vec<Address> {
    header
        .from
        .iter()
        .chain(header.to.iter())
        .chain(header.cc.iter())
        .cloned()
        .collect()
}

fn participant_key(address: &Address) -> String {
    address.email.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_window_is_inclusive() {
        let record = ThreadRecord::new(
            AccountId("account".to_string()),
            1_000,
            &[Address::new(None, "alice@example.com")],
        );

        assert!(record.within_window(1_000 + SUBJECT_WINDOW_SECS));
        assert!(!record.within_window(1_001 + SUBJECT_WINDOW_SECS));
    }
}
