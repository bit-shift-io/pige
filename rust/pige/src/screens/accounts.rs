//! Account onboarding: the account list and the add/edit form.
//!
//! The screen is pure state and layout. Network work (validation, the test
//! connection, save, sync, and removal) runs through `Task::perform` in
//! `update`, so the iced UI thread never waits on DNS, TLS, or SQLite.

use std::collections::HashMap;

use iced::widget::{button, column, container, row, space, stack};
use iced::{Alignment, Element, Fill};
use pige_core::account::{
    ACCOUNT_COLORS, Account, AccountId, Security,
    presets::{ProviderPreset, detect, preset},
};
use pige_core::api::{AccountInput, AccountSummary, ModelSnapshot};

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

/// How a new account authenticates. Native Google is deliberately separate from
/// the existing Gmail-over-IMAP preset: an existing Gmail IMAP account keeps
/// editing its servers until the user explicitly switches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountMethod {
    Google,
    Imap,
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
    pub method: AccountMethod,
    /// True when editing an account that has no IMAP endpoints (native Gmail).
    /// Such an account can only reconnect through Google sign-in.
    pub native: bool,
    /// The user's optional Google People opt-in for a new or re-authenticated
    /// Google account.
    pub google_contacts: bool,
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
            .field("method", &self.method)
            .field("native", &self.native)
            .field("google_contacts", &self.google_contacts)
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
            method: AccountMethod::Imap,
            native: false,
            google_contacts: false,
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
    ///
    /// The form edits IMAP/SMTP accounts only. A native account (Gmail or
    /// Proton) has no endpoints, so the form is left with empty, manual fields
    /// rather than inventing a server for it; the API also refuses to convert a
    /// native backend through this form.
    pub fn from_account(account: &Account) -> Self {
        let imap_smtp = account.imap_smtp();
        let (username, imap_host, imap_port, imap_security, smtp_host, smtp_port, smtp_security) =
            match imap_smtp {
                Some((username, imap, smtp, _auth)) => (
                    username.to_string(),
                    imap.host.clone(),
                    imap.port.to_string(),
                    imap.security,
                    smtp.host.clone(),
                    smtp.port.to_string(),
                    smtp.security,
                ),
                None => (
                    String::new(),
                    String::new(),
                    String::new(),
                    Security::Tls,
                    String::new(),
                    String::new(),
                    Security::Tls,
                ),
            };
        let preset = match imap_smtp {
            Some(_) => detect(&imap_host, &smtp_host),
            None => ProviderPreset::Manual,
        };
        let native = imap_smtp.is_none();
        let google_contacts = account.contact_sources.iter().any(|source| {
            source.id.as_str() == pige_core::account::GOOGLE_SAVED_CONTACTS_SOURCE_ID
                && source.enabled
        });
        Self {
            editing: Some(account.id.clone()),
            method: if native {
                AccountMethod::Google
            } else {
                AccountMethod::Imap
            },
            native,
            google_contacts,
            label: account.label.clone(),
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            username,
            password: String::new(),
            preset,
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
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

    /// Select the native Google or manual IMAP/SMTP path. Switching to IMAP
    /// leaves whatever server fields were last shown; the form still requires
    /// the user to save an IMAP account explicitly.
    ///
    /// An existing IMAP/SMTP account is never converted through the method
    /// chips: a native Google account has no endpoints to edit, and an IMAP
    /// edit keeps its backend. Conversion is the separate, gated
    /// `Switch to native Google` action so `can_switch_backend` and the
    /// `gmail.com` restriction always run first.
    pub fn choose_method(&mut self, method: AccountMethod) {
        if self.native {
            self.method = AccountMethod::Google;
            return;
        }
        if self.editing.is_some() {
            self.method = AccountMethod::Imap;
            return;
        }
        self.method = method;
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
        self.start_new_with_google(false);
    }

    /// Start the add-account form. When this build registers native Google, the
    /// Google path is the default; manual IMAP stays one chip away.
    pub fn start_new_with_google(&mut self, google_available: bool) {
        let mut form = AccountForm::new();
        if google_available {
            form.choose_method(AccountMethod::Google);
        }
        self.form = Some(form);
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
        let google_available = app
            .snapshot
            .available_providers
            .iter()
            .any(|provider| provider == pige_core::providers::GMAIL_PROVIDER);
        form_view(form, google_available)
    } else {
        list_view(app)
    };

    page = page.push(components::scroller(body));

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
    let google_available = app
        .snapshot
        .available_providers
        .iter()
        .any(|provider| provider == pige_core::providers::GMAIL_PROVIDER);

    let mut list = column![].spacing(palette.space.md).width(Fill);
    for account in &app.snapshot.accounts {
        let label = app
            .accounts
            .label_drafts
            .get(&account.id)
            .unwrap_or(&account.label);
        let provider_label = match account.provider.as_str() {
            "gmail" => "Native Google",
            "imap-smtp" if looks_like_gmail(&account.email) => "Gmail over IMAP",
            "imap-smtp" => "IMAP/SMTP",
            other => other,
        };

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

        let provider_row = row![
            styled(provider_label, TextRole::LabelMedium, palette.on_surface),
            styled(
                account.unavailable_reason.clone().unwrap_or_default(),
                TextRole::LabelMedium,
                palette.error,
            )
            .width(Fill),
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center)
        .width(Fill);

        let mut google_actions = row![].spacing(palette.space.sm).align_y(Alignment::Center);
        if account.provider == pige_core::providers::GMAIL_PROVIDER && google_available {
            google_actions = google_actions.push(components::button(
                "Reconnect",
                ButtonVariant::Outlined,
                Message::GoogleAuth {
                    account: Some(account.id.clone()),
                    enable_contacts: false,
                    switch_backend: false,
                },
            ));
            let contacts_on = account.contact_sources.iter().any(|source| {
                source.source.as_str() == pige_core::account::GOOGLE_SAVED_CONTACTS_SOURCE_ID
                    && source.enabled
            });
            google_actions = google_actions.push(components::button(
                if contacts_on {
                    "Disable Google contacts"
                } else {
                    "Enable Google contacts"
                },
                ButtonVariant::Text,
                Message::GoogleContactsToggle(account.id.clone(), !contacts_on),
            ));
        } else if account.provider == pige_core::providers::IMAP_SMTP_PROVIDER
            && google_available
            && looks_like_gmail(&account.email)
        {
            google_actions = google_actions.push(components::button(
                "Switch to native Google",
                ButtonVariant::Outlined,
                Message::GoogleAuth {
                    account: Some(account.id.clone()),
                    enable_contacts: false,
                    switch_backend: true,
                },
            ));
        }

        list = list.push(
            container(
                column![
                    label_row,
                    header,
                    provider_row,
                    google_actions,
                    color_picker(account)
                ]
                .spacing(palette.space.sm),
            )
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

/// A row of palette swatches. The check mark marks the account's current
/// colour; picking another one persists through the API immediately.
fn color_picker<'a>(account: &'a AccountSummary) -> Element<'a, Message> {
    let palette = theme::active();
    let mut swatches = row![].spacing(palette.space.sm).align_y(Alignment::Center);
    for hex in ACCOUNT_COLORS {
        let selected = account.color == *hex;
        let color = theme::parse_hex_color(hex).unwrap_or(palette.primary);
        let mark: Element<'a, Message> = if selected {
            iced::widget::text(icons::glyph(icons::CHECK))
                .font(icons::FONT)
                .size(palette.metrics.small_icon_size)
                .color(palette.on_color(color))
                .into()
        } else {
            space().into()
        };
        let swatch = container(mark)
            .width(palette.metrics.icon_size + palette.space.xs)
            .height(palette.metrics.icon_size + palette.space.xs)
            .center_x(palette.metrics.icon_size + palette.space.xs)
            .center_y(palette.metrics.icon_size + palette.space.xs)
            .style(move |_| container::Style {
                background: Some(color.into()),
                border: iced::border::rounded((palette.metrics.icon_size + palette.space.xs) / 2.0),
                ..Default::default()
            });
        swatches = swatches.push(
            button(swatch)
                .padding(0)
                .style(|_, _| button::Style {
                    background: None,
                    ..Default::default()
                })
                .on_press(Message::SetAccountColor(
                    account.id.clone(),
                    (*hex).to_string(),
                )),
        );
    }

    row![
        styled("Colour", TextRole::LabelMedium, palette.on_surface_variant),
        swatches,
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center)
    .into()
}

fn form_view<'a>(form: &'a AccountForm, google_available: bool) -> Element<'a, Message> {
    let palette = theme::active();
    let title = if form.native {
        "Google account"
    } else if form.editing.is_some() {
        "Edit account"
    } else {
        "Add account"
    };
    let is_google = form.method == AccountMethod::Google;

    let mut fields = column![styled(title, TextRole::TitleMedium, palette.on_surface)]
        .spacing(palette.space.md)
        .width(Fill);

    if !form.native && form.editing.is_none() {
        let mut methods = row![].spacing(palette.space.sm).align_y(Alignment::Center);
        if google_available {
            methods = methods.push(components::chip(
                "Google account",
                is_google,
                Message::AccountMethodChanged(AccountMethod::Google),
            ));
        } else {
            methods = methods.push(styled(
                "Google account (not available in this build)",
                TextRole::LabelMedium,
                palette.on_surface_variant,
            ));
        }
        methods = methods.push(components::chip(
            "IMAP/SMTP",
            !is_google,
            Message::AccountMethodChanged(AccountMethod::Imap),
        ));
        fields = fields
            .push(styled(
                "Account type",
                TextRole::LabelLarge,
                palette.on_surface,
            ))
            .push(methods);
    }

    if is_google {
        fields = fields.push(styled(
            if form.native {
                "Reconnect this account with Google. No password is stored, and existing local mail stays cached."
            } else {
                "Sign in with Google in your browser. Pige stores no password and uses Gmail's native API instead of IMAP."
            },
            TextRole::BodyMedium,
            palette.on_surface_variant,
        ));
        if form.editing.is_some() {
            fields = fields.push(styled(
                format!("{} — {}", form.label, form.email),
                TextRole::LabelMedium,
                palette.on_surface,
            ));
        }
        if google_available {
            fields = fields.push(components::chip(
                "Sync Google contacts (optional)",
                form.google_contacts,
                Message::AccountGoogleContactsToggled(!form.google_contacts),
            ));
        }
        if !form.native {
            fields = fields
                .push(components::text_field("Label", &form.label, |value| {
                    Message::AccountFieldChanged(AccountField::Label, value)
                }))
                .push(components::text_field(
                    "Display name",
                    &form.display_name,
                    |value| Message::AccountFieldChanged(AccountField::DisplayName, value),
                ));
        }
    } else {
        let mut providers = row![].spacing(palette.space.sm).align_y(Alignment::Center);
        for choice in ProviderPreset::CHOICES {
            providers = providers.push(components::chip(
                choice.label(),
                form.preset == choice,
                Message::AccountPresetChanged(choice),
            ));
        }
        fields = fields
            .push(components::text_field("Label", &form.label, |value| {
                Message::AccountFieldChanged(AccountField::Label, value)
            }))
            .push(components::text_field("Email", &form.email, |value| {
                Message::AccountFieldChanged(AccountField::Email, value)
            }))
            .push(components::text_field(
                "Display name",
                &form.display_name,
                |value| Message::AccountFieldChanged(AccountField::DisplayName, value),
            ))
            .push(components::text_field(
                "Username",
                &form.username,
                |value| Message::AccountFieldChanged(AccountField::Username, value),
            ))
            .push(components::password_field(
                "Password",
                &form.password,
                |value| Message::AccountPasswordChanged(crate::message::Password::new(value)),
            ))
            .push(styled(
                if form.editing.is_some() {
                    "Re-enter the password to test and save this account."
                } else {
                    "Use an app password where your provider requires one."
                },
                TextRole::LabelMedium,
                palette.on_surface_variant,
            ))
            .push(styled(
                "IMAP provider",
                TextRole::LabelLarge,
                palette.on_surface,
            ))
            .push(providers)
            .push(styled(
                form.preset.hint(),
                TextRole::LabelMedium,
                palette.on_surface_variant,
            ))
            .push(styled("IMAP", TextRole::LabelLarge, palette.on_surface))
            .push(components::text_field("Host", &form.imap_host, |value| {
                Message::AccountFieldChanged(AccountField::ImapHost, value)
            }))
            .push(components::text_field("Port", &form.imap_port, |value| {
                Message::AccountFieldChanged(AccountField::ImapPort, value)
            }))
            .push(security_row(
                "IMAP security",
                form.imap_security,
                Message::AccountImapSecurityChanged(Security::Tls),
                Message::AccountImapSecurityChanged(Security::StartTls),
            ))
            .push(styled("SMTP", TextRole::LabelLarge, palette.on_surface))
            .push(components::text_field("Host", &form.smtp_host, |value| {
                Message::AccountFieldChanged(AccountField::SmtpHost, value)
            }))
            .push(components::text_field("Port", &form.smtp_port, |value| {
                Message::AccountFieldChanged(AccountField::SmtpPort, value)
            }))
            .push(security_row(
                "SMTP security",
                form.smtp_security,
                Message::AccountSmtpSecurityChanged(Security::Tls),
                Message::AccountSmtpSecurityChanged(Security::StartTls),
            ));

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
    }

    let actions = if form.busy {
        row![styled(
            "Working…",
            TextRole::BodyMedium,
            palette.on_surface_variant
        )]
    } else if is_google {
        let mut actions = row![components::button(
            "Cancel",
            ButtonVariant::Text,
            Message::AccountCancel
        )]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center);
        if google_available {
            actions = actions.push(components::button(
                if form.editing.is_some() {
                    "Reconnect with Google"
                } else {
                    "Sign in with Google"
                },
                ButtonVariant::Filled,
                Message::GoogleAuth {
                    account: form.editing.clone(),
                    enable_contacts: form.google_contacts,
                    // A native account reconnects in place. If this ever renders
                    // for an IMAP edit, it must take the explicit, pre-flighted
                    // switch path rather than silently converting the backend.
                    switch_backend: form.editing.is_some() && !form.native,
                },
            ));
        }
        actions
    } else {
        let mut actions = row![
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
        .spacing(palette.space.sm)
        .align_y(Alignment::Center);
        if form.editing.is_some() && google_available && looks_like_gmail(&form.email) {
            actions = actions.push(components::button(
                "Switch to native Google",
                ButtonVariant::Outlined,
                Message::GoogleAuth {
                    account: form.editing.clone(),
                    enable_contacts: form.google_contacts,
                    switch_backend: true,
                },
            ));
        }
        actions
    };

    container(
        column![fields, actions]
            .spacing(palette.space.lg)
            .width(Fill)
            .max_width(palette.metrics.dialog_width),
    )
    .width(Fill)
    .padding(palette.space.xl)
    .into()
}

fn looks_like_gmail(email: &str) -> bool {
    let domain = email.rsplit('@').next().unwrap_or_default().to_lowercase();
    domain == "gmail.com" || domain == "googlemail.com"
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
    fn google_onboarding_is_separate_from_the_gmail_imap_preset() {
        let mut form = AccountForm::new();
        form.choose_method(AccountMethod::Google);
        assert_eq!(form.method, AccountMethod::Google);
        // The Gmail preset is still IMAP; selecting Google never invents a
        // password requirement or reuses the IMAP server fields.
        form.choose_preset(ProviderPreset::Gmail);
        assert_eq!(form.method, AccountMethod::Google);
        assert_eq!(form.imap_host, "imap.gmail.com");
    }

    #[test]
    fn an_existing_gmail_imap_account_still_edits_as_imap() {
        let account = Account {
            id: AccountId("imap-gmail".into()),
            label: "Old Gmail".into(),
            email: "person@gmail.com".into(),
            display_name: "Person".into(),
            backend: pige_core::account::MailBackend::ImapSmtp {
                username: "person@gmail.com".into(),
                imap: pige_core::account::ServerConfig {
                    host: "imap.gmail.com".into(),
                    port: 993,
                    security: Security::Tls,
                },
                smtp: pige_core::account::ServerConfig {
                    host: "smtp.gmail.com".into(),
                    port: 465,
                    security: Security::Tls,
                },
                auth: pige_core::account::AuthKind::Password,
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        let form = AccountForm::from_account(&account);
        assert_eq!(form.method, AccountMethod::Imap);
        assert!(!form.native);
        assert_eq!(form.imap_host, "imap.gmail.com");
        assert_eq!(form.smtp_host, "smtp.gmail.com");
        assert!(looks_like_gmail(&form.email));
    }

    #[test]
    fn a_native_google_account_cannot_be_saved_as_imap() {
        let account = Account {
            id: AccountId("native".into()),
            label: "Google".into(),
            email: "person@example.com".into(),
            display_name: "Person".into(),
            backend: pige_core::account::MailBackend::gmail("google-user"),
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        let mut form = AccountForm::from_account(&account);
        assert_eq!(form.method, AccountMethod::Google);
        assert!(form.native);
        form.choose_method(AccountMethod::Imap);
        assert_eq!(
            form.method,
            AccountMethod::Google,
            "a native account has no IMAP endpoints to edit"
        );
    }

    #[test]
    fn disabled_google_contacts_stay_off_across_edit_and_reconnect() {
        let mut account = Account {
            id: AccountId("native".into()),
            label: "Google".into(),
            email: "person@example.com".into(),
            display_name: "Person".into(),
            backend: pige_core::account::MailBackend::gmail("google-user"),
            contact_sources: pige_core::account::google_contact_sources(),
            sort_order: 0,
            color: None,
        };
        for source in &mut account.contact_sources {
            source.enabled = false;
        }

        let form = AccountForm::from_account(&account);
        assert!(form.native);
        assert_eq!(form.method, AccountMethod::Google);
        assert!(
            !form.google_contacts,
            "a disabled Google contact source must not become an opt-in when the edit form reconnects"
        );
    }

    #[test]
    fn an_imap_edit_cannot_choose_google_without_the_gated_switch() {
        let account = Account {
            id: AccountId("imap-gmail".into()),
            label: "Old Gmail".into(),
            email: "person@gmail.com".into(),
            display_name: "Person".into(),
            backend: pige_core::account::MailBackend::ImapSmtp {
                username: "person@gmail.com".into(),
                imap: pige_core::account::ServerConfig {
                    host: "imap.gmail.com".into(),
                    port: 993,
                    security: Security::Tls,
                },
                smtp: pige_core::account::ServerConfig {
                    host: "smtp.gmail.com".into(),
                    port: 465,
                    security: Security::Tls,
                },
                auth: pige_core::account::AuthKind::Password,
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        };
        let mut form = AccountForm::from_account(&account);
        form.choose_method(AccountMethod::Google);
        assert_eq!(
            form.method,
            AccountMethod::Imap,
            "an IMAP edit must use the explicit switch action, not a silent Google reconnect"
        );
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
            backend: pige_core::account::MailBackend::ImapSmtp {
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
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
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
                color: "#00897b".into(),
                learning_paused: false,
                contact_sources: Vec::new(),
                provider: "imap-smtp".into(),
                available: true,
                unavailable_reason: None,
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
