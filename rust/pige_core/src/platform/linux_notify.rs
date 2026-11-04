//! Linux desktop notifications through the freedesktop notification daemon.
//!
//! `notify-rust` speaks the D-Bus `org.freedesktop.Notifications` protocol, so
//! the same code works under GNOME, KDE, and any other session that implements
//! it. A missing daemon is not fatal: it is logged and mail still syncs.

use crate::platform::Notifier;
use crate::sync::NewMailSummary;

/// Posts new-mail notifications. Deliberately carries only the sender and the
/// subject; message bodies are never read here, so no private text can reach the
/// notification daemon or its history.
#[derive(Debug, Default)]
pub struct LinuxNotifier;

impl LinuxNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Notifier for LinuxNotifier {
    fn notify(&self, mail: &NewMailSummary) {
        let summary = if mail.from.trim().is_empty() {
            "New mail".to_string()
        } else {
            mail.from.clone()
        };
        let body = if mail.subject.trim().is_empty() {
            "(no subject)".to_string()
        } else {
            mail.subject.clone()
        };

        if let Err(error) = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .show()
        {
            log::warn!("post Linux notification: {error}");
        }
    }
}
