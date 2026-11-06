//! Account onboarding: the account list and the add/edit form.
//!
//! The screen is pure state and layout. Network work (validation, the test
//! connection, save, sync, and removal) runs through `Task::perform` in
//! `update`, so the iced UI thread never waits on DNS, TLS, or SQLite.

use std::collections::HashMap;

use iced::widget::{column, container, row, scrollable, stack};
use iced::{Alignment, Element, Fill};
use pige_core::account::{
    Account, AccountId, Security,
    presets::{ProviderPreset, detect, preset},
};
use pige_core::api::{AccountInput, ModelSnapshot};

use crate::app::App;
use crate::components::{self, ButtonVariant};
use crate::icons;
use crate::message::Message;
use crate::theme::{self, TextRole};

/// Which form field changed. Keeping the changes in one enum avoids a message
/// variant per field while still letting `update` know whether to re-derive the
/// username or reset the provider choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountField {
    Label,
    Email,
    DisplayName,
    Username,
    Password,
    ImapHost,
    ImapPort,
    SmtpHost,
    SmtpPort,
}

/// The result of the last connection test.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TestState {
    #[default]
    Idle,
    Testing,
    Passed,
    Failed(String),
}

/// Form data for adding or editing one account.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountForm {
    pub editing: Option<AccountId>,
    pub label: String,
    pub email: String,
    pub display_name: String,
    pub username: String,
    pub password: String,
    pub preset: ProviderPreset,
    pub imap_host: String,
    pub imap_port: String,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_security: Security,
    pub test: TestState,
    pub busy: bool,
}

impl std::fmt::Debug for AccountForm {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountForm")
            .field("editing", &self.editing)
            .field("label", &self.label)
            .field("email", &self.email)
            .field("display_name", &self.display_name)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("preset", &self.preset)
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("imap_security", &self.imap_security)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_security", &self.smtp_security)
            .field("test", &self.test)
            .field("busy", &self.busy)
            .finish()
    }
}

impl AccountForm {
    /// A new account starts on the primary v1 provider, Gmail, with a friendly
    /// default label the user can overwrite.
    pub fn new() -> Self {
        let defaults = preset(ProviderPreset::Gmail);
        Self {
            editing: None,
            label: "Personal".to_string(),
            email: String::new(),
            display_name: String::new(),
            username: String::new(),
            password: String::new(),
            preset: ProviderPreset::Gmail,
            imap_host: defaults.imap_host.to_string(),
            imap_port: defaults.imap_port.to_string(),
            imap_security: defaults.security,
            smtp_host: defaults.smtp_host.to_string(),
            smtp_port: defaults.smtp_port.to_string(),
            smtp_security: defaults.security,
            test: TestState::Idle,
            busy: false,
        }
    }

    /// Edit an existing account. The password is never read back from the
    /// keychain; the user re-enters it to test and save.
    pub fn from_account(account: &Account) -> Self {
        Self {
            editing: Some(account.id.clone()),
            label: account.label.clone(),
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            username: account.username.clone(),
            password: String::new(),
            preset: detect(&account.imap.host, &account.smtp.host),
            imap_host: account.imap.host.clone(),
            imap_port: account.imap.port.to_string(),
            imap_security: account.imap.security,
            smtp_host: account.smtp.host.clone(),
            smtp_port: account.smtp.port.to_string(),
            smtp_security: account.smtp.security,
            test: TestState::Idle,
            busy: false,
        }
    }

    pub fn apply_field(&mut self, field: AccountField, value: String) {
        match field {
            AccountField::Label => self.label = value,
            AccountField::Email => self.email = value,
            AccountField::DisplayName => self.display_name = value,
            AccountField::Username => self.username = value,
            AccountField::Password => self.password = value,
            AccountField::ImapHost => {
                self.imap_host = value;
                self.preset = detect(&self.imap_host, &self.smtp_host);
            }
            AccountField::ImapPort => self.imap_port = value,
            AccountField::SmtpHost => {
                self.smtp_host = value;
                self.preset = detect(&self.imap_host, &self.smtp_host);
            }
            AccountField::SmtpPort => self.smtp_port = value,
        }
        self.test = TestState::Idle;
    }

    /// Fill the server fields from a provider preset. Manual clears them so the
    /// form never claims an endpoint the user has not supplied.
    pub fn choose_preset(&mut self, name: ProviderPreset) {
        self.preset = name;
        let defaults = preset(name);
        if name == ProviderPreset::Manual {
            self.imap_host.clear();
            self.imap_port.clear();
            self.smtp_host.clear();
            self.smtp_port.clear();
        } else {
            self.imap_host = defaults.imap_host.to_string();
            self.imap_port = defaults.imap_port.to_string();
            self.smtp_host = defaults.smtp_host.to_string();
            self.smtp_port = defaults.smtp_port.to_string();
        }
        self.imap_security = defaults.security;
        self.smtp_security = defaults.security;
        self.test = TestState::Idle;
    }

    /// Convert the form into the API's account input. Ports are parsed here so
    /// a typo produces a field-level error before any network call.
    pub fn to_input(&self) -> Result<AccountInput, String> {
        let imap_port = parse_port("IMAP", &self.imap_port)?;
        let smtp_port = parse_port("SMTP", &self.smtp_port)?;
        let email = self.email.trim().to_string();
        let username = if self.username.trim().is_empty() {
            email.clone()
        } else {
            self.username.trim().to_string()
        };

        Ok(AccountInput {
            label: self.label.trim().to_string(),
            email,
            display_name: self.display_name.trim().to_string(),
            username,
            password: self.password.clone(),
            imap: pige_core::account::ServerConfig {
                host: self.imap_host.trim().to_string(),
                port: imap_port,
                security: self.imap_security,
            },
            smtp: pige_core::account::ServerConfig {
                host: self.smtp_host.trim().to_string(),
                port: smtp_port,
                security: self.smtp_security,
            },
        })
    }
}

impl Default for AccountForm {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_port(name: &str, value: &str) -> Result<u16, String> {
    value
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or_else(|| format!("{name} port must be a number between 1 and 65535"))
}

/// Everything the account screen remembers between messages.
#[derive(Debug, Default)]
pub struct AccountsState {
    /// The open add/edit form, if any.
    pub form: Option<AccountForm>,
    /// The account awaiting remove confirmation.
    pub confirm_remove: Option<AccountId>,
    /// An inline label draft per account, so the list can edit labels without
    /// needing the full account record.
    pub label_drafts: HashMap<AccountId, String>,
    /// A persistent (non-snackbar) note, such as "Account removed".
    pub notice: Option<String>,
}

impl AccountsState {
    /// Prepare the list for a freshly opened screen. Existing label edits are
    /// discarded because the snapshot is authoritative.
    pub fn open(&mut self, snapshot: &ModelSnapshot) {
        self.form = None;
        self.confirm_remove = None;
        self.label_drafts = snapshot
            .accounts
            .iter()
            .map(|account| (account.id.clone(), account.label.clone()))
            .collect();
    }

    pub fn start_new(&mut self) {
        self.form = Some(AccountForm::new());
        self.confirm_remove = None;
        self.notice = None;
    }

    pub fn edit(&mut self, account: &Account) {
        self.form = Some(AccountForm::from_account(account));
        self.confirm_remove = None;
        self.notice = None;
    }

    pub fn close_form(&mut self) {
        self.form = None;
    }
}

/// The full-window account screen.
pub fn screen<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let leading = Some(components::icon_button(
        icons::ARROW_BACK,
        "Back to mail",
        Message::CloseAccounts,
    ));
    let actions = if app.accounts.form.is_none() {
        vec![components::icon_button(
            icons::ADD,
            "Add account",
            Message::AccountNew,
        )]
    } else {
        Vec::new()
    };

    let mut page = column![components::app_bar("Accounts", leading, actions)]
        .spacing(0)
        .width(Fill)
        .height(Fill);

    if let Some(notice) = &app.accounts.notice {
        page = page.push(notice_bar(notice));
    }

    let body: Element<'a, Message> = if let Some(form) = &app.accounts.form {
        form_view(form)
    } else {
        list_view(app)
    };

    page = page.push(
        scrollable(body)
            .width(Fill)
            .height(Fill)
            .style(palette.scrollable()),
    );

    let page: Element<'a, Message> = page.into();

    match &app.accounts.confirm_remove {
        Some(id) => {
            let label = app
                .snapshot
                .accounts
                .iter()
                .find(|account| &account.id == id)
                .map(|account| account.label.clone())
                .unwrap_or_else(|| "this account".to_string());
            let body = styled(
                format!(
                    "Remove {label}? Its stored password and cached mail will be deleted. This cannot be undone."
                ),
                TextRole::BodyMedium,
                palette.on_surface_variant,
            );
            let dialog = components::dialog(
                "Remove account",
                body.into(),
                vec![
                    components::button(
                        "Cancel",
                        ButtonVariant::Text,
                        Message::AccountRemoveCancelled,
                    ),
                    components::button(
                        "Remove",
                        ButtonVariant::Danger,
                        Message::AccountRemoveConfirmed(id.clone()),
                    ),
                ],
            );
            stack![page, dialog].into()
        }
        None => page,
    }
}

fn list_view<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    if app.snapshot.accounts.is_empty() {
        return components::empty_state(
            "No accounts yet",
            "Add your first mailbox to start syncing mail.",
        );
    }

    let mut list = column![].spacing(palette.space.md).width(Fill);
    for account in &app.snapshot.accounts {
        let label = app
            .accounts
            .label_drafts
            .get(&account.id)
            .unwrap_or(&account.label);

        let header = row![
            styled(
                account.email.clone(),
                TextRole::BodyMedium,
                palette.on_surface_variant,
            )
            .width(Fill),
            components::button(
                "Edit",
                ButtonVariant::Outlined,
                Message::AccountEdit(account.id.clone()),
            ),
            components::button(
                "Remove",
                ButtonVariant::Danger,
                Message::AccountRemove(account.id.clone()),
            ),
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center)
        .width(Fill);

        let label_row = row![
            components::text_field("Label", label, {
                let id = account.id.clone();
                move |value| Message::AccountLabelDraft(id.clone(), value)
            }),
            components::button(
                "Save label",
                ButtonVariant::Text,
                Message::AccountLabelCommit(account.id.clone()),
            ),
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::End)
        .width(Fill);

        list = list.push(
            container(column![label_row, header].spacing(palette.space.sm))
                .width(Fill)
                .padding(palette.space.lg)
                .style(move |_| container::Style {
                    background: Some(palette.surface_container.into()),
                    text_color: Some(palette.on_surface),
                    border: iced::border::rounded(palette.radius.md),
                    ..Default::default()
                }),
        );
    }
    list.into()
}

fn form_view<'a>(form: &'a AccountForm) -> Element<'a, Message> {
    let palette = theme::active();
    let title = if form.editing.is_some() {
        "Edit account"
    } else {
        "Add account"
    };

    let mut providers = row![].spacing(palette.space.sm).align_y(Alignment::Center);
    for choice in ProviderPreset::CHOICES {
        providers = providers.push(components::chip(
            choice.label(),
            form.preset == choice,
            Message::AccountPresetChanged(choice),
        ));
    }

    let mut fields = column![
        styled(title, TextRole::TitleMedium, palette.on_surface),
        components::text_field("Label", &form.label, |value| {
            Message::AccountFieldChanged(AccountField::Label, value)
        }),
        components::text_field("Email", &form.email, |value| {
            Message::AccountFieldChanged(AccountField::Email, value)
        }),
        components::text_field("Display name", &form.display_name, |value| {
            Message::AccountFieldChanged(AccountField::DisplayName, value)
        }),
        components::text_field("Username", &form.username, |value| {
            Message::AccountFieldChanged(AccountField::Username, value)
        }),
        components::password_field("Password", &form.password, |value| {
            Message::AccountPasswordChanged(crate::message::Password::new(value))
        }),
        styled(
            if form.editing.is_some() {
                "Re-enter the password to test and save this account."
            } else {
                "Use an app password where your provider requires one."
            },
            TextRole::LabelMedium,
            palette.on_surface_variant,
        ),
        styled("Provider", TextRole::LabelLarge, palette.on_surface),
        providers,
        styled(
            form.preset.hint(),
            TextRole::LabelMedium,
            palette.on_surface_variant
        ),
        styled("IMAP", TextRole::LabelLarge, palette.on_surface),
        components::text_field("Host", &form.imap_host, |value| {
            Message::AccountFieldChanged(AccountField::ImapHost, value)
        }),
        components::text_field("Port", &form.imap_port, |value| {
            Message::AccountFieldChanged(AccountField::ImapPort, value)
        }),
        security_row(
            "IMAP security",
            form.imap_security,
            Message::AccountImapSecurityChanged(Security::Tls),
            Message::AccountImapSecurityChanged(Security::StartTls),
        ),
        styled("SMTP", TextRole::LabelLarge, palette.on_surface),
        components::text_field("Host", &form.smtp_host, |value| {
            Message::AccountFieldChanged(AccountField::SmtpHost, value)
        }),
        components::text_field("Port", &form.smtp_port, |value| {
            Message::AccountFieldChanged(AccountField::SmtpPort, value)
        }),
        security_row(
            "SMTP security",
            form.smtp_security,
            Message::AccountSmtpSecurityChanged(Security::Tls),
            Message::AccountSmtpSecurityChanged(Security::StartTls),
        ),
    ]
    .spacing(palette.space.md)
    .width(Fill);

    match &form.test {
        TestState::Idle => {}
        TestState::Testing => {
            fields = fields.push(styled(
                "Testing connection…",
                TextRole::BodyMedium,
                palette.on_surface_variant,
            ));
        }
        TestState::Passed => {
            fields = fields.push(styled(
                "Connection works.",
                TextRole::BodyMedium,
                palette.primary,
            ));
        }
        TestState::Failed(error) => {
            fields = fields.push(styled(error.clone(), TextRole::BodyMedium, palette.error));
        }
    }

    let actions = if form.busy {
        row![styled(
            "Working…",
            TextRole::BodyMedium,
            palette.on_surface_variant
        )]
    } else {
        row![
            components::button(
                "Test connection",
                ButtonVariant::Outlined,
                Message::AccountTest,
            ),
            components::button("Cancel", ButtonVariant::Text, Message::AccountCancel),
            components::button(
                if form.editing.is_some() {
                    "Save changes"
                } else {
                    "Save account"
                },
                ButtonVariant::Filled,
                Message::AccountSave,
            ),
        ]
    };

    container(
        column![
            fields,
            actions.spacing(palette.space.sm).align_y(Alignment::Center)
        ]
        .spacing(palette.space.lg)
        .width(Fill)
        .max_width(palette.metrics.dialog_width),
    )
    .width(Fill)
    .padding(palette.space.xl)
    .into()
}

fn security_row<'a>(
    label: &str,
    current: Security,
    tls: Message,
    starttls: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    row![
        styled(label, TextRole::LabelMedium, palette.on_surface_variant),
        components::chip("TLS", current == Security::Tls, tls),
        components::chip("STARTTLS", current == Security::StartTls, starttls),
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center)
    .into()
}

fn notice_bar<'a>(message: &str) -> Element<'a, Message> {
    let palette = theme::active();
    container(styled(
        message.to_string(),
        TextRole::BodyMedium,
        palette.on_error_container,
    ))
    .width(Fill)
    .padding([palette.space.sm, palette.space.lg])
    .style(move |_| container::Style {
        background: Some(palette.error_container.into()),
        text_color: Some(palette.on_error_container),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AccountSaveResult;

    #[test]
    fn a_new_form_starts_with_gmail_defaults() {
        let form = AccountForm::new();
        assert_eq!(form.preset, ProviderPreset::Gmail);
        assert_eq!(form.imap_host, "imap.gmail.com");
        assert_eq!(form.smtp_port, "465");
        assert_eq!(form.label, "Personal");
        assert_eq!(form.test, TestState::Idle);
    }

    #[test]
    fn choosing_a_preset_fills_both_servers_and_security() {
        let mut form = AccountForm::new();
        form.choose_preset(ProviderPreset::Fastmail);
        assert_eq!(form.imap_host, "imap.fastmail.com");
        assert_eq!(form.smtp_host, "smtp.fastmail.com");
        assert_eq!(form.imap_security, Security::Tls);

        form.choose_preset(ProviderPreset::Manual);
        assert!(form.imap_host.is_empty());
        assert!(form.imap_port.is_empty());
        assert!(form.smtp_host.is_empty());
    }

    #[test]
    fn editing_an_account_preserves_identity_and_never_reads_a_password_back() {
        let account = Account {
            id: AccountId("account-1".into()),
            label: "Work".into(),
            email: "grace@example.com".into(),
            display_name: "Grace".into(),
            username: "grace@example.com".into(),
            imap: pige_core::account::ServerConfig {
                host: "imap.fastmail.com".into(),
                port: 993,
                security: Security::Tls,
            },
            smtp: pige_core::account::ServerConfig {
                host: "smtp.fastmail.com".into(),
                port: 465,
                security: Security::Tls,
            },
            auth: pige_core::account::AuthKind::Password,
            sort_order: 0,
        };

        let form = AccountForm::from_account(&account);
        assert_eq!(form.editing, Some(AccountId("account-1".into())));
        assert_eq!(form.preset, ProviderPreset::Fastmail);
        assert!(form.password.is_empty());
    }

    #[test]
    fn form_ports_parse_and_bad_ports_fail_before_any_network_call() {
        let mut form = AccountForm::new();
        form.email = "ada@example.com".to_string();
        form.username = "ada@example.com".to_string();
        form.password = "secret".to_string();
        let input = form.to_input().expect("valid form");
        assert_eq!(input.imap.port, 993);

        form.imap_port = "nope".to_string();
        let error = form.to_input().expect_err("bad port");
        assert!(error.contains("IMAP port"));

        form.imap_port = "0".to_string();
        assert!(form.to_input().is_err());
    }

    #[test]
    fn an_empty_username_defaults_to_the_email() {
        let mut form = AccountForm::new();
        form.email = "ada@example.com".to_string();
        form.password = "secret".to_string();
        let input = form.to_input().expect("valid form");
        assert_eq!(input.username, "ada@example.com");
    }

    #[test]
    fn save_result_keeps_the_snapshot_and_optional_session_warning() {
        let result = AccountSaveResult {
            snapshot: ModelSnapshot::default(),
            warning: Some("session only".to_string()),
            account_id: AccountId("acc".into()),
        };
        assert!(result.warning.as_deref() == Some("session only"));
    }

    #[test]
    fn state_open_populates_label_drafts() {
        let snapshot = ModelSnapshot {
            accounts: vec![pige_core::api::AccountSummary {
                id: AccountId("a".into()),
                label: "Personal".into(),
                email: "ada@example.com".into(),
                unread: 0,
            }],
            ..ModelSnapshot::default()
        };
        let mut state = AccountsState::default();
        state.open(&snapshot);
        assert_eq!(
            state
                .label_drafts
                .get(&AccountId("a".into()))
                .map(String::as_str),
            Some("Personal")
        );
    }
}
