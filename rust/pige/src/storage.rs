//! Reading backups in shared Downloads, on Android.
//!
//! Android attributes a `MediaStore` file to the install that wrote it, so after
//! a reflash the scoped query cannot see the backups a previous install wrote
//! without "All files access". Elsewhere Downloads is a plain directory that
//! needs no permission, so access is always granted. Adapted from qlaunch's
//! `storage.rs` (bitshift, MIT).

/// Whether the app can list backups written by a previous install.
#[cfg(target_os = "android")]
pub(crate) fn has_all_files_access() -> bool {
    pige_core::platform::android::has_all_files_access().unwrap_or(false)
}

#[cfg(not(target_os = "android"))]
pub(crate) fn has_all_files_access() -> bool {
    true
}

/// Opens the system "All files access" screen, or does nothing off Android.
#[cfg(target_os = "android")]
pub(crate) fn request_all_files_access() -> Result<(), String> {
    pige_core::platform::android::request_all_files_access()
}

#[cfg(not(target_os = "android"))]
pub(crate) fn request_all_files_access() -> Result<(), String> {
    Ok(())
}
