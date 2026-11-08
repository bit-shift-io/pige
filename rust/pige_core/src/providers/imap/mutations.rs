//! Semantic IMAP mutations behind the provider `MutationService` trait.
//!
//! A mutation names its targets by explicit [`RemoteMessageId`]s (each encodes
//! its `(mailbox, UIDVALIDITY, UID)`), so the adapter never depends on a mutable
//! "currently selected folder" chosen elsewhere. Gmail's label model is
//! preserved *inside this adapter* — where `Capabilities.gmail` was previously
//! application policy:
//!
//! - `Archive` on Gmail removes the source label (a scoped delete), not a
//!   message-wide expunge.
//! - `Move` on Gmail is COPY-to-label + delete-source-label; from All Mail it is
//!   COPY only, because All Mail membership is implicit.
//! - `Trash` on Gmail copies to the Trash label then removes the source label.
//! - Generic servers use `MOVE` where advertised and COPY + delete otherwise.
//!
//! A multi-step write that fails *after* its first step has changed the server
//! is reported as [`MutationOutcome::Unknown`]: never retry it blindly. An
//! `Archive`/`Move` that only removed a label is distinct from a
//! `DeletePermanently`, which expunges.

use async_trait::async_trait;

use crate::mail::{Flags, FolderKind, Uid};
use crate::providers::error::{ErrorKind, ProviderError};
use crate::providers::traits::MutationService;
use crate::providers::types::{
    MutationAction, MutationItemResult, MutationOutcome, MutationRequest, MutationResult,
    OperationContext, RemoteMessageId,
};
use crate::transport::{Capabilities, FlagOp, FolderState, MailTransport, RemoteFolder};

use super::{identity, transport_error};

/// A [`MutationService`] over a live IMAP transport.
pub struct ImapMutations {
    transport: Box<dyn MailTransport>,
    capabilities: Capabilities,
}

impl ImapMutations {
    pub fn new(transport: Box<dyn MailTransport>, capabilities: Capabilities) -> Self {
        Self {
            transport,
            capabilities,
        }
    }
}

#[async_trait]
impl MutationService for ImapMutations {
    async fn apply(
        &mut self,
        request: MutationRequest,
        ctx: &OperationContext,
    ) -> Result<MutationResult, ProviderError> {
        ctx.ensure_active()?;

        // Group the targets by the mailbox that names their identity: one
        // SELECT then one transport call per mailbox. `request.source` is
        // advisory: the explicit remote id already names the message's mailbox,
        // so it is the authoritative folder to act on.
        let mut groups: Vec<(String, Vec<(Uid, RemoteMessageId)>)> = Vec::new();
        let mut items = Vec::new();
        for target in &request.targets {
            match identity::decode_message(target) {
                Some(coords) => match groups
                    .iter_mut()
                    .find(|(mailbox, _)| mailbox == &coords.mailbox)
                {
                    Some((_, entries)) => entries.push((coords.uid, target.clone())),
                    None => groups.push((coords.mailbox, vec![(coords.uid, target.clone())])),
                },
                None => items.push(MutationItemResult {
                    message: target.clone(),
                    outcome: MutationOutcome::Rejected(ProviderError::unsupported(
                        "this message identity",
                    )),
                }),
            }
        }

        for (mailbox, entries) in &groups {
            ctx.ensure_active()?;
            let uids: Vec<Uid> = entries.iter().map(|(uid, _)| *uid).collect();
            let outcome = self.apply_group(mailbox, &uids, &request.action).await;
            for (_, message) in entries {
                items.push(MutationItemResult {
                    message: message.clone(),
                    outcome: outcome.clone(),
                });
            }
        }

        Ok(MutationResult { items })
    }
}

impl ImapMutations {
    /// Apply `action` to `uids`, all currently named by `mailbox`.
    async fn apply_group(
        &mut self,
        mailbox: &str,
        uids: &[Uid],
        action: &MutationAction,
    ) -> MutationOutcome {
        match self.attempt(mailbox, uids, action).await {
            Ok(()) => MutationOutcome::Applied,
            Err(error) if error.kind == ErrorKind::UnknownWriteOutcome => MutationOutcome::Unknown,
            Err(error) => MutationOutcome::Rejected(error),
        }
    }

    async fn attempt(
        &mut self,
        mailbox: &str,
        uids: &[Uid],
        action: &MutationAction,
    ) -> Result<(), ProviderError> {
        match action {
            MutationAction::SetRead(read) => {
                let flags = Flags {
                    seen: true,
                    ..Flags::default()
                };
                self.select(mailbox).await?;
                self.store(uids, flag_op(*read, flags)).await
            }
            MutationAction::SetStar(star) => {
                let flags = Flags {
                    flagged: true,
                    ..Flags::default()
                };
                self.select(mailbox).await?;
                self.store(uids, flag_op(*star, flags)).await
            }
            MutationAction::AddLabel(label) => {
                // Adding a label is a COPY into the label's folder: the source
                // membership is untouched.
                self.select(mailbox).await?;
                self.copy(uids, label.as_str()).await
            }
            MutationAction::RemoveLabel(label) => {
                // Only Gmail folds labels into folders. Removing a label is a
                // delete *scoped to that label*; on a plain IMAP server a
                // folder is the message's only home, so removing it would
                // delete the message — genuinely unsupported, never a silent
                // permanent delete.
                if !self.capabilities.gmail {
                    return Err(ProviderError::unsupported(
                        "removing a label on this mail server",
                    ));
                }
                self.select(label.as_str()).await?;
                self.remove(uids).await
            }
            MutationAction::Move(dest) => self.move_to(mailbox, uids, dest.as_str()).await,
            MutationAction::Archive => self.archive(mailbox, uids).await,
            MutationAction::Trash => self.trash(mailbox, uids).await,
            MutationAction::DeletePermanently => {
                self.select(mailbox).await?;
                self.remove(uids).await
            }
        }
    }

    /// Gmail: COPY to the destination label, then delete the source label —
    /// except from All Mail, whose membership must never be deleted.
    /// Generic with `MOVE`: one server-side move. Generic otherwise: COPY, then
    /// delete the source.
    async fn move_to(
        &mut self,
        mailbox: &str,
        uids: &[Uid],
        dest: &str,
    ) -> Result<(), ProviderError> {
        self.select(mailbox).await?;
        if self.capabilities.gmail {
            self.copy(uids, dest).await?;
            // All Mail (the `Archive`-kind folder) is implicit membership:
            // deleting it would remove the message from every label.
            let all_mail =
                self.special_folder(FolderKind::Archive).await?.as_deref() == Some(mailbox);
            if all_mail {
                Ok(())
            } else {
                self.remove(uids).await
            }
        } else if self.capabilities.move_ext {
            self.move_messages(uids, dest).await
        } else {
            self.copy(uids, dest).await?;
            self.remove(uids).await
        }
    }

    /// Gmail: remove the source label only. Generic: move into the Archive
    /// folder.
    async fn archive(&mut self, mailbox: &str, uids: &[Uid]) -> Result<(), ProviderError> {
        if self.capabilities.gmail {
            self.select(mailbox).await?;
            return self.remove(uids).await;
        }
        let archive = self
            .special_folder(FolderKind::Archive)
            .await?
            .ok_or_else(|| ProviderError::unsupported("archiving without an Archive folder"))?;
        self.relocate(mailbox, uids, &archive).await
    }

    /// Gmail: copy to the Trash label then remove the source label. Generic:
    /// move into the Trash folder.
    async fn trash(&mut self, mailbox: &str, uids: &[Uid]) -> Result<(), ProviderError> {
        let trash = self
            .special_folder(FolderKind::Trash)
            .await?
            .ok_or_else(|| ProviderError::unsupported("moving to Trash without a Trash folder"))?;
        if self.capabilities.gmail {
            self.select(mailbox).await?;
            self.copy(uids, &trash).await?;
            return self.remove(uids).await;
        }
        self.relocate(mailbox, uids, &trash).await
    }

    /// A generic relocation to `dest`: `MOVE` where available, COPY + delete
    /// otherwise.
    async fn relocate(
        &mut self,
        mailbox: &str,
        uids: &[Uid],
        dest: &str,
    ) -> Result<(), ProviderError> {
        self.select(mailbox).await?;
        if self.capabilities.move_ext {
            self.move_messages(uids, dest).await
        } else {
            self.copy(uids, dest).await?;
            self.remove(uids).await
        }
    }

    /// Find a special folder by kind among the account's folders.
    async fn special_folder(&mut self, kind: FolderKind) -> Result<Option<String>, ProviderError> {
        let folders = self
            .transport
            .list_folders()
            .await
            .map_err(transport_error)?;
        Ok(folders
            .into_iter()
            .find(|folder| folder.kind == kind)
            .map(|folder| folder.name))
    }

    async fn select(&mut self, mailbox: &str) -> Result<(), ProviderError> {
        let folder = RemoteFolder {
            name: mailbox.to_string(),
            delimiter: None,
            kind: FolderKind::Other,
        };
        self.transport
            .select(&folder)
            .await
            .map(|_state: FolderState| ())
            .map_err(transport_error)
    }

    /// A flag change is idempotent, so a failure is a definite rejection.
    async fn store(&mut self, uids: &[Uid], op: FlagOp) -> Result<(), ProviderError> {
        self.transport
            .store_flags(uids, op)
            .await
            .map_err(transport_error)
    }

    /// A failed COPY added nothing to the source, so it is a definite rejection.
    async fn copy(&mut self, uids: &[Uid], dest: &str) -> Result<(), ProviderError> {
        self.transport
            .copy_messages(uids, dest)
            .await
            .map_err(transport_error)
    }

    /// A `MOVE` may have reached the server before the connection broke.
    async fn move_messages(&mut self, uids: &[Uid], dest: &str) -> Result<(), ProviderError> {
        self.transport
            .move_messages(uids, dest)
            .await
            .map_err(|error| ambiguous("the mail server did not confirm the move", error))
    }

    /// A delete/expunge may have partially applied; never retry it blindly.
    async fn remove(&mut self, uids: &[Uid]) -> Result<(), ProviderError> {
        self.transport
            .delete_messages(uids)
            .await
            .map_err(|error| ambiguous("the mail server did not confirm the delete", error))
    }
}

/// The flag operation that adds or removes `flags`.
fn flag_op(enable: bool, flags: Flags) -> FlagOp {
    if enable {
        FlagOp::Add(flags)
    } else {
        FlagOp::Remove(flags)
    }
}

/// Wrap a transport failure whose result is genuinely unknown.
fn ambiguous(message: &str, error: crate::transport::TransportError) -> ProviderError {
    ProviderError::unknown_write_outcome(message).with_source_detail(error.0)
}
