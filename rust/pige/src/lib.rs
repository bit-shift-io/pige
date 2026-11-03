//! The `pige` mail client shell: an iced daemon that opens one window showing a
//! centered "pige" placeholder. The Android entry point (`android_main`) and the
//! native Back bridge follow the qlaunch launcher (bitshift, MIT).

use iced::widget::{center, text};
use iced::window;
use iced::{Element, Task};

/// App state. The placeholder shell has nothing to remember yet.
struct App;

impl App {
    fn new() -> (Self, Task<()>) {
        let (_id, open) = window::open(window::Settings {
            size: iced::Size::new(900.0, 700.0),
            ..Default::default()
        });
        (App, open.discard())
    }

    fn update(&mut self, _message: ()) -> Task<()> {
        Task::none()
    }

    fn view(&self, _window: window::Id) -> Element<'_, ()> {
        center(text("pige")).into()
    }
}

/// Starts the shell. Called from `main` on desktop and from `android_main` on
/// Android.
pub fn run() -> iced::Result {
    iced::daemon(App::new, App::update, App::view)
        .title(|_: &App, _| "pige".into())
        .subscription(|_| iced::Subscription::none())
        .run()
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: iced_android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("pige")
            .with_max_level(log::LevelFilter::Info),
    );
    log::info!("pige schema version {}", pige_core::SCHEMA_VERSION);
    iced_android::init(app);
    if let Err(error) = run() {
        log::error!("iced failed: {error}");
    }
}

/// The Back button, from `MainActivity.onBackPressed`. The placeholder shell has
/// a single window and nothing to pop, so Back is a no-op until the mail UI
/// lands. Adapted from qlaunch's
/// `Java_bitshift_qlaunch_MainActivity_onBackPressedEvent`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_onBackPressedEvent(
    _env: jni::JNIEnv,
    _this: jni::objects::JObject,
) {
    log::info!("back pressed (no-op)");
}
