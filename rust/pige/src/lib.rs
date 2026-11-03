//! The `pige` mail client shell: an iced daemon that draws a responsive
//! three-pane mail layout from the shared component kit. The Android entry
//! point (`android_main`) and the native Back bridge follow the qlaunch
//! launcher (bitshift, MIT).

use iced::widget::{MouseArea, column, container, row, scrollable, space, stack};
use iced::window;
use iced::{Element, Fill, Length, Size, Subscription, Task};

pub mod components;
pub mod fonts;
pub mod icons;
pub mod layout;
pub mod theme;

pub use layout::{Breakpoint, Panes, panes};

use components::elastic_scroll::{self, Motion, Policy};
use components::{FolderKind, FolderView, ListRow, PaneTone};

/// Everything the placeholder shell can respond to. Later screens extend this
/// enum rather than styling their own widgets.
#[derive(Debug, Clone)]
pub enum Message {
    Resized(Size),
    Menu,
    CloseDrawer,
    Folder(FolderKind),
    Search(String),
    Compose,
    AddAccount,
    Settings,
    Refresh,
    Noop,
}

/// App state. The shell only remembers the window size and navigation state;
/// mail data arrives in later tasks.
struct App {
    size: Size,
    dark: bool,
    drawer_open: bool,
    selected: FolderKind,
    search: String,
    list_motion: Motion,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let (_id, open) = window::open(window::Settings {
            size: iced::Size::new(900.0, 700.0),
            ..Default::default()
        });
        (
            App {
                size: Size::new(900.0, 700.0),
                dark: true,
                drawer_open: false,
                selected: FolderKind::Inbox,
                search: String::new(),
                list_motion: Motion::new(),
            },
            open.map(|_| Message::Noop),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Resized(size) => {
                self.size = size;
                if Breakpoint::from_width(size.width) == Breakpoint::Expanded {
                    self.drawer_open = false;
                }
            }
            Message::Menu => self.drawer_open = true,
            Message::CloseDrawer => self.drawer_open = false,
            Message::Folder(folder) => {
                self.selected = folder;
                self.drawer_open = false;
            }
            Message::Search(query) => self.search = query,
            Message::Refresh => {}
            Message::Compose | Message::AddAccount | Message::Settings | Message::Noop => {}
        }
        Task::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        window::resize_events().map(|(_, size)| Message::Resized(size))
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        theme::set_active(theme::palette(self.dark));
        let breakpoint = Breakpoint::from_width(self.size.width);
        let panes = panes(breakpoint, self.drawer_open);
        let content = self.content(breakpoint, panes);

        if panes.nav && breakpoint != Breakpoint::Expanded {
            stack![content, self.drawer()].into()
        } else {
            content
        }
    }

    fn content(&self, breakpoint: Breakpoint, panes: Panes) -> Element<'_, Message> {
        let mut layout = row![].width(Fill).height(Fill);

        if panes.nav && breakpoint == Breakpoint::Expanded {
            layout = layout.push(self.nav_pane());
            layout = layout.push(components::divider());
        }
        if panes.list {
            layout = layout.push(self.list_pane(breakpoint, panes.reader));
        }
        if panes.reader {
            layout = layout.push(components::divider());
            layout = layout.push(self.reader_pane());
        }

        container(layout)
            .width(Fill)
            .height(Fill)
            .style(components::pane_style(PaneTone::Page))
            .into()
    }

    fn nav_pane(&self) -> Element<'_, Message> {
        let palette = theme::active();
        let mut nav = column![].spacing(0).width(Fill);
        nav = nav.push(components::app_bar("pige", None, Vec::new()));
        nav = nav.push(space().height(palette.space.sm));
        nav = nav.push(components::folder_row(
            folder("All inboxes", FolderKind::All, 4),
            self.selected == FolderKind::All,
            Message::Folder(FolderKind::All),
        ));

        nav = nav.push(components::section_header(
            "Personal",
            false,
            3,
            Message::Noop,
        ));
        for item in [
            ("Inbox", FolderKind::Inbox, 3),
            ("Starred", FolderKind::Starred, 0),
            ("Sent", FolderKind::Sent, 0),
            ("Drafts", FolderKind::Drafts, 1),
            ("Archive", FolderKind::Archive, 0),
            ("Spam", FolderKind::Spam, 0),
            ("Trash", FolderKind::Trash, 0),
        ] {
            let (name, kind, unread) = item;
            nav = nav.push(components::folder_row(
                folder(name, kind, unread),
                self.selected == kind,
                Message::Folder(kind),
            ));
        }

        nav = nav.push(components::section_header("Work", true, 0, Message::Noop));
        nav = nav.push(space().height(Fill));
        nav = nav.push(components::folder_row(
            folder("Add account", FolderKind::AddAccount, 0),
            false,
            Message::AddAccount,
        ));
        nav = nav.push(components::folder_row(
            folder("Settings", FolderKind::Settings, 0),
            false,
            Message::Settings,
        ));

        container(nav)
            .width(layout::NAV_WIDTH)
            .height(Fill)
            .padding([0.0, palette.space.sm])
            .style(components::pane_style(PaneTone::Navigation))
            .into()
    }

    fn list_pane(&self, breakpoint: Breakpoint, reader_visible: bool) -> Element<'_, Message> {
        let palette = theme::active();
        let leading = (breakpoint != Breakpoint::Expanded)
            .then(|| components::icon_button(icons::MENU, "Open navigation", Message::Menu));
        let actions = vec![
            components::icon_button(icons::SEARCH, "Search", Message::Noop),
            components::icon_button(icons::EDIT, "Compose", Message::Compose),
        ];

        let mut pane = column![].spacing(0).width(Fill).height(Fill);
        pane = pane.push(components::app_bar(
            folder_title(self.selected),
            leading,
            actions,
        ));
        pane = pane.push(
            container(components::search_field(&self.search, Message::Search))
                .padding(iced::Padding {
                    top: 0.0,
                    right: palette.space.lg,
                    bottom: palette.space.md,
                    left: palette.space.lg,
                })
                .width(Fill),
        );

        let mut rows = column![].spacing(0).width(Fill);
        for item in placeholder_rows() {
            rows = rows.push(elastic_scroll::row(
                components::list_row(item, Message::Noop),
                &self.list_motion,
            ));
        }
        pane = pane.push(elastic_scroll::wrap(
            scrollable(elastic_scroll::content(rows.into(), &self.list_motion))
                .width(Fill)
                .height(Fill)
                .style(palette.scrollable())
                .into(),
            self.list_motion.clone(),
            Policy::Normal,
        ));

        let width = if reader_visible {
            Length::Fixed(layout::LIST_WIDTH)
        } else {
            Length::Fill
        };
        container(pane)
            .width(width)
            .height(Fill)
            .style(components::pane_style(PaneTone::Surface))
            .into()
    }

    fn reader_pane(&self) -> Element<'_, Message> {
        let actions = vec![components::icon_button(
            icons::MORE_VERT,
            "More",
            Message::Noop,
        )];
        container(column![
            components::app_bar("Conversation", None, actions),
            components::empty_state(
                "No conversation selected",
                "Choose a message from the list to read it here."
            ),
        ])
        .width(Fill)
        .height(Fill)
        .style(components::pane_style(PaneTone::Surface))
        .into()
    }

    fn drawer(&self) -> Element<'_, Message> {
        let scrim = MouseArea::new(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(components::scrim_style()),
        )
        .on_press(Message::CloseDrawer);

        row![self.nav_pane(), scrim].width(Fill).height(Fill).into()
    }
}

fn folder(name: &str, kind: FolderKind, unread: u32) -> FolderView {
    FolderView {
        name: name.to_string(),
        kind,
        unread,
    }
}

fn folder_title(kind: FolderKind) -> &'static str {
    match kind {
        FolderKind::All => "All inboxes",
        FolderKind::Inbox => "Inbox",
        FolderKind::Starred => "Starred",
        FolderKind::Sent => "Sent",
        FolderKind::Drafts => "Drafts",
        FolderKind::Outbox => "Outbox",
        FolderKind::Archive => "Archive",
        FolderKind::Spam => "Spam",
        FolderKind::Trash => "Trash",
        FolderKind::Custom => "Mail",
        FolderKind::AddAccount => "Add account",
        FolderKind::Settings => "Settings",
    }
}

fn placeholder_rows() -> Vec<ListRow> {
    let palette = theme::active();
    vec![
        ListRow {
            initials: "AL".into(),
            avatar_color: palette.primary_container,
            sender: "Ada Lovelace".into(),
            date: "09:41".into(),
            subject: "Notes on the analytical engine".into(),
            snippet: "The engine might compose elaborate music, if asked.".into(),
            unread: true,
            has_attachment: true,
            count: 3,
        },
        ListRow {
            initials: "GH".into(),
            avatar_color: palette.secondary_container,
            sender: "Grace Hopper".into(),
            date: "Yesterday".into(),
            subject: "Compiler design review".into(),
            snippet: "The draft is ready for a first pass this afternoon.".into(),
            unread: true,
            has_attachment: false,
            count: 1,
        },
        ListRow {
            initials: "MA".into(),
            avatar_color: palette.surface_container_highest,
            sender: "Margaret Hamilton".into(),
            date: "Mon".into(),
            subject: "Apollo guidance notes".into(),
            snippet: "Attaching the checklist we discussed on Friday.".into(),
            unread: false,
            has_attachment: true,
            count: 1,
        },
    ]
}

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

/// The Back button, from `MainActivity.onBackPressed`. The shell has a single
/// window; the Android bridge expands this when the mail screens land. Adapted
/// from qlaunch's `Java_bitshift_qlaunch_MainActivity_onBackPressedEvent`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_bitshift_pige_MainActivity_onBackPressedEvent(
    _env: jni::JNIEnv,
    _this: jni::objects::JObject,
) {
    log::info!("back pressed (no-op)");
}

#[cfg(test)]
mod tests {
    use super::*;

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
            app.size = Size::new(width, 800.0);
            let mut element = app.view(window::Id::unique());
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 800.0)),
            );
            assert_eq!(node.size().width, width);
            assert_eq!(node.size().height, 800.0);
        }

        // The compact drawer overlays the list without changing the window.
        let (mut app, _) = App::new();
        app.size = Size::new(600.0, 800.0);
        app.drawer_open = true;
        let mut element = app.view(window::Id::unique());
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(600.0, 800.0)),
        );
        assert_eq!(node.size().width, 600.0);
    }
}
