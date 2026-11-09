//! Host-platform wiring for the desktop shell.
//!
//! The core owns the platform traits; this module chooses concrete adapters and
//! the per-user data/cache directories. Linux uses XDG/Secret Service; Android
//! uses app-private storage, the Keystore, and the JNI bridge.

use std::sync::Arc;

use pige_core::api;

/// Open the same Pige store the core API will use, for a provider factory that
/// needs session/key generation state before `api::init` runs. Both handles
/// point at one SQLite file, so durable generations stay shared.
#[cfg(all(
    any(feature = "provider-google", feature = "provider-proton"),
    any(target_os = "linux", target_os = "android")
))]
fn open_store(data_dir: &std::path::Path) -> Option<Arc<pige_core::store::Store>> {
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        log::warn!("create provider data dir: {error}");
        return None;
    }
    let store = match pige_core::store::Store::open(data_dir.join(pige_core::api::DB_FILE_NAME)) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            log::warn!("open provider store: {error}");
            return None;
        }
    };
    if let Err(error) = store.migrate() {
        log::warn!("migrate provider store: {error}");
        return None;
    }
    Some(store)
}

/// Native Google shell construction (G5).
///
/// Client ids are configuration, never credentials: they come from environment
/// variables or `<config_dir>/google-oauth.json`. No token, secret, or account
/// address is stored in that file. When the desktop id is absent, or any setup
/// step fails, the shell simply omits the Google provider; native Gmail accounts
/// then appear as an unavailable-provider account state instead of breaking
/// startup.
#[cfg(all(
    feature = "provider-google",
    any(target_os = "linux", target_os = "android")
))]
mod google {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Mutex as AsyncMutex;

    use pige_core::auth::{
        AccountSessionManager, AuthenticationService, CredentialSecretRepository,
    };
    use pige_core::platform::CredentialStore;
    use pige_core::providers::ProviderRegistry;
    use pige_core::providers::google::{
        GoogleAuthentication, GoogleEndpoints, GoogleOAuthConfig, google_auth_service,
        google_registry,
    };

    const ATTEMPT_TTL: Duration = Duration::from_secs(300);

    #[derive(Default)]
    struct GoogleSettings {
        desktop_client_id: Option<String>,
        server_client_id: Option<String>,
        server_exchange_url: Option<String>,
    }

    fn non_empty(value: Option<String>) -> Option<String> {
        value.filter(|value| !value.trim().is_empty())
    }

    fn env(name: &str) -> Option<String> {
        non_empty(std::env::var(name).ok())
    }

    /// Read the optional local config, then let the environment override it per
    /// key. The file is never created here; a missing file means "not
    /// configured".
    fn settings(config_dir: &Path) -> GoogleSettings {
        let mut settings = GoogleSettings::default();
        let path = config_dir.join("google-oauth.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(value) => {
                    settings.desktop_client_id = value
                        .get("desktop_client_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    settings.server_client_id = value
                        .get("server_client_id")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    settings.server_exchange_url = value
                        .get("server_exchange_url")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                }
                Err(error) => log::warn!("read {}: {error}", path.display()),
            }
        }
        settings.desktop_client_id = env("PIGE_GOOGLE_DESKTOP_CLIENT_ID")
            .or_else(|| env("GOOGLE_DESKTOP_CLIENT_ID"))
            .or(settings.desktop_client_id);
        settings.server_client_id = env("PIGE_GOOGLE_SERVER_CLIENT_ID")
            .or_else(|| env("GOOGLE_WEB_SERVER_CLIENT_ID"))
            .or(settings.server_client_id);
        settings.server_exchange_url =
            env("PIGE_GOOGLE_SERVER_EXCHANGE_URL").or(settings.server_exchange_url);
        settings
    }

    pub(super) struct GoogleShell {
        pub registry: ProviderRegistry,
        pub auth: Arc<AsyncMutex<Box<dyn AuthenticationService>>>,
    }

    #[cfg(target_os = "linux")]
    pub(super) fn build(
        data_dir: &Path,
        config_dir: &Path,
        credentials: Arc<dyn CredentialStore>,
    ) -> Option<GoogleShell> {
        use pige_core::providers::google::LoopbackRedirect;

        let settings = settings(config_dir);
        let client_id = settings.desktop_client_id?;
        let store = super::open_store(data_dir)?;
        let sessions = Arc::new(AccountSessionManager::new(store.clone()));
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials,
            store.clone(),
        ));
        let make = || {
            GoogleAuthentication::desktop(
                secrets.clone(),
                sessions.clone(),
                store.clone(),
                GoogleOAuthConfig::production(client_id.clone()),
                Arc::new(LoopbackRedirect::new()),
                ATTEMPT_TTL,
            )
        };
        let factory_auth = Arc::new(make());
        let registry = google_registry(
            sessions.clone(),
            GoogleEndpoints::production(),
            factory_auth,
        )
        .map_err(|error| log::warn!("register Google provider: {}", error.safe_message))
        .ok()?;
        Some(GoogleShell {
            registry,
            auth: google_auth_service(make()),
        })
    }

    #[cfg(target_os = "android")]
    pub(super) fn build(
        data_dir: &Path,
        config_dir: &Path,
        credentials: Arc<dyn CredentialStore>,
    ) -> Option<GoogleShell> {
        use pige_core::providers::google::{AndroidOfflineExchange, AndroidPlatformHost};
        let settings = settings(config_dir);
        // Play services identifies the Android OAuth client by package and
        // signing certificate, so no client id is required for on-demand
        // authorization. A server client id/exchange URL is optional and only
        // used for the deferred offline-refresh path.
        let client_id = settings.desktop_client_id.unwrap_or_default();
        let offline = settings
            .server_client_id
            .zip(settings.server_exchange_url)
            .and_then(|(server_client_id, exchange_url)| {
                url::Url::parse(&exchange_url)
                    .ok()
                    .map(|exchange_url| AndroidOfflineExchange {
                        server_client_id,
                        exchange_url,
                    })
            });
        let store = super::open_store(data_dir)?;
        let sessions = Arc::new(AccountSessionManager::new(store.clone()));
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials,
            store.clone(),
        ));
        let host: Arc<dyn pige_core::providers::google::GoogleAndroidHost> =
            Arc::new(AndroidPlatformHost::new());
        let oauth = GoogleOAuthConfig::production(client_id);
        let make = || {
            GoogleAuthentication::android(
                secrets.clone(),
                sessions.clone(),
                store.clone(),
                oauth.clone(),
                host.clone(),
                offline.clone(),
                ATTEMPT_TTL,
            )
        };
        let factory_auth = Arc::new(make());
        let registry = google_registry(
            sessions.clone(),
            GoogleEndpoints::production(),
            factory_auth,
        )
        .map_err(|error| log::warn!("register Google provider: {}", error.safe_message))
        .ok()?;
        Some(GoogleShell {
            registry,
            auth: google_auth_service(make()),
        })
    }
}

/// Native Proton provider registration.
///
/// This wires only the provider factory. Interactive Proton authentication is
/// still a P5 shell decision because [`api::Config::auth`] is a single provider
/// slot; core callers compose both halves with
/// `pige_core::providers::proton::proton_registration`.
#[cfg(all(
    feature = "provider-proton",
    any(target_os = "linux", target_os = "android")
))]
mod proton {
    use std::path::Path;
    use std::sync::Arc;

    use pige_core::auth::{AccountSessionManager, CredentialSecretRepository};
    use pige_core::platform::CredentialStore;
    use pige_core::providers::ProviderRegistry;
    use pige_core::providers::proton::{
        ProtonEndpoints, ProtonKeyStores, ProtonProviderFactory, register_proton,
    };

    pub(super) fn register(
        registry: &mut ProviderRegistry,
        data_dir: &Path,
        credentials: Arc<dyn CredentialStore>,
    ) {
        let Some(store) = super::open_store(data_dir) else {
            return;
        };
        let sessions = Arc::new(AccountSessionManager::new(store.clone()));
        let secrets = Arc::new(CredentialSecretRepository::with_gate(
            credentials,
            store.clone(),
        ));
        let factory = ProtonProviderFactory::new(
            sessions,
            secrets,
            ProtonEndpoints::production(),
            Arc::new(ProtonKeyStores::new()),
        );
        if let Err(error) = register_proton(registry, factory) {
            log::warn!("register Proton provider: {}", error.safe_message);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pige_core::platform::fake::FakeCredentials;
        use pige_core::providers::{PROTON_PROVIDER, ProviderId};

        #[test]
        fn enabling_provider_proton_registers_the_factory() {
            let data_dir = std::env::temp_dir().join(format!(
                "pige-proton-registry-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::default());
            let mut registry = ProviderRegistry::new();
            register(&mut registry, &data_dir, credentials);
            assert!(
                registry.contains(&ProviderId::from(PROTON_PROVIDER)),
                "the provider-proton feature must register the Proton factory"
            );
            let _ = std::fs::remove_dir_all(&data_dir);
        }
    }
}

/// The real Linux platform: XDG data/cache directories, Secret Service
/// passwords, `xdg-open`, and the TLS IMAP transport.
#[cfg(target_os = "linux")]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::{
        CredentialStore, TransportFactory,
        linux::{
            LinuxBackupStore, LinuxCredentials, LinuxNotifier, LinuxOpener, LinuxTransportFactory,
        },
    };

    let credentials: Arc<dyn CredentialStore> = Arc::new(LinuxCredentials::new());
    let transport: Arc<dyn TransportFactory> =
        Arc::new(LinuxTransportFactory::new(credentials.clone()));
    let data_dir = xdg_dir("XDG_DATA_HOME", ".local/share");
    let cache_dir = xdg_dir("XDG_CACHE_HOME", ".cache");
    let config_dir = config_dir();
    #[cfg_attr(
        not(any(feature = "provider-google", feature = "provider-proton")),
        allow(unused_mut)
    )]
    let mut registry =
        pige_core::providers::imap::imap_registry(transport.clone(), credentials.clone());
    #[cfg_attr(not(feature = "provider-google"), allow(unused_mut))]
    let mut auth = None;
    #[cfg(feature = "provider-google")]
    if let Some(shell) = google::build(&data_dir, &config_dir, credentials.clone()) {
        match registry.merge(shell.registry) {
            Ok(()) => auth = Some(shell.auth),
            Err(error) => log::warn!("compose Google provider: {}", error.safe_message),
        }
    }
    #[cfg(feature = "provider-proton")]
    proton::register(&mut registry, &data_dir, credentials.clone());
    let providers: Arc<dyn pige_core::providers::ProviderFactory> = Arc::new(registry);
    api::Config {
        data_dir,
        cache_dir,
        config_dir,
        credentials: credentials.clone(),
        notifier: Arc::new(LinuxNotifier::new()),
        opener: Arc::new(LinuxOpener::new()),
        providers,
        transport,
        backups: Arc::new(LinuxBackupStore::new()),
        auth,
    }
}

/// The real Android platform: app-private storage from `PigeBridge`, Android
/// Keystore credentials, the notification bridge, `Intent`/`FileProvider`
/// opening, and the TLS IMAP transport.
#[cfg(target_os = "android")]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::{
        CredentialStore, TransportFactory,
        android::{
            AndroidBackupStore, AndroidCredentials, AndroidNotifier, AndroidOpener, AndroidPaths,
        },
    };

    let paths = AndroidPaths::load().unwrap_or_else(|error| {
        // Never guess a directory for mail data or credentials. An empty path
        // makes `api::init` fail and the shell shows that error instead of
        // silently writing somewhere else.
        log::error!("android storage paths unavailable: {error}");
        AndroidPaths {
            data_dir: std::path::PathBuf::new(),
            cache_dir: std::path::PathBuf::new(),
            config_dir: std::path::PathBuf::new(),
        }
    });
    let credentials: Arc<dyn CredentialStore> = Arc::new(AndroidCredentials::new());
    let transport: Arc<dyn TransportFactory> = Arc::new(
        pige_core::platform::android::AndroidTransportFactory::new(credentials.clone()),
    );
    #[cfg_attr(
        not(any(feature = "provider-google", feature = "provider-proton")),
        allow(unused_mut)
    )]
    let mut registry =
        pige_core::providers::imap::imap_registry(transport.clone(), credentials.clone());
    #[cfg_attr(not(feature = "provider-google"), allow(unused_mut))]
    let mut auth = None;
    #[cfg(feature = "provider-google")]
    if let Some(shell) = google::build(&paths.data_dir, &paths.config_dir, credentials.clone()) {
        match registry.merge(shell.registry) {
            Ok(()) => auth = Some(shell.auth),
            Err(error) => log::warn!("compose Google provider: {}", error.safe_message),
        }
    }
    #[cfg(feature = "provider-proton")]
    proton::register(&mut registry, &paths.data_dir, credentials.clone());
    let providers: Arc<dyn pige_core::providers::ProviderFactory> = Arc::new(registry);
    api::Config {
        config_dir: paths.config_dir,
        data_dir: paths.data_dir,
        cache_dir: paths.cache_dir.clone(),
        credentials: credentials.clone(),
        notifier: Arc::new(AndroidNotifier::new()),
        opener: Arc::new(AndroidOpener::new(paths.cache_dir)),
        providers,
        transport,
        backups: Arc::new(AndroidBackupStore::new()),
        auth,
    }
}

/// Non-Linux, non-Android targets keep the temporary in-memory adapters until
/// their own platform layer is implemented.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn config() -> api::Config {
    use pige_core::platform::{
        CredentialStore, TransportFactory,
        fake::{FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory},
    };

    let root = std::env::temp_dir().join(format!("pige-dev-{}", std::process::id()));
    let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::new());
    let transport: Arc<dyn TransportFactory> = Arc::new(FakeTransportFactory::new());
    let providers: Arc<dyn pige_core::providers::ProviderFactory> = Arc::new(
        pige_core::providers::imap::imap_registry(transport.clone(), credentials.clone()),
    );
    api::Config {
        data_dir: root.join("data"),
        cache_dir: root.join("cache"),
        config_dir: root.join("config"),
        credentials: credentials.clone(),
        notifier: Arc::new(FakeNotifier::new()),
        opener: Arc::new(FakeOpener::new()),
        providers,
        transport,
        backups: Arc::new(FakeBackupStore::new()),
        auth: None,
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

/// Resolve the configuration root `$XDG_CONFIG_HOME/bitshift/pige`, falling
/// back to `~/.config/bitshift/pige` and then a process-scoped temp directory,
/// matching qlaunch's `bitshift/<app>` layout.
#[cfg(target_os = "linux")]
fn config_dir() -> std::path::PathBuf {
    let base = if let Some(value) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        std::path::PathBuf::from(value)
    } else if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        std::path::PathBuf::from(home).join(".config")
    } else {
        std::env::temp_dir().join(format!("pige-dev-{}", std::process::id()))
    };
    base.join("bitshift").join("pige")
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
