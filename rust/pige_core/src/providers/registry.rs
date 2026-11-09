//! Application-owned provider dispatch.
//!
//! The registry is a plain map assembled at startup, not a process-global
//! plugin loader: the application decides which backends exist and passes the
//! registry around as a `dyn ProviderFactory`.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::account::Account;

use super::error::ProviderError;
use super::traits::{ProviderFactory, ProviderSession};
use super::types::{OperationContext, ProviderId};

/// Maps each [`ProviderId`] to its factory and opens sessions by dispatching on
/// [`Account::provider_id`].
#[derive(Default)]
pub struct ProviderRegistry {
    factories: HashMap<ProviderId, Arc<dyn ProviderFactory>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. A duplicate id is rejected rather than silently
    /// replacing the existing factory.
    pub fn register(
        &mut self,
        id: ProviderId,
        factory: Arc<dyn ProviderFactory>,
    ) -> Result<(), ProviderError> {
        if self.factories.contains_key(&id) {
            return Err(ProviderError::conflict(format!(
                "provider `{id}` is already registered"
            )));
        }
        self.factories.insert(id, factory);
        Ok(())
    }

    pub fn contains(&self, id: &ProviderId) -> bool {
        self.factories.contains_key(id)
    }

    /// The registered provider ids, in unspecified order.
    pub fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        self.factories.keys()
    }
}

#[async_trait]
impl ProviderFactory for ProviderRegistry {
    fn supports(&self, id: &ProviderId) -> bool {
        self.factories.contains_key(id)
    }

    async fn open(
        &self,
        account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        // A cancelled/deadline-expired context must not reach any network.
        ctx.ensure_active()?;

        let id = account.provider_id();
        let factory = self.factories.get(&id).ok_or_else(|| {
            ProviderError::unsupported(format!("no provider registered for `{id}`"))
        })?;
        factory.open(account, ctx).await
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRegistry")
            .field("providers", &self.factories.keys().collect::<Vec<_>>())
            .finish()
    }
}
