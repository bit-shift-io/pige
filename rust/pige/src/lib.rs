//! The `pige` mail client shell: an iced daemon that draws a responsive
//! three-pane mail layout from the shared component kit. The Android entry
//! point (`android_main`) and the native Back bridge follow the qlaunch
//! launcher (bitshift, MIT).

pub mod app;
pub mod components;
pub mod fonts;
pub mod html_view;
pub mod icons;
#[cfg(target_os = "android")]
mod jni;
pub mod layout;
pub mod message;
mod platform;
pub mod screens;
pub mod theme;
pub mod update;
pub mod view;

pub use app::{App, SectionView, avatar_color, format_list_date, initials, row_view, sections};
pub use layout::{Breakpoint, Panes, panes};
pub use message::Message;

/// Starts the shell. Called from `main` on desktop and from `android_main` on
/// Android.
pub fn run() -> iced::Result {
    let mut daemon = iced::daemon(App::new, App::update, App::view)
        .title(|_: &App, _| "pige".into())
        .theme(|app: &App, _| {
            if app.dark {
                iced::Theme::Dark
            } else {
                iced::Theme::Light
            }
        })
        .style(|app: &App, _| {
            let palette = theme::palette(app.dark);
            iced::theme::Style {
                background_color: palette.background,
                text_color: palette.on_surface,
            }
        })
        .subscription(App::subscription)
        .default_font(fonts::DEFAULT_FONT);

    for font in fonts::all() {
        daemon = daemon.font(font);
    }

    daemon.run()
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

#[cfg(test)]
mod tests {
    use super::*;

    use iced::window;

    /// Lays the real shell out at each breakpoint, including the drawer
    /// overlay. Marked ignored because it needs a headless wgpu adapter.
    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn shell_lays_out_at_every_breakpoint() {
        use iced::advanced::renderer::Headless;
        use iced::advanced::{layout, widget::Tree};

        let renderer = futures::executor::block_on(<iced::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            16.0.into(),
            None,
        ))
        .expect("headless wgpu adapter");

        for width in [600.0, 900.0, 1300.0] {
            let (mut app, _) = App::new();
            app.size = iced::Size::new(width, 800.0);
            let mut element = app.view(window::Id::unique());
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 800.0)),
            );
            assert_eq!(node.size().width, width);
            assert_eq!(node.size().height, 800.0);
        }

        // The compact drawer overlays the list without changing the window.
        let (mut app, _) = App::new();
        app.size = iced::Size::new(600.0, 800.0);
        app.drawer_open = true;
        let mut element = app.view(window::Id::unique());
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
        );
        assert_eq!(node.size().width, 600.0);
    }

    /// Focused, repeatable measurement of the CPU work iced does for one frame
    /// of the real shell: build the view element, build its widget tree, and lay
    /// it out. GPU rasterization is not included (`Headless` cannot draw the
    /// widget tree), so this is a lower bound on frame time, not a GPU frame
    /// time. Marked ignored; run explicitly with `--ignored --nocapture`.
    #[test]
    #[ignore = "performance measurement; run explicitly with --ignored --nocapture"]
    fn shell_view_build_and_layout_time() {
        use std::time::Instant;

        use iced::advanced::renderer::Headless;
        use iced::advanced::{layout, widget::Tree};

        let renderer = futures::executor::block_on(<iced::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            16.0.into(),
            None,
        ))
        .expect("headless wgpu adapter");

        const RUNS: usize = 50;
        for (label, width) in [("compact", 600.0_f32), ("medium", 900.0), ("expanded", 1300.0)] {
            let (mut app, _) = App::new();
            app.size = iced::Size::new(width, 800.0);
            let limits =
                layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 800.0));

            // Warm up caches before timing.
            for _ in 0..5 {
                let mut element = app.view(window::Id::unique());
                let mut tree = Tree::new(&element);
                let _ = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
            }

            let mut samples = Vec::with_capacity(RUNS);
            for _ in 0..RUNS {
                let started = Instant::now();
                let mut element = app.view(window::Id::unique());
                let mut tree = Tree::new(&element);
                let _ = element.as_widget_mut().layout(&mut tree, &renderer, &limits);
                samples.push(started.elapsed());
            }
            samples.sort_unstable();
            let median = samples[samples.len() / 2];
            let min = samples[0];
            println!(
                "shell {label} ({width}pt): view+layout median={median:?} min={min:?} over {RUNS} runs"
            );
        }
    }
}
