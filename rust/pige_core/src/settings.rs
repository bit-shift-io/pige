//! User settings, now persisted inside the configuration document
//! (`crate::config::ConfigDocument`). The core owns the schema so every shell
//! (desktop, Android) reads and writes the same values.
//!
//! [`load_settings`] remains for the one-time migration from the old `meta`
//! table; [`clear_settings`] removes that legacy row after the document has
//! taken over. New writes go through `api::save_settings`, which edits the
//! document.

use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError};

/// `meta` key the settings JSON is stored under.
const SETTINGS_KEY: &str = "settings";

/// Interval used when the user has not chosen one, or enters `0`.
pub const DEFAULT_SYNC_MINUTES: u16 = 15;
/// Largest allowed background sync interval, in minutes.
pub const MAX_SYNC_MINUTES: u16 = 60;

/// How the shell picks a palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the operating system's light/dark preference.
    System,
    /// Always use the light scheme.
    Light,
    /// Always use the dark scheme.
    Dark,
}

/// Everything the user can change about pige's behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Minutes between background syncs. `0` is never stored: it is resolved to
    /// [`DEFAULT_SYNC_MINUTES`] on save.
    pub sync_minutes: u16,
    /// Whether new mail posts a desktop/Android notification.
    pub notifications: bool,
    /// Whether the shell runs periodic background sync. Android schedules (or
    /// cancels) WorkManager from this flag; desktop only persists it.
    pub background_sync: bool,
    /// The preferred colour scheme.
    pub theme: ThemeMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sync_minutes: DEFAULT_SYNC_MINUTES,
            notifications: true,
            background_sync: true,
            theme: ThemeMode::System,
        }
    }
}

impl Settings {
    /// Normalise a user-entered interval: `0` means the default, `1..=59` is
    /// kept, and anything at or above [`MAX_SYNC_MINUTES`] is pinned to it.
    pub fn clamp_minutes(minutes: u16) -> u16 {
        match minutes {
            0 => DEFAULT_SYNC_MINUTES,
            1..=59 => minutes,
            _ => MAX_SYNC_MINUTES,
        }
    }
}

/// Load the persisted settings. Missing or corrupt data degrades to the
/// defaults rather than failing the UI; the raw JSON is never surfaced.
pub fn load_settings(store: &Store) -> Settings {
    let Ok(Some(raw)) = store.get_meta(SETTINGS_KEY) else {
        return Settings::default();
    };
    let mut settings: Settings = serde_json::from_str(&raw).unwrap_or_default();
    settings.sync_minutes = Settings::clamp_minutes(settings.sync_minutes);
    settings
}

/// Remove the legacy `meta` settings row after it has been migrated into the
/// configuration document, so a later downgrade cannot resurrect it.
pub fn clear_settings(store: &Store) -> Result<(), StoreError> {
    store.delete_meta(SETTINGS_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_clamping_resolves_zero_to_the_default() {
        assert_eq!(Settings::clamp_minutes(0), DEFAULT_SYNC_MINUTES);
        assert_eq!(Settings::clamp_minutes(1), 1);
        assert_eq!(Settings::clamp_minutes(59), 59);
        assert_eq!(Settings::clamp_minutes(60), MAX_SYNC_MINUTES);
        assert_eq!(Settings::clamp_minutes(u16::MAX), MAX_SYNC_MINUTES);
    }

    #[test]
    fn defaults_are_fifteen_on_and_system() {
        let settings = Settings::default();
        assert_eq!(settings.sync_minutes, 15);
        assert!(settings.notifications);
        assert!(settings.background_sync);
        assert_eq!(settings.theme, ThemeMode::System);
    }
}
