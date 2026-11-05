//! Settings defaults and the configuration-document round trip.

use pige_core::Store;
use pige_core::config::{self, ConfigDocument};
use pige_core::settings::{Settings, ThemeMode, load_settings};
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
    assert!(settings.background_sync);
    assert_eq!(settings.theme, ThemeMode::System);
    assert_eq!(settings, Settings::default());
}

#[test]
fn settings_round_trip_in_the_configuration_document() {
    let dir = TempDir::new().expect("temp dir");
    let path = config::document_path(dir.path());

    let settings = Settings {
        sync_minutes: 30,
        notifications: false,
        background_sync: false,
        theme: ThemeMode::Dark,
    };
    let doc = ConfigDocument {
        settings: settings.clone(),
        ..ConfigDocument::default()
    };
    config::save(&path, &doc).expect("save document");

    let loaded = config::load(&path).expect("load document").expect("document");
    assert_eq!(loaded.settings, settings);
}
