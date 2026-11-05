//! The full-window settings screen: sync interval, notifications, theme, and
//! account management.
//!
//! Like the account screen this is pure layout over [`App`] state. Persisting
//! and rescheduling happen through `Task`/message handling in `update`; nothing
//! here touches SQLite or the network.

use iced::widget::{column, container, row, scrollable};
use iced::{Alignment, Element, Fill};
use pige_core::settings::ThemeMode;

use crate::app::App;
use crate::components::{self, ButtonVariant};
use crate::icons;
use crate::message::Message;
use crate::theme::{self, TextRole};

/// The full-window settings screen.
pub fn screen<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let leading = Some(components::icon_button(
        icons::ARROW_BACK,
        "Back to mail",
        Message::CloseSettings,
    ));
    let actions = vec![components::button(
        "Save",
        ButtonVariant::Filled,
        Message::SettingsSave,
    )];

    let body = column![
        sync_section(app),
        background_sync_section(app),
        notifications_section(app),
        theme_section(app),
        accounts_section(app),
        components::action_card(
            "Advanced",
            "Backup, restore and factory reset.",
            icons::SETTINGS,
            Message::OpenAdvanced,
        ),
    ]
    .spacing(palette.space.lg)
    .width(Fill)
    .max_width(palette.metrics.dialog_width);

    column![
        components::app_bar("Settings", leading, actions),
        scrollable(container(body).width(Fill).padding(palette.space.xl),)
            .width(Fill)
            .height(Fill)
            .style(palette.scrollable()),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill)
    .into()
}

fn sync_section<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let body = column![
        components::text_field(
            "Minutes between syncs",
            &app.settings_sync,
            Message::SettingsSyncChanged,
        ),
        styled(
            "1–60 minutes; 0 uses 15. Android schedules at a 15-minute minimum.".to_string(),
            TextRole::LabelMedium,
            palette.on_surface_variant,
        ),
    ]
    .spacing(palette.space.xs)
    .width(Fill);
    section("Sync", body.into())
}

fn background_sync_section<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let on = app.settings.background_sync;
    let body = column![
        row![
            components::chip("On", on, Message::SettingsBackgroundSyncToggled(true)),
            components::chip("Off", !on, Message::SettingsBackgroundSyncToggled(false)),
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center),
        styled(
            "Android schedules the interval above while this is on; desktop has no background scheduler.".to_string(),
            TextRole::LabelMedium,
            palette.on_surface_variant,
        ),
    ]
    .spacing(palette.space.xs)
    .width(Fill);
    section("Background sync", body.into())
}

fn notifications_section<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let on = app.settings.notifications;
    let body = row![
        components::chip("On", on, Message::SettingsNotificationsToggled(true),),
        components::chip("Off", !on, Message::SettingsNotificationsToggled(false),),
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center);
    section("Notifications", body.into())
}

fn theme_section<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let current = app.settings.theme;
    let body = row![
        components::chip(
            "System",
            current == ThemeMode::System,
            Message::SettingsThemeChanged(ThemeMode::System),
        ),
        components::chip(
            "Light",
            current == ThemeMode::Light,
            Message::SettingsThemeChanged(ThemeMode::Light),
        ),
        components::chip(
            "Dark",
            current == ThemeMode::Dark,
            Message::SettingsThemeChanged(ThemeMode::Dark),
        ),
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center);
    section("Theme", body.into())
}

fn accounts_section<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let mut list = column![].spacing(palette.space.md).width(Fill);

    if app.snapshot.accounts.is_empty() {
        list = list.push(styled(
            "No accounts yet.".to_string(),
            TextRole::BodyMedium,
            palette.on_surface_variant,
        ));
    }

    for account in &app.snapshot.accounts {
        let label = app
            .accounts
            .label_drafts
            .get(&account.id)
            .unwrap_or(&account.label);

        list = list.push(
            column![
                styled(
                    account.email.clone(),
                    TextRole::LabelMedium,
                    palette.on_surface_variant,
                ),
                row![
                    components::text_field("Label", label, {
                        let id = account.id.clone();
                        move |value| Message::AccountLabelDraft(id.clone(), value)
                    }),
                    components::button(
                        "Save",
                        ButtonVariant::Text,
                        Message::AccountLabelCommit(account.id.clone()),
                    ),
                    components::button(
                        "Remove",
                        ButtonVariant::Danger,
                        Message::AccountRemove(account.id.clone()),
                    ),
                ]
                .spacing(palette.space.sm)
                .align_y(Alignment::End)
                .width(Fill),
            ]
            .spacing(palette.space.sm)
            .width(Fill),
        );
    }

    list = list.push(components::button(
        "Add account",
        ButtonVariant::Outlined,
        Message::AddAccount,
    ));

    section("Accounts", list.into())
}

/// A titled card holding one group of settings.
fn section<'a>(title: &str, body: Element<'a, Message>) -> Element<'a, Message> {
    let palette = theme::active();
    container(
        column![
            styled(title.to_string(), TextRole::TitleSmall, palette.on_surface),
            body,
        ]
        .spacing(palette.space.md)
        .width(Fill),
    )
    .width(Fill)
    .padding(palette.space.lg)
    .style(move |_| container::Style {
        background: Some(palette.surface_container.into()),
        text_color: Some(palette.on_surface),
        border: iced::border::rounded(palette.radius.md),
        ..Default::default()
    })
    .into()
}

fn styled<'a>(
    value: impl Into<String>,
    role: TextRole,
    color: iced::Color,
) -> iced::widget::Text<'a> {
    components::styled_text(value.into(), theme::active().role(role), color)
}
