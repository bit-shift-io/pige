//! Host-platform wiring for the desktop shell.
//!
//! The core owns the platform traits; this module chooses concrete adapters and
//! the per-user data/cache directories. Linux uses XDG/Secret Service; Android
//! uses app-private storage, the Keystore, and the JNI bridge.

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

/// The real Android platform: app-private storage from `PigeBridge`, Android
/// Keystore credentials, the notification bridge, `Intent`/`FileProvider`
/// opening, and the TLS IMAP transport.
#[cfg(target_os = "android")]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::{
        CredentialStore,
        android::{AndroidCredentials, AndroidNotifier, AndroidOpener, AndroidPaths},
    };

    let paths = AndroidPaths::load().unwrap_or_else(|error| {
        // Never guess a directory for mail data or credentials. An empty path
        // makes `api::init` fail and the shell shows that error instead of
        // silently writing somewhere else.
        log::error!("android storage paths unavailable: {error}");
        AndroidPaths {
            data_dir: std::path::PathBuf::new(),
            cache_dir: std::path::PathBuf::new(),
        }
    });
    let credentials: Arc<dyn CredentialStore> = Arc::new(AndroidCredentials::new());
    api::Config {
        data_dir: paths.data_dir,
        cache_dir: paths.cache_dir.clone(),
        credentials: credentials.clone(),
        notifier: Arc::new(AndroidNotifier::new()),
        opener: Arc::new(AndroidOpener::new(paths.cache_dir)),
        transport: Arc::new(pige_core::platform::android::AndroidTransportFactory::new(
            credentials,
        )),
    }
}

/// Non-Linux, non-Android targets keep the temporary in-memory adapters until
/// their own platform layer is implemented.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
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

/// Apply the persisted background-sync choice. Android schedules the periodic
/// WorkManager job when enabled and cancels it when disabled; the desktop has no
/// background worker, so this is a deliberate no-op there.
#[cfg(target_os = "android")]
pub(crate) fn apply_background_sync(enabled: bool, minutes: u16) {
    let result = if enabled {
        pige_core::platform::android::schedule_sync(minutes)
    } else {
        pige_core::platform::android::cancel_sync()
    };
    if let Err(error) = result {
        log::warn!("update Android background sync: {error}");
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) fn apply_background_sync(_enabled: bool, _minutes: u16) {}

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
