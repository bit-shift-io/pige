//! Host-platform wiring for the desktop shell.
//!
//! The core owns the platform traits; this module chooses concrete adapters and
//! the per-user data/cache directories. Android's `FileProvider` and JNI
//! bridge arrive in Task 16.

use std::sync::Arc;

use pige_core::api;

/// The real Linux platform: XDG data/cache directories, Secret Service
/// passwords, `xdg-open`, and the TLS IMAP transport.
#[cfg(target_os = "linux")]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::{
        CredentialStore,
        linux::{LinuxCredentials, LinuxNotifier, LinuxOpener, LinuxTransportFactory},
    };

    let credentials: Arc<dyn CredentialStore> = Arc::new(LinuxCredentials::new());
    api::Config {
        data_dir: xdg_dir("XDG_DATA_HOME", ".local/share"),
        cache_dir: xdg_dir("XDG_CACHE_HOME", ".cache"),
        credentials: credentials.clone(),
        notifier: Arc::new(LinuxNotifier::new()),
        opener: Arc::new(LinuxOpener::new()),
        transport: Arc::new(LinuxTransportFactory::new(credentials)),
    }
}

/// Non-Linux targets keep the temporary in-memory adapters until their own
/// platform layer is implemented.
#[cfg(not(target_os = "linux"))]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::fake::{
        FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
    };

    let root = std::env::temp_dir().join(format!("pige-dev-{}", std::process::id()));
    api::Config {
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        credentials: Arc::new(FakeCredentials::new()),
        notifier: Arc::new(FakeNotifier::new()),
        opener: Arc::new(FakeOpener::new()),
        transport: Arc::new(FakeTransportFactory::new()),
    }
}

/// Resolve `$XDG_*_HOME/pige`, falling back to the XDG default under `$HOME`.
/// A process-scoped temp directory keeps the shell usable if `HOME` is unset.
#[cfg(target_os = "linux")]
fn xdg_dir(variable: &str, fallback: &str) -> std::path::PathBuf {
    if let Some(value) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
        return std::path::PathBuf::from(value).join("pige");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return std::path::PathBuf::from(home).join(fallback).join("pige");
    }
    let leaf = fallback.rsplit('/').next().unwrap_or("data");
    std::env::temp_dir()
        .join(format!("pige-dev-{}", std::process::id()))
        .join(leaf)
        .join("pige")
}
