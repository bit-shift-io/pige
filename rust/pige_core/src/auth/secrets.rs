//! Protected secret envelopes and the repository over the OS credential store.
//!
//! A secret is stored as one opaque string in the platform [`CredentialStore`]
//! (the OS keychain on Linux, the Android Keystore, or an in-memory fake).
//! Existing installs stored a bare password there, so [`SecretEnvelope`] parses
//! a stored value that is not an envelope as a legacy password and reads it back
//! without loss. Nothing in this module ever writes a secret to `pige.json` or
//! the mail cache; only the credential store holds it.

use std::fmt;
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::platform::{CredentialStore, credentials_unavailable, is_valid_credential_key};
use crate::providers::{AuthAttemptId, ErrorKind, OperationContext, ProviderError};
use crate::store::{Store, StoreError};

/// On-disk (well, in-credential-store) envelope format version.
pub const SECRET_ENVELOPE_VERSION: u32 = 1;

/// Upper bound on one serialized envelope. The Android Keystore and Secret
/// Service have no documented hard cap, so this is a sanity limit that keeps a
/// corrupt or hostile payload from being written.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// The secure-store key namespace for staged (pending) credentials.
const STAGED_KEY_PREFIX: &str = "pending_";

/// A Google session. The desktop flow owns a rotating refresh token; the Android
/// flow (per the S2 §9.6 device-only model) may hold only a short-lived access
/// token and the granted scopes. Both live here because they are equally secret.
#[derive(Clone)]
pub struct GoogleSessionSecret {
    pub access_token: Option<SecretString>,
    pub refresh_token: Option<SecretString>,
    /// Unix-epoch milliseconds at which `access_token` expires.
    pub expires_at_ms: Option<i64>,
    /// Scopes actually granted for this session.
    pub scopes: Vec<String>,
}

/// A Proton session. `skp` is the primary user-key passphrase material that
/// unlocks every address key; the login password is deliberately never stored.
#[derive(Clone)]
pub struct ProtonSessionSecret {
    pub access_token: Option<SecretString>,
    pub refresh_token: Option<SecretString>,
    pub skp: Option<SecretString>,
}

/// The credential a provider session protects.
#[derive(Clone)]
pub enum StoredCredentials {
    /// A plain password (legacy IMAP/SMTP, or a mailbox password).
    Password(SecretString),
    Google(GoogleSessionSecret),
    Proton(ProtonSessionSecret),
}

/// A versioned credential envelope. `version` gates future format changes; a
/// value newer than this build is rejected rather than reinterpreted.
#[derive(Clone)]
pub struct SecretEnvelope {
    pub version: u32,
    pub credentials: StoredCredentials,
}

impl SecretEnvelope {
    pub fn password(password: impl Into<String>) -> Self {
        Self {
            version: SECRET_ENVELOPE_VERSION,
            credentials: StoredCredentials::Password(SecretString::from(password.into())),
        }
    }

    pub fn google(secret: GoogleSessionSecret) -> Self {
        Self {
            version: SECRET_ENVELOPE_VERSION,
            credentials: StoredCredentials::Google(secret),
        }
    }

    pub fn proton(secret: ProtonSessionSecret) -> Self {
        Self {
            version: SECRET_ENVELOPE_VERSION,
            credentials: StoredCredentials::Proton(secret),
        }
    }

    /// Serialize the envelope to the single opaque string the credential store
    /// holds. Fails (rather than truncating) when the payload is too large.
    pub fn to_stored_string(&self) -> Result<String, ProviderError> {
        let wire = WireEnvelope {
            version: self.version,
            credentials: self.credentials.to_wire(),
        };
        let json = serde_json::to_string(&wire).map_err(|error| {
            ProviderError::transient("could not encode the protected secret")
                .with_source_detail(error.to_string())
        })?;
        if json.len() > MAX_ENVELOPE_BYTES {
            return Err(ProviderError::unsupported(
                "the protected secret is larger than the store accepts",
            ));
        }
        Ok(json)
    }

    /// Parse a stored string. A value that is not a current envelope is treated
    /// as a legacy bare password, so an upgrade never loses an existing secret.
    pub fn from_stored_string(stored: &str) -> Result<Self, ProviderError> {
        match serde_json::from_str::<WireEnvelope>(stored) {
            Ok(wire) if wire.version <= SECRET_ENVELOPE_VERSION => Ok(Self {
                version: wire.version,
                credentials: StoredCredentials::from_wire(wire.credentials),
            }),
            Ok(wire) => Err(ProviderError::unsupported(format!(
                "protected secret format {} is newer than supported format {}",
                wire.version, SECRET_ENVELOPE_VERSION
            ))),
            Err(_) => Ok(Self {
                version: SECRET_ENVELOPE_VERSION,
                credentials: StoredCredentials::Password(SecretString::from(stored)),
            }),
        }
    }
}

impl StoredCredentials {
    fn to_wire(&self) -> WireCredentials {
        match self {
            Self::Password(password) => WireCredentials::Password {
                password: password.expose_secret().to_string(),
            },
            Self::Google(secret) => WireCredentials::Google {
                access_token: expose(&secret.access_token),
                refresh_token: expose(&secret.refresh_token),
                expires_at_ms: secret.expires_at_ms,
                scopes: secret.scopes.clone(),
            },
            Self::Proton(secret) => WireCredentials::Proton {
                access_token: expose(&secret.access_token),
                refresh_token: expose(&secret.refresh_token),
                skp: expose(&secret.skp),
            },
        }
    }

    fn from_wire(wire: WireCredentials) -> Self {
        match wire {
            WireCredentials::Password { password } => Self::Password(SecretString::from(password)),
            WireCredentials::Google {
                access_token,
                refresh_token,
                expires_at_ms,
                scopes,
            } => Self::Google(GoogleSessionSecret {
                access_token: access_token.map(SecretString::from),
                refresh_token: refresh_token.map(SecretString::from),
                expires_at_ms,
                scopes,
            }),
            WireCredentials::Proton {
                access_token,
                refresh_token,
                skp,
            } => Self::Proton(ProtonSessionSecret {
                access_token: access_token.map(SecretString::from),
                refresh_token: refresh_token.map(SecretString::from),
                skp: skp.map(SecretString::from),
            }),
        }
    }
}

fn expose(secret: &Option<SecretString>) -> Option<String> {
    secret.as_ref().map(|value| value.expose_secret().to_string())
}

impl fmt::Debug for SecretEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretEnvelope")
            .field("version", &self.version)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => formatter.write_str("Password(<redacted>)"),
            Self::Google(secret) => formatter
                .debug_struct("Google")
                .field("access_token", &secret.access_token.as_ref().map(|_| "<redacted>"))
                .field(
                    "refresh_token",
                    &secret.refresh_token.as_ref().map(|_| "<redacted>"),
                )
                .field("expires_at_ms", &secret.expires_at_ms)
                .field("scopes", &secret.scopes)
                .finish(),
            Self::Proton(secret) => formatter
                .debug_struct("Proton")
                .field("access_token", &secret.access_token.as_ref().map(|_| "<redacted>"))
                .field(
                    "refresh_token",
                    &secret.refresh_token.as_ref().map(|_| "<redacted>"),
                )
                .field("skp", &secret.skp.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

/// A non-secret reference to a staged (pending) protected credential entry.
/// The reference is a secure-store key name, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StagedSecretRef(String);

impl StagedSecretRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build the `pending_<attempt uuid>` key for one attempt, validating it
    /// against the platform key whitelist.
    pub fn for_attempt(attempt: &AuthAttemptId) -> Result<Self, ProviderError> {
        let key = format!("{STAGED_KEY_PREFIX}{}", attempt.as_str());
        if !is_valid_credential_key(&key) {
            return Err(ProviderError::unsupported(
                "the authentication attempt id is not a valid credential key",
            ));
        }
        Ok(Self(key))
    }

    /// A reference to an account's own (active) entry, used when resuming a
    /// stored session.
    pub fn for_account(account: &AccountId) -> Self {
        Self(account.0.clone())
    }
}

/// Protected secret storage over the platform credential boundary.
///
/// `save` is the persistent (OS store) path; `save_session` keeps a credential
/// in process memory only. When a durable [`Store`] gate is supplied, both
/// verify the caller's account generation inside the write path, so a refresh
/// that lost its session can never persist a rotated secret.
pub trait SecretRepository: Send + Sync {
    fn load(&self, account: &AccountId) -> Result<Option<SecretEnvelope>, ProviderError>;
    fn save(&self, ctx: &OperationContext, secret: &SecretEnvelope) -> Result<(), ProviderError>;
    fn save_session(
        &self,
        ctx: &OperationContext,
        secret: &SecretEnvelope,
    ) -> Result<(), ProviderError>;
    fn delete(&self, account: &AccountId) -> Result<(), ProviderError>;

    /// Stage a candidate secret for a pending attempt under `pending_<uuid>`.
    /// A staged entry never overwrites an active credential.
    fn stage(
        &self,
        attempt: &AuthAttemptId,
        secret: &SecretEnvelope,
    ) -> Result<StagedSecretRef, ProviderError>;
    fn load_staged(
        &self,
        reference: &StagedSecretRef,
    ) -> Result<Option<SecretEnvelope>, ProviderError>;
    /// Remove a staged entry. Always called when an attempt completes, is
    /// cancelled, or expires.
    fn discard_staged(&self, reference: &StagedSecretRef) -> Result<(), ProviderError>;
}

/// The [`CredentialStore`]-backed [`SecretRepository`].
pub struct CredentialSecretRepository {
    credentials: Arc<dyn CredentialStore>,
    gate: Option<Arc<Store>>,
}

impl CredentialSecretRepository {
    /// A repository without a durable generation gate. Prefer [`Self::with_gate`]
    /// wherever an account generation exists.
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            credentials,
            gate: None,
        }
    }

    /// A repository that fences writes against an account's durable generation.
    pub fn with_gate(credentials: Arc<dyn CredentialStore>, gate: Arc<Store>) -> Self {
        Self {
            credentials,
            gate: Some(gate),
        }
    }

    fn fence(&self, ctx: &OperationContext) -> Result<(), ProviderError> {
        ctx.ensure_active()?;
        if let Some(gate) = &self.gate {
            gate.check_generation(&ctx.account, ctx.generation)
                .map_err(store_error_to_provider)?;
        }
        Ok(())
    }

    fn read(&self, key: &AccountId) -> Result<Option<SecretEnvelope>, ProviderError> {
        self.credentials
            .get(key)
            .map_err(credential_error)?
            .map(|stored| SecretEnvelope::from_stored_string(&stored))
            .transpose()
    }
}

impl SecretRepository for CredentialSecretRepository {
    fn load(&self, account: &AccountId) -> Result<Option<SecretEnvelope>, ProviderError> {
        self.read(account)
    }

    fn save(&self, ctx: &OperationContext, secret: &SecretEnvelope) -> Result<(), ProviderError> {
        self.fence(ctx)?;
        let stored = secret.to_stored_string()?;
        self.credentials
            .set(&ctx.account, &stored)
            .map_err(credential_error)
    }

    fn save_session(
        &self,
        ctx: &OperationContext,
        secret: &SecretEnvelope,
    ) -> Result<(), ProviderError> {
        self.fence(ctx)?;
        let stored = secret.to_stored_string()?;
        self.credentials
            .set_session(&ctx.account, &stored)
            .map_err(credential_error)
    }

    fn delete(&self, account: &AccountId) -> Result<(), ProviderError> {
        self.credentials.delete(account).map_err(credential_error)?;
        let _ = self.credentials.delete_session(account);
        Ok(())
    }

    fn stage(
        &self,
        attempt: &AuthAttemptId,
        secret: &SecretEnvelope,
    ) -> Result<StagedSecretRef, ProviderError> {
        let reference = StagedSecretRef::for_attempt(attempt)?;
        let stored = secret.to_stored_string()?;
        // Staging is process-memory only: an uncommitted candidate never reaches
        // the OS keychain and never touches SQLite. It is discarded on
        // completion, cancellation, or expiry.
        self.credentials
            .set_session(&AccountId(reference.as_str().to_string()), &stored)
            .map_err(credential_error)?;
        Ok(reference)
    }

    fn load_staged(
        &self,
        reference: &StagedSecretRef,
    ) -> Result<Option<SecretEnvelope>, ProviderError> {
        self.read(&AccountId(reference.as_str().to_string()))
    }

    fn discard_staged(&self, reference: &StagedSecretRef) -> Result<(), ProviderError> {
        self.credentials
            .delete_session(&AccountId(reference.as_str().to_string()))
            .map_err(credential_error)
    }
}

/// Map a store failure to a safe, provider-neutral error. Internal SQL details
/// go to `source_detail`, which [`ProviderError`] redacts from `Display`/`Debug`.
pub fn store_error_to_provider(error: StoreError) -> ProviderError {
    match error {
        StoreError::StaleGeneration { .. } => {
            ProviderError::conflict("the account session is out of date")
        }
        StoreError::StaleCheckpoint { .. } => {
            ProviderError::cursor_invalid("the sync checkpoint is out of date")
        }
        StoreError::LeaseHeld | StoreError::LeaseLost => {
            ProviderError::conflict("the account is busy with another operation")
        }
        StoreError::SchemaTooNew { .. } => {
            ProviderError::unsupported("the local cache is newer than this build")
        }
        other => ProviderError::transient("a local cache operation failed")
            .with_source_detail(other.to_string()),
    }
}

/// Map a credential-store failure to a safe error. An unreachable store is a
/// distinct kind so the UI can offer a session-only credential; every other
/// failure is transient.
pub fn credential_error(message: impl Into<String>) -> ProviderError {
    let message = message.into();
    if credentials_unavailable(&message) {
        ProviderError::new(ErrorKind::Unsupported, "the credential store is unavailable")
            .with_source_detail(message)
    } else {
        ProviderError::transient("could not access the account credential")
            .with_source_detail(message)
    }
}

#[derive(Serialize, Deserialize)]
struct WireEnvelope {
    version: u32,
    credentials: WireCredentials,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireCredentials {
    Password {
        password: String,
    },
    Google {
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_at_ms: Option<i64>,
        #[serde(default)]
        scopes: Vec<String>,
    },
    Proton {
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        skp: Option<String>,
    },
}
