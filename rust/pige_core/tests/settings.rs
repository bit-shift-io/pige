//! Settings persistence: defaults, JSON round trip, and interval clamping.

use pige_core::Store;
use pige_core::settings::{Settings, ThemeMode, load_settings, save_settings};
use tempfile::TempDir;

fn migrated_store() -> (Store, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    (store, dir)
}

#[test]
fn settings_default_to_15_minutes_notifications_on_and_system_theme() {
    let (store, _dir) = migrated_store();

    let settings = load_settings(&store);

    assert_eq!(settings.sync_minutes, 15);
    assert!(settings.notifications);
    assert_eq!(settings.theme, ThemeMode::System);
    assert_eq!(settings, Settings::default());
}

#[test]
fn settings_round_trip_and_clamp_invalid_interval() {
    let (store, _dir) = migrated_store();

    let mut settings = Settings {
        sync_minutes: 30,
        notifications: false,
        theme: ThemeMode::Dark,
    };
    save_settings(&store, &settings).expect("save settings");
    assert_eq!(load_settings(&store), settings);

    // 0 means "use the default" rather than "never".
    settings.sync_minutes = 0;
    save_settings(&store, &settings).expect("save zero");
    assert_eq!(load_settings(&store).sync_minutes, 15);

    // Anything above the maximum is pinned to the maximum.
    settings.sync_minutes = 999;
    save_settings(&store, &settings).expect("save huge");
    assert_eq!(load_settings(&store).sync_minutes, 60);
}
