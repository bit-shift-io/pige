//! Android platform adapters: Keystore-backed credentials, `NotificationManager`
//! notifications, `Intent`/`FileProvider` opening, and the TLS mail transports.
//!
//! Everything OS-specific crosses a small JNI bridge (`bitshift/pige/PigeBridge`).
//! The `JavaVM` and the bridge's global class reference are cached by the shell's
//! `JNI_OnLoad` and installed through [`install_vm`] / [`install_bridge_class`].
//! Every call attaches the calling thread; callbacks can arrive on the UI thread,
//! a WorkManager worker, or a Tokio worker, and none of them is assumed to be
//! attached already.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use futures::future::BoxFuture;
use jni::objects::{GlobalRef, JValue};
use jni::{JNIEnv, JavaVM};

use crate::account::{Account, AccountId};
use crate::platform::{
    CredentialStore, CredentialsUnavailable, Notifier, Opener, Result, TransportFactory,
};
use crate::sync::NewMailSummary;
use crate::transport::{MailTransport, SmtpSender, imap, smtp};

/// The Kotlin object every adapter calls. Fixed by the Android package name.
pub const BRIDGE_CLASS: &str = "bitshift/pige/PigeBridge";

static ANDROID_VM: OnceLock<JavaVM> = OnceLock::new();
static BRIDGE: OnceLock<GlobalRef> = OnceLock::new();

/// Install the process-wide JVM reference. Called by `JNI_OnLoad`.
pub fn install_vm(vm: JavaVM) {
    let _ = ANDROID_VM.set(vm);
}

/// Install the cached `PigeBridge` class reference. Called by `JNI_OnLoad` on
/// the thread whose class loader can see the app's classes.
pub fn install_bridge_class(class: GlobalRef) {
    let _ = BRIDGE.set(class);
}

fn class() -> Result<&'static GlobalRef> {
    BRIDGE
        .get()
        .ok_or_else(|| "the PigeBridge class was not captured by JNI_OnLoad".to_string())
}

fn clear_pending(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

fn jni_error(env: &mut JNIEnv, error: jni::errors::Error) -> String {
    // Without clearing it, a pending Java exception aborts every later call on
    // this thread. The message is all the Rust side needs.
    clear_pending(env);
    format!("JNI call failed: {error}")
}

/// Attach the current thread to the JVM and run `f`. `attach_current_thread`
/// returns a guard that detaches again when this call was the one that attached.
fn with_env<T>(f: impl FnOnce(&mut JNIEnv) -> Result<T>) -> Result<T> {
    let vm = ANDROID_VM
        .get()
        .ok_or_else(|| "the JavaVM is not available".to_string())?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|error| format!("attach JNI thread: {error}"))?;
    f(&mut env)
}

/// Call a static bridge method that returns `String?` and takes no arguments.
fn call_string(method: &str) -> Result<Option<String>> {
    with_env(|env| {
        let result = env
            .call_static_method(class()?, method, "()Ljava/lang/String;", &[])
            .map_err(|error| jni_error(env, error))?;
        let object = result.l().map_err(|error| jni_error(env, error))?;
        if object.is_null() {
            return Ok(None);
        }
        let text: jni::objects::JString = object.into();
        let value: String = env
            .get_string(&text)
            .map_err(|error| jni_error(env, error))?
            .into();
        Ok(Some(value))
    })
}

/// Call a static bridge method that takes one `String` and returns `String?`.
fn call_string_arg(method: &str, arg: &str) -> Result<Option<String>> {
    with_env(|env| {
        let arg = env.new_string(arg).map_err(|error| jni_error(env, error))?;
        let result = env
            .call_static_method(
                class()?,
                method,
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[(&arg).into()],
            )
            .map_err(|error| jni_error(env, error))?;
        let object = result.l().map_err(|error| jni_error(env, error))?;
        if object.is_null() {
            return Ok(None);
        }
        let text: jni::objects::JString = object.into();
        let value: String = env
            .get_string(&text)
            .map_err(|error| jni_error(env, error))?
            .into();
        Ok(Some(value))
    })
}

/// Call a static bridge method that takes only `String` arguments and returns
/// `void`.
fn call_void(method: &str, args: &[&str]) -> Result<()> {
    with_env(|env| {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(env.new_string(arg).map_err(|error| jni_error(env, error))?);
        }
        let signature = format!("({})V", "Ljava/lang/String;".repeat(args.len()));
        let jvalues: Vec<JValue> = values.iter().map(|value| value.into()).collect();
        env.call_static_method(class()?, method, &signature, &jvalues)
            .map(|_| ())
            .map_err(|error| jni_error(env, error))
    })
}

/// Call a static bridge method that takes no arguments and returns `boolean`.
fn call_bool(method: &str) -> Result<bool> {
    with_env(|env| {
        let result = env
            .call_static_method(class()?, method, "()Z", &[])
            .map_err(|error| jni_error(env, error))?;
        result.z().map_err(|error| jni_error(env, error))
    })
}

/// Call a static bridge method that takes one `int` and returns `void`.
fn call_void_int(method: &str, value: i32) -> Result<()> {
    with_env(|env| {
        env.call_static_method(class()?, method, "(I)V", &[JValue::Int(value)])
            .map(|_| ())
            .map_err(|error| jni_error(env, error))
    })
}

/// Replace the periodic background sync with one that runs every `minutes`.
///
/// WorkManager enforces its own 15-minute minimum, and the settings layer
/// clamps before calling, so the value is passed through unchanged. Kept small
/// and separate from the sync worker so changing a setting never rebuilds the
/// background architecture.
pub fn schedule_sync(minutes: u16) -> Result<()> {
    call_void_int("scheduleSync", i32::from(minutes))
}

/// Cancel the periodic background sync, for when the user turns it off.
pub fn cancel_sync() -> Result<()> {
    call_void("cancelSync", &[])
}

/// App-private storage locations read from `PigeBridge` at startup. Android
/// does not expose a portable XDG equivalent, so these are never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidPaths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// App-private directory holding `pige.json`.
    pub config_dir: PathBuf,
}

impl AndroidPaths {
    /// Read `Context.filesDir`, `Context.cacheDir`, and the config directory
    /// through the bridge.
    pub fn load() -> Result<Self> {
        let data_dir = call_string("dataDir")?
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "PigeBridge.dataDir returned an empty path".to_string())?;
        let cache_dir = call_string("cacheDir")?
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "PigeBridge.cacheDir returned an empty path".to_string())?;
        let config_dir = call_string("configDir")?
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "PigeBridge.configDir returned an empty path".to_string())?;
        Ok(Self {
            data_dir: PathBuf::from(data_dir),
            cache_dir: PathBuf::from(cache_dir),
            config_dir: PathBuf::from(config_dir),
        })
    }
}

/// Parse the JSON array of backup names `PigeBridge.listBackups` returns.
/// Invalid names are dropped, exactly as a filesystem listing would be.
pub fn parse_backup_names_json(json: &str) -> Result<Vec<String>> {
    let names: Vec<String> =
        serde_json::from_str(json).map_err(|error| format!("bad backup list JSON: {error}"))?;
    Ok(crate::platform::backup::sort_backup_names(names))
}

/// Whether this install holds "All files access" (`MANAGE_EXTERNAL_STORAGE`).
/// A reflash attributes a Downloads file to the previous install, so its
/// backups are hidden from the scoped MediaStore query until this is granted.
pub fn has_all_files_access() -> Result<bool> {
    call_bool("hasAllFilesAccess")
}

/// Open the system "All files access" screen for this app.
pub fn request_all_files_access() -> Result<()> {
    call_void("requestAllFilesAccess", &[])
}

/// Downloads-backed backup store using MediaStore, so backups are visible in
/// Files and readable by a later install.
pub struct AndroidBackupStore;

impl AndroidBackupStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AndroidBackupStore {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::platform::BackupStore for AndroidBackupStore {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let body = std::str::from_utf8(bytes)
            .map_err(|error| format!("backup is not valid UTF-8: {error}"))?;
        call_void("writeBackup", &[name, body])
    }

    fn list(&self) -> Result<Vec<String>> {
        let json = call_string("listBackups")?.unwrap_or_else(|| "[]".to_string());
        parse_backup_names_json(&json)
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        call_string_arg("readBackup", name)?
            .map(String::into_bytes)
            .ok_or_else(|| format!("no backup named {name}"))
    }

    fn available(&self) -> bool {
        true
    }
}

/// Android Keystore-backed credentials. The Kotlin side encrypts each secret
/// with AES/GCM under a non-exportable Android Keystore key; plaintext never
/// touches the filesystem. A failed Keystore reports
/// [`CredentialsUnavailable`] so the UI can offer session-only passwords, and
/// this adapter keeps those in process memory only.
#[derive(Default)]
pub struct AndroidCredentials {
    session: Mutex<HashMap<AccountId, String>>,
    /// Accounts this process has successfully read or written through the
    /// Keystore. They can never be treated as session-only.
    persistent: Mutex<HashSet<AccountId>>,
    /// Accounts whose only credential is the in-memory session password.
    session_only: Mutex<HashSet<AccountId>>,
}

/// Redacting `Debug`: the struct holds plaintext session passwords, so only
/// account counts are printable.
impl std::fmt::Debug for AndroidCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AndroidCredentials")
            .field("session_accounts", &self.session().len())
            .field("persistent_accounts", &self.persistent().len())
            .field("session_only_accounts", &self.session_only().len())
            .finish()
    }
}

impl AndroidCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    fn session(&self) -> MutexGuard<'_, HashMap<AccountId, String>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persistent(&self) -> MutexGuard<'_, HashSet<AccountId>> {
        self.persistent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session_only(&self) -> MutexGuard<'_, HashSet<AccountId>> {
        self.session_only
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn unavailable(error: String) -> String {
    CredentialsUnavailable(error).into_message()
}

impl CredentialStore for AndroidCredentials {
    fn get(&self, id: &AccountId) -> Result<Option<String>> {
        match call_string_arg("getPassword", &id.0) {
            Ok(Some(secret)) => {
                self.persistent().insert(id.clone());
                Ok(Some(secret))
            }
            Ok(None) => Ok(self.session().get(id).cloned()),
            Err(error) => {
                if let Some(secret) = self.session().get(id).cloned() {
                    Ok(Some(secret))
                } else {
                    Err(unavailable(error))
                }
            }
        }
    }

    fn set(&self, id: &AccountId, secret: &str) -> Result<()> {
        call_void("setPassword", &[&id.0, secret]).map_err(unavailable)?;
        // A persistent write supersedes any session fallback for this account.
        self.persistent().insert(id.clone());
        self.session().remove(id);
        self.session_only().remove(id);
        Ok(())
    }

    fn delete(&self, id: &AccountId) -> Result<()> {
        let known_persistent = self.persistent().contains(id);
        let was_session_only = self.session_only().remove(id) && !known_persistent;
        self.session().remove(id);

        match call_void("deletePassword", &[&id.0]) {
            Ok(()) => {
                self.persistent().remove(id);
                Ok(())
            }
            // Only an account known to have no persistent entry may be treated
            // as fully cleaned up while the Keystore is unavailable.
            Err(_) if was_session_only => {
                self.persistent().remove(id);
                Ok(())
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    fn set_session(&self, id: &AccountId, secret: &str) -> Result<()> {
        self.session().insert(id.clone(), secret.to_string());
        if !self.persistent().contains(id) {
            self.session_only().insert(id.clone());
        }
        Ok(())
    }

    fn set_transient(&self, id: &AccountId, secret: &str) -> Result<()> {
        self.session().insert(id.clone(), secret.to_string());
        Ok(())
    }

    fn delete_session(&self, id: &AccountId) -> Result<()> {
        self.session().remove(id);
        self.session_only().remove(id);
        Ok(())
    }
}

/// Posts new-mail notifications through `NotificationPoster`.
#[derive(Debug, Default)]
pub struct AndroidNotifier;

impl AndroidNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Notifier for AndroidNotifier {
    fn notify(&self, mail: &NewMailSummary) {
        if let Err(error) = call_void(
            "notify",
            &[
                &mail.account_id.0,
                &mail.thread_id.0,
                &mail.from,
                &mail.subject,
            ],
        ) {
            log::warn!("post Android notification: {error}");
        }
    }
}

/// Opens URLs with `ACTION_VIEW` and cached attachments through the app's
/// `FileProvider`.
#[derive(Debug, Clone)]
pub struct AndroidOpener {
    attachments: PathBuf,
}

impl AndroidOpener {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            attachments: cache_dir.into().join("attachments"),
        }
    }
}

impl Opener for AndroidOpener {
    fn open_file(&self, path: &Path) -> Result<()> {
        // The API already verifies cached paths; resolving again here keeps the
        // bridge from ever handing the OS a path outside the attachment cache.
        let path = crate::platform::opener::file_below(&self.attachments, path)?;
        let path = path
            .to_str()
            .ok_or_else(|| format!("attachment path is not UTF-8: {}", path.display()))?;
        call_void("openFile", &[path]).map_err(|error| format!("open attachment: {error}"))
    }

    fn open_url(&self, url: &str) -> Result<()> {
        let url = crate::platform::opener::checked_url(url)?;
        call_void("openUrl", &[url]).map_err(|error| format!("open URL: {error}"))
    }
}

/// Builds real TLS IMAP/SMTP connections with passwords from the Android
/// credential store. Mirrors the Linux factory's behaviour; the network half is
/// platform-neutral and lives in [`crate::transport`].
pub struct AndroidTransportFactory {
    credentials: Arc<dyn CredentialStore>,
}

impl AndroidTransportFactory {
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Self {
        Self { credentials }
    }

    fn password(&self, account: &Account) -> Result<String> {
        self.credentials
            .get(&account.id)
            .map_err(|error| format!("{}: {error}", account.label))?
            .ok_or_else(|| {
                format!(
                    "{}: no password is stored for this account; edit the account and enter it again",
                    account.label
                )
            })
    }
}

impl TransportFactory for AndroidTransportFactory {
    fn imap<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        Box::pin(async move {
            let password = self.password(account)?;
            let transport = imap::connect(account, &password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn MailTransport>)
        })
    }

    fn imap_with_password<'a>(
        &'a self,
        account: &'a Account,
        password: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn MailTransport>>> {
        Box::pin(async move {
            let transport = imap::connect(account, password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn MailTransport>)
        })
    }

    fn smtp<'a>(&'a self, account: &'a Account) -> BoxFuture<'a, Result<Box<dyn SmtpSender>>> {
        Box::pin(async move {
            let password = self.password(account)?;
            let transport = smtp::connect(account, &password)
                .await
                .map_err(|error| format!("{}: {error}", account.label))?;
            Ok(Box::new(transport) as Box<dyn SmtpSender>)
        })
    }
}
