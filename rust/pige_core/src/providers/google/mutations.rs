//! Gmail semantic mutations.
//!
//! Gmail labels are memberships: archive removes only `INBOX`, and removing one
//! label never deletes the message. Trash moves the message to Gmail's Trash
//! (recoverable). Permanent delete is available only in a separately approved
//! full-mail grant; under the v1 `gmail.modify` scope the capability is false
//! and the action is rejected instead of being presented as a destructive
//! action that cannot work.

use async_trait::async_trait;

use crate::providers::{
    ErrorKind, MutationAction, MutationCapabilities, MutationItemResult, MutationOutcome,
    MutationRequest, MutationResult, MutationService, OperationContext, ProviderError,
    RemoteMessageId,
};

use super::client::GmailApi;
use super::normalize;

/// Gmail mutation service.
pub struct GmailMutations {
    api: GmailApi,
    permanent_delete: bool,
}

impl GmailMutations {
    /// v1 behavior: `gmail.modify` only, so permanent delete is unavailable.
    pub fn new(client: super::GoogleClient, user: impl Into<String>) -> Self {
        Self {
            api: GmailApi::new(client, user),
            permanent_delete: false,
        }
    }

    /// Construct with an explicitly approved full-mail grant. No v1 shell
    /// enables this; it exists so a future grant is a deliberate wiring change.
    pub fn with_permanent_delete(
        client: super::GoogleClient,
        user: impl Into<String>,
        permanent_delete: bool,
    ) -> Self {
        Self {
            api: GmailApi::new(client, user),
            permanent_delete,
        }
    }

    pub fn capabilities(&self) -> MutationCapabilities {
        MutationCapabilities {
            read: true,
            star: true,
            labels: true,
            move_messages: true,
            archive: true,
            trash: true,
            permanent_delete: self.permanent_delete,
        }
    }

    async fn attempt(
        &self,
        target: &RemoteMessageId,
        request: &MutationRequest,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        match &request.action {
            MutationAction::SetRead(read) => {
                self.modify(target, &[], &[normalize::UNREAD_LABEL], !read, ctx)
                    .await
            }
            MutationAction::SetStar(star) => {
                // `SetStar(true)` adds STARRED; `false` removes it. Turning
                // this around silently inverts every star action.
                self.modify(target, &[normalize::STARRED_LABEL], &[], !star, ctx)
                    .await
            }
            MutationAction::AddLabel(label) => {
                self.modify(target, &[label.as_str()], &[], false, ctx)
                    .await
            }
            MutationAction::RemoveLabel(label) => {
                self.modify(target, &[], &[label.as_str()], false, ctx)
                    .await
            }
            MutationAction::Move(destination) => {
                let source = request.source.as_ref().map(|source| source.as_str());
                // All Mail is implicit membership; moving out of it only adds
                // the destination label. A move with no source is Gmail's
                // inbox move, which also removes INBOX.
                let remove: &[&str] = match source {
                    Some(source) if source == normalize::ALL_MAIL_COLLECTION => &[],
                    Some(source) if source == destination.as_str() => return Ok(()),
                    Some(source) => &[source],
                    None => &[normalize::INBOX_LABEL],
                };
                self.modify(target, &[destination.as_str()], remove, false, ctx)
                    .await
            }
            MutationAction::Archive => {
                // Archive is exactly "remove INBOX"; unrelated labels stay.
                self.modify(target, &[], &[normalize::INBOX_LABEL], false, ctx)
                    .await
            }
            MutationAction::Trash => {
                // Gmail's trash endpoint is recoverable; it is not DELETE.
                self.api.trash(target.as_str(), ctx).await
            }
            MutationAction::DeletePermanently => {
                if !self.permanent_delete {
                    return Err(ProviderError::unsupported(
                        "permanently deleting a Gmail message under the gmail.modify scope",
                    ));
                }
                self.api.delete(target.as_str(), ctx).await
            }
        }
    }

    /// Apply one addition/removal pair. `invert` flips the pair so a `SetRead`
    /// false or `SetStar` false call can share the same code path.
    async fn modify(
        &self,
        target: &RemoteMessageId,
        add: &[&str],
        remove: &[&str],
        invert: bool,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        let (add, remove) = if invert { (remove, add) } else { (add, remove) };
        let add: Vec<String> = add.iter().map(|label| (*label).to_string()).collect();
        let remove: Vec<String> = remove.iter().map(|label| (*label).to_string()).collect();
        self.api.modify(target.as_str(), &add, &remove, ctx).await
    }
}

#[async_trait]
impl MutationService for GmailMutations {
    async fn apply(
        &mut self,
        request: MutationRequest,
        ctx: &OperationContext,
    ) -> Result<MutationResult, ProviderError> {
        ctx.ensure_active()?;
        let mut items = Vec::with_capacity(request.targets.len());
        for target in &request.targets {
            ctx.ensure_active()?;
            let outcome = match self.attempt(target, &request, ctx).await {
                Ok(()) => MutationOutcome::Applied,
                Err(error) if is_ambiguous_write(&error) => MutationOutcome::Unknown,
                Err(error) => MutationOutcome::Rejected(error),
            };
            items.push(MutationItemResult {
                message: target.clone(),
                outcome,
            });
        }
        Ok(MutationResult { items })
    }
}

/// A transport failure after a write was sent may already have reached Gmail.
/// A typed server rejection (including a rate limit) is a definite rejection.
fn is_ambiguous_write(error: &ProviderError) -> bool {
    matches!(
        error.kind,
        ErrorKind::Offline | ErrorKind::Transient | ErrorKind::UnknownWriteOutcome
    )
}
