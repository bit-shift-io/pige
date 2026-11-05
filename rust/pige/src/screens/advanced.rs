//! The Advanced screen: back up the configuration, restore one, or reset.
//!
//! Pure layout over shared components; the actual work runs through `Task`s in
//! `update`. Modelled on qlaunch's `screens/advanced.rs` (bitshift, MIT), with
//! only the rows pige has.

use iced::widget::{column, container, scrollable};
use iced::{Element, Fill};
use pige_core::api::ConfigOutcome;

use crate::app::App;
use crate::components::{self, ButtonVariant};
use crate::icons;
use crate::message::Message;
use crate::theme;

/// One action the Advanced screen offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Backup,
    Restore,
    FactoryReset,
}

impl Row {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Row::Backup => "Backup config",
            Row::Restore => "Restore config",
            Row::FactoryReset => "Factory Reset",
        }
    }

    pub(crate) fn subtitle(self) -> &'static str {
        match self {
            Row::Backup => "Saves accounts and settings to a dated file in Downloads. Passwords are not included.",
            Row::Restore => "Replaces accounts and settings with a backup from Downloads.",
            Row::FactoryReset => "Wipes every account, setting and cached message, after confirmation.",
        }
    }

    pub(crate) fn icon(self) -> char {
        match self {
            Row::Backup => icons::BACKUP,
            Row::Restore => icons::RESTORE,
            Row::FactoryReset => icons::DELETE_FOREVER,
        }
    }
}

/// The rows in display order.
pub(crate) fn rows() -> Vec<Row> {
    vec![Row::Backup, Row::Restore, Row::FactoryReset]
}

/// Shown when Restore finds no backup files.
pub(crate) const NO_BACKUPS: &str = "No backups found in Downloads.";

/// Shown after sending the user to the system "All files access" screen, so a
/// second press of Restore lists the backups the previous install wrote.
pub(crate) const NEEDS_ALL_FILES_ACCESS: &str =
    "pige needs \"All files access\" to read backups from Downloads. Enable it, then press Restore again.";

/// The toast copy for a finished backup.
pub(crate) fn backup_message(result: &Result<String, String>) -> String {
    match result {
        Ok(name) => format!("Backup saved to Downloads/{name}."),
        Err(error) => format!("Backup failed: {error}"),
    }
}

/// The toast copy for a finished restore. Success names the file and reminds
/// the user that passwords are not carried in a backup.
pub(crate) fn restore_message(result: &Result<ConfigOutcome, String>, name: &str) -> String {
    match result {
        Ok(outcome) => {
            let mut message = format!(
                "Config restored from {name}. Re-enter passwords for any accounts that need them."
            );
            for warning in &outcome.warnings {
                message.push(' ');
                message.push_str(warning);
            }
            message
        }
        Err(error) => format!("Could not restore: {error}"),
    }
}

/// The full-window Advanced screen.
pub(crate) fn view(_app: &App) -> Element<'_, Message> {
    let palette = theme::active();
    let leading = Some(components::icon_button(
        icons::ARROW_BACK,
        "Back to settings",
        Message::CloseAdvanced,
    ));

    let mut body = column![]
        .spacing(palette.space.md)
        .width(Fill)
        .max_width(palette.metrics.dialog_width);
    for row in rows() {
        body = body.push(components::action_card(
            row.label(),
            row.subtitle(),
            row.icon(),
            Message::Advanced(row),
        ));
    }

    column![
        components::app_bar("Advanced", leading, vec![]),
        scrollable(container(body).width(Fill).padding(palette.space.xl))
            .width(Fill)
            .height(Fill)
            .style(palette.scrollable()),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill)
    .into()
}

/// The restore picker: one button per backup name, newest first.
pub(crate) fn restore_picker_modal(names: &[String]) -> Element<'_, Message> {
    let palette = theme::active();
    let mut list = column![].spacing(palette.space.sm).width(Fill);
    for name in names {
        list = list.push(components::button(
            name,
            ButtonVariant::Text,
            Message::RestoreChosen(name.clone()),
        ));
    }
    components::dialog(
        "Restore config",
        list.into(),
        vec![components::button(
            "Cancel",
            ButtonVariant::Text,
            Message::RestoreCancelled,
        )],
    )
}

/// The restore confirmation: choosing a backup asks first, as qlaunch does.
pub(crate) fn restore_confirm_modal(name: &str) -> Element<'static, Message> {
    let body = format!("This replaces your current accounts and settings with {name}.");
    components::confirm(
        "Restore config",
        &body,
        "Restore",
        false,
        Message::RestoreConfirmed,
        Message::RestoreCancelled,
    )
}

/// The factory-reset confirmation.
pub(crate) fn reset_confirm_modal() -> Element<'static, Message> {
    components::confirm(
        "Factory Reset",
        "This wipes every account, setting and cached message. It cannot be undone.",
        "Reset",
        true,
        Message::ResetConfirmed,
        Message::ResetCancelled,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pige_core::api::ConfigOutcome;

    #[test]
    fn rows_are_backup_restore_reset() {
        assert_eq!(rows(), vec![Row::Backup, Row::Restore, Row::FactoryReset]);
    }

    #[test]
    fn no_backups_copy_is_defined() {
        assert_eq!(NO_BACKUPS, "No backups found in Downloads.");
    }

    #[test]
    fn backup_message_names_the_downloads_file() {
        assert_eq!(
            backup_message(&Ok("pige-backup-2026.json".to_string())),
            "Backup saved to Downloads/pige-backup-2026.json."
        );
        assert_eq!(
            backup_message(&Err("disk full".to_string())),
            "Backup failed: disk full"
        );
    }

    #[test]
    fn restore_message_names_the_file_and_password_step() {
        let outcome = ConfigOutcome {
            snapshot: Default::default(),
            settings: Default::default(),
            warnings: Vec::new(),
        };
        let message = restore_message(&Ok(outcome), "a.json");
        assert!(message.contains("Config restored from a.json."), "{message}");
        assert!(message.to_lowercase().contains("password"), "{message}");

        assert_eq!(
            restore_message(&Err("bad file".to_string()), "a.json"),
            "Could not restore: bad file"
        );
    }
}
