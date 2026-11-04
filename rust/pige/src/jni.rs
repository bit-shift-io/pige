//! Android JNI entry points.
//!
//! The Kotlin activity and `SyncWorker` own the Android lifecycle; these
//! functions only install the process-wide `pige_core` state and forward OS
//! events. `JNI_OnLoad` caches the `JavaVM` and the `PigeBridge` class so every
//! later callback can attach its own thread instead of assuming the UI thread.
//!
//! Adapted from qlaunch's `jni_host.rs` (bitshift, MIT).

use std::ffi::c_void;
use std::sync::Mutex;

use jni::objects::{JObject, JString};
use jni::sys::{JNI_VERSION_1_6, jint};
use jni::{JNIEnv, JavaVM};

use pige_core::account::AccountId;
use pige_core::api;
use pige_core::mail::{FolderKind, ThreadId};
use pige_core::platform::android::{self, BRIDGE_CLASS};

fn init_logging() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("pige")
            .with_max_level(log::LevelFilter::Info),
    );
}

static API_INIT: Mutex<()> = Mutex::new(());

fn initialize_api(origin: &str) -> bool {
    // The activity and a WorkManager worker can race on a cold start. Keep the
    // check-and-open sequence to one winner; the loser sees the installed state
    // and returns.
    let _guard = API_INIT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if api::is_initialized() {
        log::info!("{origin}: pige api is already initialized");
        return true;
    }
    match api::init(crate::platform::config()) {
        Ok(()) => {
            log::info!("{origin}: pige api initialized");
            true
        }
        Err(error) => {
            log::error!("{origin}: initialize pige api: {error}");
            false
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create tokio runtime: {error}"))
}

/// Export for `System.loadLibrary("pige")`. Runs on the thread that loaded the
/// library, whose class loader is the app's, so `FindClass` works here and only
/// here; later threads use the cached global reference.
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    let vm = match unsafe { JavaVM::from_raw(vm) } {
        Ok(vm) => vm,
        Err(error) => {
            log::error!("JNI_OnLoad: {error}");
            return JNI_VERSION_1_6;
        }
    };
    if let Ok(mut env) = vm.get_env() {
        match env.find_class(BRIDGE_CLASS) {
            Ok(class) => match env.new_global_ref(class) {
                Ok(global) => android::install_bridge_class(global),
                Err(error) => {
                    let _ = env.exception_clear();
                    log::error!("JNI_OnLoad: cache PigeBridge: {error}");
                }
            },
            Err(error) => {
                // Do not leave an exception pending across JNI_OnLoad.
                let _ = env.exception_clear();
                log::error!("JNI_OnLoad: find PigeBridge: {error}");
            }
        }
    }
    android::install_vm(vm);
    JNI_VERSION_1_6
}

/// Called by `MainActivity.onCreate` before `super.onCreate`. Installs the
/// process-wide core state without starting a window: `android_main` starts the
/// single iced window afterwards.
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_nativeInit(_env: JNIEnv, _this: JObject) {
    init_logging();
    if initialize_api("nativeInit") {
        // Rust owns the persisted background-sync choice; apply it now that the
        // store is open instead of scheduling a hardcoded default.
        let settings = api::settings();
        crate::platform::apply_background_sync(settings.background_sync, settings.sync_minutes);
    }
}

/// Called by `SyncWorker` on a background thread. If WorkManager started this
/// process there is no activity and no iced loop, so initialise the core here
/// first. Returns 0 on success and 1 when the worker should retry.
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_SyncWorker_nativeSyncAll(
    _env: JNIEnv,
    _this: JObject,
) -> jint {
    init_logging();
    if !initialize_api("nativeSyncAll") {
        return 1;
    }
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("nativeSyncAll: {error}");
            return 1;
        }
    };
    match runtime.block_on(api::sync_all()) {
        Ok(snapshot) => {
            log::info!(
                "nativeSyncAll: synced {} account(s)",
                snapshot.accounts.len()
            );
            0
        }
        Err(error) => {
            log::warn!("nativeSyncAll: sync failed: {error}");
            1
        }
    }
}

/// The Back button/gesture. Both this and the older
/// `onBackPressedEvent` symbol dispatch the same core event.
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_nativeBack(_env: JNIEnv, _this: JObject) {
    api::back_pressed();
}

/// Older Back dispatch retained for the existing export. Kept working for
/// callers that still invoke it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_onBackPressedEvent(
    _env: JNIEnv,
    _this: JObject,
) {
    api::back_pressed();
}

/// A notification tap, forwarded once by `MainActivity.onNewIntent`/`onCreate`.
/// Best-effort: select the account's Inbox when it is cached, then open the
/// notified conversation. Failures are logged and never cross back into Java.
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_nativeNotificationTap(
    mut env: JNIEnv,
    _this: JObject,
    account: JString,
    thread: JString,
) {
    init_logging();
    let account: String = env.get_string(&account).map(Into::into).unwrap_or_default();
    let thread: String = env.get_string(&thread).map(Into::into).unwrap_or_default();
    if thread.is_empty() {
        log::warn!("notification tap without a thread id");
        return;
    }

    std::thread::spawn(move || {
        if !initialize_api("notificationTap") {
            return;
        }
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(error) => {
                log::error!("notification tap: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            // Notifications only come from Inbox mail. Select that account's
            // Inbox when its folder is known; otherwise fall back to the
            // unified view. The thread id is what actually opens the reader.
            let inbox = api::snapshot()
                .sections
                .into_iter()
                .find(|section| section.account_id == AccountId(account.clone()))
                .and_then(|section| {
                    section
                        .folders
                        .into_iter()
                        .find(|folder| folder.kind == FolderKind::Inbox)
                })
                .map(|folder| folder.id);
            let selected = match inbox {
                Some(folder) => api::select_folder(&folder).await,
                None => api::select_unified().await,
            };
            if let Err(error) = selected {
                log::warn!("notification tap: select inbox: {error}");
            }
            match api::open_conversation(&ThreadId(thread)).await {
                Ok(_) => log::info!("notification tap: opened conversation"),
                Err(error) => log::warn!("notification tap: open conversation: {error}"),
            }
        });
    });
}
