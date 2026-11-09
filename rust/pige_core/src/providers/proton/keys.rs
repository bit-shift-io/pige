//! Pige-owned unlocked Proton key state.
//!
//! The vendored library's key store keeps each private key as armored bytes
//! plus the passphrase that unlocks it. Pige keeps that material only in memory
//! and, on [`UnlockedKeyStore::lock`], wipes the armor and drops every
//! passphrase (which is a `secrecy::SecretString` and zeroizes on drop). Nothing
//! here ever writes to disk or to the credential store.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use zeroize::Zeroize;

use crate::account::AccountId;
use crate::providers::ProviderError;

use proton_core::crypto::keys::{KeyStore, StoredKey};

/// Public, non-secret description of one unlocked address ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtonAddressKeys {
    pub email: String,
    pub key_count: usize,
}

/// The unlocked key hierarchy for one account.
///
/// The store is deliberately small and synchronous: key material is only
/// touched for short, non-awaiting operations. Callers must not hold the inner
/// guard across an await.
#[derive(Default)]
pub struct UnlockedKeyStore {
    state: Mutex<KeyState>,
}

#[derive(Default)]
struct KeyState {
    /// Proton session UID, stored as non-secret session metadata so an
    /// authenticated client can send `x-pm-uid` even though [`crate::auth::TokenSource`]
    /// only exposes the access token.
    session_uid: Option<String>,
    keys: Option<KeyStore>,
}

impl UnlockedKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install freshly unlocked keys and the session UID that owns them.
    /// Replaces (and wipes) any previous material.
    pub(crate) fn install(&self, session_uid: impl Into<String>, keys: KeyStore) {
        let mut state = self.lock_state();
        if let Some(previous) = state.keys.take() {
            wipe_key_store(previous);
        }
        state.session_uid = Some(session_uid.into());
        state.keys = Some(keys);
    }

    /// Whether a key hierarchy is currently available.
    pub fn is_unlocked(&self) -> bool {
        self.lock_state().keys.is_some()
    }

    /// Install only the non-secret session UID for a synthetic metadata test.
    ///
    /// Metadata listing needs the `x-pm-uid` header but no private key. This
    /// hook is compiled only for this crate's tests and the test-support
    /// feature; product builds cannot install a session without unlocking keys.
    #[cfg(any(test, feature = "provider-proton-test-support"))]
    pub fn install_test_session(&self, session_uid: impl Into<String>) {
        self.lock_state().session_uid = Some(session_uid.into());
    }

    /// The non-secret Proton session UID, if a session is installed.
    pub fn session_uid(&self) -> Option<String> {
        self.lock_state().session_uid.clone()
    }

    /// Wipe and release every unlocked key and its passphrase. Any later
    /// operation that needs key material reports reauthentication instead of
    /// silently using a stale copy.
    pub fn lock(&self) {
        let mut state = self.lock_state();
        if let Some(keys) = state.keys.take() {
            wipe_key_store(keys);
        }
        state.session_uid = None;
    }

    /// Non-secret address view for capability checks and UI diagnostics.
    pub fn addresses(&self) -> Result<Vec<ProtonAddressKeys>, ProviderError> {
        self.with_keys(|keys| {
            keys.addresses
                .iter()
                .map(|address| ProtonAddressKeys {
                    email: address.email.clone(),
                    key_count: address.keys.len(),
                })
                .collect()
        })
    }

    /// The default sending address (first by Proton's `Order`).
    pub fn primary_email(&self) -> Option<String> {
        self.with_keys(|keys| keys.primary_address().map(|address| address.email.clone()))
            .ok()
            .flatten()
    }

    /// Borrow the unlocked hierarchy for one synchronous operation.
    pub(crate) fn with_keys<R>(
        &self,
        operation: impl FnOnce(&KeyStore) -> R,
    ) -> Result<R, ProviderError> {
        let state = self.lock_state();
        let keys = state.keys.as_ref().ok_or_else(|| {
            ProviderError::reauthenticate("unlock the Proton mailbox to continue")
        })?;
        Ok(operation(keys))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, KeyState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for UnlockedKeyStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock_state();
        formatter
            .debug_struct("UnlockedKeyStore")
            .field("unlocked", &state.keys.is_some())
            .field(
                "addresses",
                &state
                    .keys
                    .as_ref()
                    .map(|keys| keys.addresses.len())
                    .unwrap_or(0),
            )
            .field(
                "session_uid",
                &state.session_uid.as_ref().map(|_| "<present>"),
            )
            .finish()
    }
}

/// Per-account registry shared by the authentication service and the provider
/// factory, so a login and every later session open use the same key state.
#[derive(Default)]
pub struct ProtonKeyStores {
    stores: Mutex<HashMap<AccountId, Arc<UnlockedKeyStore>>>,
}

impl ProtonKeyStores {
    pub fn new() -> Self {
        Self::default()
    }

    /// The store for an account, creating a locked one on first use.
    pub fn get_or_create(&self, account: &AccountId) -> Arc<UnlockedKeyStore> {
        let mut stores = self
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stores
            .entry(account.clone())
            .or_insert_with(|| Arc::new(UnlockedKeyStore::new()))
            .clone()
    }

    /// The store for an account, if one has ever been opened.
    pub fn get(&self, account: &AccountId) -> Option<Arc<UnlockedKeyStore>> {
        self.stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account)
            .cloned()
    }

    /// Wipe an account's keys and drop the registry entry. Live handles keep a
    /// locked store, so a stale service cannot resurrect old material.
    pub fn lock_and_remove(&self, account: &AccountId) {
        let store = self
            .stores
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(account);
        if let Some(store) = store {
            store.lock();
        }
    }
}

impl fmt::Debug for ProtonKeyStores {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtonKeyStores")
            .field(
                "accounts",
                &self
                    .stores
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len(),
            )
            .finish()
    }
}

pub(crate) fn wipe_key_store(mut keys: KeyStore) {
    for key in keys.user_keys.drain(..) {
        wipe_stored_key(key);
    }
    for mut address in keys.addresses.drain(..) {
        for key in address.keys.drain(..) {
            wipe_stored_key(key);
        }
    }
}

fn wipe_stored_key(mut key: StoredKey) {
    key.armored.zeroize();
    // Replace then drop so the old SecretString is zeroized.
    key.passphrase = SecretString::from(String::new());
}
