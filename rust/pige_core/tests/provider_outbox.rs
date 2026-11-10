//! Task F9: provider-independent durable submission.
//!
//! These tests exercise the durable outbox state machine against a scripted
//! [`SubmissionService`]. They prove the crash-safety invariants that matter
//! most: an accepted message never has its delivery repeated, a Sent copy is
//! retried independently, and an ambiguous outcome becomes `Uncertain` instead
//! of being blindly resent.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::legacy::LegacyV4;
use pige_core::{
    Account, AccountId, Address, AuthKind, MailBackend, Security, ServerConfig, Store,
    mail::{AttachmentDisposition, MAX_ATTACHMENT_BYTES},
    platform::{
        CredentialStore,
        fake::{FakeCredentials, FakeTransportFactory},
    },
    providers::{
        ErrorKind, IMAP_SMTP_PROVIDER, OperationContext, OperationId, PreparedSubmission,
        ProviderError, ProviderId, ReconcileOutcome, SentCopyPolicy, SubmissionAttachment,
        SubmissionOutcome, SubmissionReceipt, SubmissionRequest, SubmissionService,
    },
    send::{
        self, OutboxId, OutgoingMessage, SubmissionObserver, SubmissionState, build_mime,
        build_mime_with_parts, outbox_entry, queue_send, queue_submission,
    },
    transport::FakeTransport,
};
use tempfile::TempDir;

fn account() -> Account {
    Account {
        id: AccountId("account-1".to_string()),
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
        backend: MailBackend::ImapSmtp {
            username: "ada@example.com".to_string(),
            imap: ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: Security::Tls,
            },
            smtp: ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
            auth: AuthKind::Password,
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store.upsert_account(&account()).expect("seed account");
    store
}

fn message(subject: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account().id,
        to: vec![Address::new(None, "charles@example.com")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.to_string(),
        body: "Hello from the provider outbox.".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    }
}

fn context() -> OperationContext {
    OperationContext::with_timeout(account().id, 1, Duration::from_secs(30))
}

fn part(id: &str, bytes: Vec<u8>) -> SubmissionAttachment {
    SubmissionAttachment {
        id: id.to_string(),
        filename: format!("{id}.txt"),
        content_type: "text/plain".to_string(),
        bytes,
        content_id: None,
        disposition: AttachmentDisposition::Attachment,
    }
}

/// A scripted submission service with precise control over each transition.
#[derive(Default)]
struct RecordingSubmission {
    policy: Option<SentCopyPolicy>,
    prepare_error: Option<ProviderError>,
    submit_error: Option<ProviderError>,
    outcome: Option<SubmissionOutcome>,
    reconcile: Option<ReconcileOutcome>,
    reconcile_error: Option<ProviderError>,
    fail_finalize: usize,
    prepare_calls: usize,
    submit_calls: usize,
    reconcile_calls: usize,
    finalize_calls: usize,
    requests: Vec<SubmissionRequest>,
    finalized: Vec<(OperationId, SubmissionReceipt)>,
}

impl RecordingSubmission {
    fn policy(mut self, policy: SentCopyPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    fn outcome(mut self, outcome: SubmissionOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    fn reconcile(mut self, outcome: ReconcileOutcome) -> Self {
        self.reconcile = Some(outcome);
        self
    }

    fn reconcile_error(mut self, error: ProviderError) -> Self {
        self.reconcile_error = Some(error);
        self
    }

    fn fail_finalize(mut self, times: usize) -> Self {
        self.fail_finalize = times;
        self
    }

    fn prepare_error(mut self, error: ProviderError) -> Self {
        self.prepare_error = Some(error);
        self
    }
}

#[async_trait]
impl SubmissionService for RecordingSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        self.policy.unwrap_or(SentCopyPolicy::ClientAppend)
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        self.prepare_calls += 1;
        self.requests.push(request.clone());
        if let Some(error) = &self.prepare_error {
            return Err(error.clone());
        }
        Ok(PreparedSubmission {
            version: 1,
            backend: ProviderId::from(IMAP_SMTP_PROVIDER),
            raw: Some(b"prepared bytes".to_vec()),
            reference: Some(format!("prepared:{}", request.operation)),
        })
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        self.submit_calls += 1;
        self.requests.push(request.clone());
        if let Some(error) = &self.submit_error {
            return Err(error.clone());
        }
        Ok(self.outcome.clone().unwrap_or_else(|| {
            SubmissionOutcome::Accepted(SubmissionReceipt {
                operation: request.operation.clone(),
                remote_message: None,
                remote_thread: None,
            })
        }))
    }

    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        _ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        self.reconcile_calls += 1;
        self.requests.push(request.clone());
        if let Some(error) = &self.reconcile_error {
            return Err(error.clone());
        }
        Ok(self
            .reconcile
            .clone()
            .unwrap_or(ReconcileOutcome::NotAccepted))
    }

    async fn finalize_sent(
        &mut self,
        request: &SubmissionRequest,
        receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.finalize_calls += 1;
        self.finalized
            .push((request.operation.clone(), receipt.clone()));
        if self.fail_finalize > 0 {
            self.fail_finalize -= 1;
            return Err(ProviderError::transient("the Sent copy is not ready yet"));
        }
        Ok(())
    }
}

#[derive(Default)]
struct ObservedAcceptances {
    receipts: Vec<SubmissionReceipt>,
}

impl SubmissionObserver for ObservedAcceptances {
    fn accepted(&mut self, _entry: &pige_core::send::OutboxEntry, receipt: &SubmissionReceipt) {
        self.receipts.push(receipt.clone());
    }
}

#[tokio::test]
async fn server_managed_sent_never_appends() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Server managed"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default().policy(SentCopyPolicy::ServerManaged);

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    assert_eq!(report.sent, 1);
    assert_eq!(service.submit_calls, 1);
    assert_eq!(service.finalize_calls, 0, "the provider owns the Sent copy");
    assert!(service.finalized.is_empty());
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
}

#[tokio::test]
async fn smtp_accepted_retries_only_sent_copy() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Retry the copy"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default().fail_finalize(1);

    let first = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(first.sent_local, 1);
    assert_eq!(service.submit_calls, 1);
    assert_eq!(service.finalize_calls, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Accepted
    );

    let second = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(second.sent, 1);
    assert_eq!(
        service.submit_calls, 1,
        "the accepted message is never submitted twice"
    );
    assert_eq!(service.finalize_calls, 2);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
}

#[tokio::test]
async fn killed_submitting_becomes_uncertain() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Killed submitting"), Vec::new()).unwrap();
    store
        .set_outbox_state(&id, SubmissionState::Submitting, None)
        .unwrap();
    let mut service = RecordingSubmission::default().reconcile(ReconcileOutcome::Unknown);

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    assert_eq!(report.uncertain, 1);
    assert_eq!(service.reconcile_calls, 1);
    assert_eq!(service.submit_calls, 0, "an ambiguous send is not retried");
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Uncertain
    );
}

#[tokio::test]
async fn transient_reconcile_keeps_submitting_retryable() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Transient reconcile"), Vec::new()).unwrap();
    store
        .set_outbox_state(&id, SubmissionState::Submitting, None)
        .unwrap();
    let mut service = RecordingSubmission::default()
        .reconcile_error(ProviderError::transient("the provider is busy"));

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    assert_eq!(report.uncertain, 0, "a temporary failure is not ambiguous");
    assert_eq!(report.queued, 1);
    assert_eq!(service.reconcile_calls, 1);
    assert_eq!(service.submit_calls, 0);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Submitting,
        "the next pass must be able to reconcile again"
    );

    // Once the provider is reachable, the same row still reconciles and
    // completes; it was never stranded as `Uncertain`.
    service.reconcile_error = None;
    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
}

#[tokio::test]
async fn offline_reconcile_keeps_preparing_retryable() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Offline reconcile"), Vec::new()).unwrap();
    store
        .set_outbox_state(&id, SubmissionState::Preparing, None)
        .unwrap();
    let mut service = RecordingSubmission::default()
        .reconcile_error(ProviderError::offline("the server is unreachable"));

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    assert_eq!(report.uncertain, 0);
    assert_eq!(report.queued, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Preparing
    );

    service.reconcile_error = None;
    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
}

#[tokio::test]
async fn killed_preparing_reconciles_draft_before_repeat() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Killed preparing"), Vec::new()).unwrap();
    store
        .set_outbox_state(&id, SubmissionState::Preparing, None)
        .unwrap();
    let mut service = RecordingSubmission::default().reconcile(ReconcileOutcome::NotAccepted);

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    assert_eq!(report.sent, 1);
    assert_eq!(
        service.reconcile_calls, 1,
        "the provider draft is checked first"
    );
    assert_eq!(service.prepare_calls, 1);
    assert_eq!(service.submit_calls, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Complete
    );
}

#[tokio::test]
async fn timeout_after_acceptance_never_auto_resends() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Timed out"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default().fail_finalize(5);

    send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(service.submit_calls, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Accepted
    );

    // Simulate the flush being interrupted by a deadline after acceptance: the
    // next pass may retry the Sent copy, but must never submit again.
    let mut expired = context();
    expired.deadline = tokio::time::Instant::now();
    let report = send::flush_provider_outbox(&store, &account(), &mut service, &expired)
        .await
        .unwrap();
    assert_eq!(report.sent, 0);
    assert_eq!(report.sent_local, 1);
    assert_eq!(service.submit_calls, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Accepted
    );
}

#[test]
fn v4_sending_is_uncertain() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("account-1", "ada@example.com", 0)
        .outbox("out-1", "account-1", "sending");
    let store = fixture.into_store();
    store.migrate().unwrap();
    let entry = store
        .outbox_entry(&OutboxId("out-1".to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(entry.state, SubmissionState::Uncertain);
    assert!(entry.operation_id.as_str().contains("out-1"));
}

#[test]
fn v4_sent_local_keeps_original_backend() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("account-1", "ada@example.com", 0)
        .outbox("out-1", "account-1", "sent_local");
    let store = fixture.into_store();
    store.migrate().unwrap();
    let entry = store
        .outbox_entry(&OutboxId("out-1".to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(entry.state, SubmissionState::Accepted);
    assert_eq!(
        entry.backend.as_ref(),
        Some(&ProviderId::from(IMAP_SMTP_PROVIDER)),
        "a legacy Sent-copy repair must stay on the backend that accepted it"
    );
}

#[tokio::test]
async fn accepted_learns_once() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Learn once"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default();
    let mut observer = ObservedAcceptances::default();

    send::flush_provider_outbox_with_observer(
        &store,
        &account(),
        &mut service,
        &context(),
        &mut observer,
    )
    .await
    .unwrap();
    assert_eq!(observer.receipts.len(), 1);
    assert_eq!(
        observer.receipts[0].operation,
        outbox_entry(&store, &id).unwrap().unwrap().operation_id
    );

    send::flush_provider_outbox_with_observer(
        &store,
        &account(),
        &mut service,
        &context(),
        &mut observer,
    )
    .await
    .unwrap();
    assert_eq!(
        observer.receipts.len(),
        1,
        "a later flush must not train the same acceptance again"
    );
}

#[tokio::test]
async fn bcc_envelope_is_preserved() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let mut outgoing = message("Blind copy");
    let blind = Address::new(None, "hidden@example.com");
    outgoing.bcc = vec![blind.clone()];
    queue_submission(&store, outgoing.clone(), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default();

    send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();

    let request = service.requests.first().expect("submission request");
    assert_eq!(request.message.bcc, vec![blind.clone()]);
    assert!(
        request
            .message
            .recipients()
            .iter()
            .any(|address| address.email == blind.email),
        "the SMTP envelope must include the Bcc recipient"
    );

    let from = Address::new(
        Some(account().display_name.clone()),
        account().email.clone(),
    );
    let raw = build_mime_with_parts(&from, &outgoing, &[]).unwrap();
    let text = String::from_utf8(raw).unwrap();
    assert!(!text.contains("hidden@example.com"));
    assert!(!text.contains("Bcc:"));
}

#[tokio::test]
async fn prepared_payload_has_no_auth_secret() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let marker = "auth-secret-marker-do-not-persist";
    let credentials = Arc::new(FakeCredentials::default());
    credentials.set(&account().id, marker).unwrap();
    let transports = Arc::new(FakeTransportFactory::new());
    transports.add_transport(
        account().id,
        FakeTransport {
            folders: vec![pige_core::transport::RemoteFolder {
                name: "Sent".to_string(),
                delimiter: Some("/".to_string()),
                kind: pige_core::FolderKind::Sent,
                selectable: true,
            }],
            ..FakeTransport::default()
        },
    );

    let outgoing = message("No secrets");
    queue_submission(&store, outgoing.clone(), Vec::new()).unwrap();
    let mut adapter =
        pige_core::providers::imap::ImapSubmission::new(transports.clone(), account());

    send::flush_provider_outbox(&store, &account(), &mut adapter, &context())
        .await
        .unwrap();

    let entry = store.outbox_items(Some(&account().id)).unwrap().remove(0);
    let durable = format!("{entry:?}");
    assert!(
        !durable.contains(marker),
        "the outbox must not persist credentials"
    );
    let prepared = entry.prepared.expect("a prepared artifact is persisted");
    assert!(!format!("{prepared:?}").contains(marker));
    assert!(!serde_json::to_string(&prepared).unwrap().contains(marker));
    // No field of the prepared artifact is a credential slot.
    let debug = format!("{prepared:?}");
    assert!(!debug.contains("password"));
    assert!(!debug.contains("token"));
    assert!(!debug.contains("private_key"));
}

#[tokio::test]
async fn attachment_parts_survive_outbox_restart() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let parts = vec![
        part("part-1", b"first part".to_vec()),
        part("part-2", b"second part".to_vec()),
    ];
    let id = queue_submission(&store, message("Attachments"), parts.clone()).unwrap();
    drop(store);

    let reopened = Store::open(dir.path().join("pige.db")).expect("reopen store");
    reopened.migrate().expect("migrate store");
    let stored = reopened.outbox_parts(&id).unwrap();
    assert_eq!(stored, parts);
    assert_eq!(stored[0].id, "part-1");
    assert_eq!(stored[1].id, "part-2");

    let mut service = RecordingSubmission::default();
    send::flush_provider_outbox(&reopened, &account(), &mut service, &context())
        .await
        .unwrap();
    let request = service.requests.first().expect("submission request");
    assert_eq!(request.attachments, parts);
}

#[tokio::test]
async fn attachment_limits_reject_before_delivery() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);

    let oversized = vec![0u8; MAX_ATTACHMENT_BYTES as usize + 1];
    let error = queue_submission(&store, message("Too big"), vec![part("big", oversized)])
        .expect_err("a part over the cap is rejected");
    assert!(error.to_string().contains("too large"), "{error}");

    let half = vec![0u8; (MAX_ATTACHMENT_BYTES / 2 + 1) as usize];
    let error = queue_submission(
        &store,
        message("Too much"),
        vec![part("one", half.clone()), part("two", half)],
    )
    .expect_err("attachment bytes over the total cap are rejected");
    assert!(error.to_string().contains("attachment"), "{error}");

    assert!(
        store.outbox_items(Some(&account().id)).unwrap().is_empty(),
        "a rejected submission must not be persisted"
    );
}

#[tokio::test]
async fn empty_parts_preserve_plaintext_compose() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let outgoing = message("Plain text");
    let id = queue_submission(&store, outgoing.clone(), Vec::new()).unwrap();
    assert!(store.outbox_parts(&id).unwrap().is_empty());

    let from = Address::new(
        Some(account().display_name.clone()),
        account().email.clone(),
    );
    let with_parts = build_mime_with_parts(&from, &outgoing, &[]).unwrap();
    let plain = build_mime(&from, &outgoing).unwrap();
    for raw in [with_parts, plain] {
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(!text.contains("multipart/mixed"));
        assert!(text.contains("Hello from the provider outbox."));
    }

    let mut service = RecordingSubmission::default();
    send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert!(service.requests[0].attachments.is_empty());
}

#[tokio::test]
async fn legacy_failed_with_unknown_cause_is_uncertain() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy.db");
    let fixture = LegacyV4::create(&path);
    fixture
        .account("account-1", "ada@example.com", 0)
        .outbox("out-1", "account-1", "failed");
    let store = fixture.into_store();
    store.migrate().unwrap();
    let entry = store
        .outbox_entry(&OutboxId("out-1".to_string()))
        .unwrap()
        .unwrap();
    assert_eq!(
        entry.state,
        SubmissionState::Uncertain,
        "an old failure may hide an interrupted send and must not be retried"
    );
}

#[tokio::test]
async fn queue_send_still_persists_an_empty_parts_submission() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_send(&store, message("Wrapper")).await.unwrap();
    assert!(store.outbox_parts(&id).unwrap().is_empty());
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Queued
    );
}

#[tokio::test]
async fn a_rejected_submission_is_terminal() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Rejected"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default().outcome(SubmissionOutcome::Rejected(
        ProviderError::new(ErrorKind::Forbidden, "the provider refused the message"),
    ));

    let first = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(first.failed, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Rejected
    );

    let second = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(second.failed, 0);
    assert_eq!(service.submit_calls, 1);
}

#[tokio::test]
async fn an_unknown_submit_outcome_becomes_uncertain() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Unknown"), Vec::new()).unwrap();
    let mut service = RecordingSubmission::default().outcome(SubmissionOutcome::Unknown);

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(report.uncertain, 1);
    assert_eq!(service.submit_calls, 1);
    assert_eq!(
        outbox_entry(&store, &id).unwrap().unwrap().state,
        SubmissionState::Uncertain
    );
}

#[tokio::test]
async fn sent_copy_append_is_not_repeated_when_evidence_exists() {
    let transports = Arc::new(FakeTransportFactory::new());
    let sent = pige_core::transport::RemoteFolder {
        name: "Sent".to_string(),
        delimiter: Some("/".to_string()),
        kind: pige_core::FolderKind::Sent,
        selectable: true,
    };
    transports.add_transport(
        account().id,
        FakeTransport {
            folders: vec![sent],
            search_results: vec![pige_core::Uid(7)],
            ..FakeTransport::default()
        },
    );
    let mut service =
        pige_core::providers::imap::ImapSubmission::new(transports.clone(), account());
    let request = SubmissionRequest {
        operation: OperationId::new("op-1"),
        message: message("Already copied"),
        attachments: Vec::new(),
        prepared: Some(PreparedSubmission {
            version: 1,
            backend: ProviderId::from(IMAP_SMTP_PROVIDER),
            raw: Some(
                b"Message-ID: <already-copied@example.com>\r\nSubject: x\r\n\r\nbody".to_vec(),
            ),
            reference: None,
        }),
    };
    let receipt = SubmissionReceipt {
        operation: request.operation.clone(),
        remote_message: None,
        remote_thread: None,
    };

    service
        .finalize_sent(&request, &receipt, &context())
        .await
        .expect("existing evidence makes the copy complete");
    let transport = transports.transport(&account().id).await.unwrap();
    assert_eq!(transport.search_calls.len(), 1);
    assert!(
        transport.append_calls.is_empty(),
        "an evidenced Sent copy must not be appended again"
    );
}

#[tokio::test]
async fn sent_copy_is_not_appended_when_evidence_cannot_be_checked() {
    let transports = Arc::new(FakeTransportFactory::new());
    let sent = pige_core::transport::RemoteFolder {
        name: "Sent".to_string(),
        delimiter: Some("/".to_string()),
        kind: pige_core::FolderKind::Sent,
        selectable: true,
    };
    transports.add_transport(
        account().id,
        FakeTransport {
            folders: vec![sent],
            fail_search: true,
            ..FakeTransport::default()
        },
    );
    let mut service =
        pige_core::providers::imap::ImapSubmission::new(transports.clone(), account());
    let request = SubmissionRequest {
        operation: OperationId::new("op-1"),
        message: message("Unverifiable copy"),
        attachments: Vec::new(),
        prepared: Some(PreparedSubmission {
            version: 1,
            backend: ProviderId::from(IMAP_SMTP_PROVIDER),
            raw: Some(b"Message-ID: <unverifiable@example.com>\r\nSubject: x\r\n\r\nbody".to_vec()),
            reference: None,
        }),
    };
    let receipt = SubmissionReceipt {
        operation: request.operation.clone(),
        remote_message: None,
        remote_thread: None,
    };

    let error = service
        .finalize_sent(&request, &receipt, &context())
        .await
        .expect_err("unverifiable append is left pending");
    assert_eq!(error.kind, ErrorKind::UnknownWriteOutcome);
    let transport = transports.transport(&account().id).await.unwrap();
    assert!(
        transport.append_calls.is_empty(),
        "an ambiguous APPEND must never be retried blindly"
    );
}

#[tokio::test]
async fn a_transient_submit_error_requeues_without_losing_the_operation() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    let id = queue_submission(&store, message("Offline"), Vec::new()).unwrap();
    let mut service =
        RecordingSubmission::default().prepare_error(ProviderError::offline("no network"));

    let report = send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(report.queued, 1);
    let entry = outbox_entry(&store, &id).unwrap().unwrap();
    assert_eq!(entry.state, SubmissionState::Queued);
    assert_eq!(entry.retry_count, 1);
    let operation = entry.operation_id.clone();

    // Re-run with a clean service: the same durable operation id is reused.
    let mut service = RecordingSubmission::default();
    send::flush_provider_outbox(&store, &account(), &mut service, &context())
        .await
        .unwrap();
    assert_eq!(service.requests[1].operation, operation);
}
