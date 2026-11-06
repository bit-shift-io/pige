//! Application state and the pure presentation helpers the shell builds on.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::window;
use iced::{Color, Size, Task};
use pige_core::account::AccountId;
use pige_core::api::{
    self, ConversationSummary as CoreConversationSummary, FolderSummary, ModelSnapshot,
};
use pige_core::mail::{FolderId, FolderKind as CoreFolderKind};
use pige_core::search::{SearchHit, SearchScope};
use pige_core::settings::Settings;

use crate::components::elastic_scroll::Motion;
use crate::components::{FolderKind, FolderView, ListRow};
use crate::message::Message;
use crate::screens::accounts::AccountsState;
use crate::screens::compose::ComposeState;
use crate::theme;

/// One folder row in an account group, carrying the id navigation needs to
/// select it without re-resolving the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionFolder {
    pub id: FolderId,
    pub view: FolderView,
}

/// One account group in the navigation rail, with its folders already resolved
/// to plain view data.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionView {
    pub account_id: AccountId,
    pub label: String,
    pub email: String,
    pub unread: u32,
    pub folders: Vec<SectionFolder>,
}

/// The shell's single brief-message channel. A generation guard means a late
/// expiry from an older toast cannot dismiss a newer one.
#[derive(Debug, Default)]
pub(crate) struct Toast {
    pub(crate) current: Option<(u64, String)>,
    generation: u64,
}

impl Toast {
    /// Show `message`, replacing any current one, and return the generation
    /// whose expiry is allowed to clear it.
    pub(crate) fn show(&mut self, message: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.current = Some((self.generation, message));
        self.generation
    }

    /// Clear the toast only if it still belongs to `generation`.
    pub(crate) fn expire(&mut self, generation: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            self.current = None;
        }
    }
}

/// Everything the shell remembers between messages.
pub struct App {
    /// The latest authoritative cache view.
    pub snapshot: ModelSnapshot,
    /// Whether the compact/medium navigation drawer is showing.
    pub drawer_open: bool,
    /// The account whose folder is selected, if any.
    pub selected_account: Option<AccountId>,
    /// Accounts whose folder groups are collapsed.
    pub collapsed: HashSet<AccountId>,
    pub(crate) search: String,
    /// The scope chip's current choice. Starts at All accounts and cycles
    /// through the selected account and folder when they exist.
    pub(crate) search_scope: SearchScope,
    /// The latest background search results.
    pub(crate) search_results: Vec<SearchHit>,
    pub(crate) size: Size,
    /// Android safe-area insets; zero on desktop.
    pub(crate) insets: iced_android::Insets,
    pub(crate) dark: bool,
    pub(crate) list_motion: Motion,
    /// The single shared brief-message channel (a small dark card at the
    /// bottom). Errors and results both flow through it.
    pub(crate) toast: Toast,
    /// Whether the reader's earlier-message section is expanded.
    pub(crate) reader_expanded: bool,
    pub(crate) spin_phase: u32,
    /// Whether the reader's "Move to folder" picker is open.
    pub(crate) move_picker_open: bool,
    /// Whether the reader's permanent-delete confirmation is open.
    pub(crate) delete_confirm_open: bool,
    /// Candidate backup names shown in the Advanced restore picker.
    pub(crate) restore_picker: Option<Vec<String>>,
    /// The backup name awaiting a restore confirmation.
    pub(crate) restore_confirm: Option<String>,
    /// Whether the Advanced factory-reset confirmation is open.
    pub(crate) confirm_reset: bool,
    /// Whether the full-window account screen is showing.
    pub accounts_open: bool,
    /// The account list/form/confirmation state.
    pub accounts: AccountsState,
    /// Whether the full-window settings screen is showing.
    pub settings_open: bool,
    /// Whether the Advanced screen is showing (opened from Settings).
    pub advanced_open: bool,
    /// The settings being edited. Persisted to the store on `SettingsSave`.
    pub(crate) settings: Settings,
    /// The raw sync-interval text while it is being edited.
    pub(crate) settings_sync: String,
    /// The open compose surface, if any.
    pub compose: Option<ComposeState>,
}

impl App {
    /// Build the shell around the process-wide cache. On Linux the core gets
    /// the Secret Service credential store, `xdg-open`, and real IMAP; other
    /// targets still use the in-memory adapters until their platform layer
    /// lands.
    pub fn new() -> (Self, Task<Message>) {
        let (_id, open) = window::open(window::Settings {
            size: Size::new(900.0, 700.0),
            ..Default::default()
        });
        let (snapshot, init_error) = initialize_api();
        let selected_account = selection_account(&snapshot);
        let has_accounts = !snapshot.accounts.is_empty();

        // Start from the defaults and let the persisted settings arrive through
        // `SettingsLoaded`; `App::new` must not block the runtime on SQLite.
        let settings = Settings::default();
        let settings_sync = settings.sync_minutes.to_string();
        // A `System` theme keeps the dark default until the OS reports its
        // preference; a persisted `Light`/`Dark` mode is applied on load.
        let dark = true;

        // Light system-bar icons against the dark shell. No-op off Android.
        iced_android::set_system_bars_dark(true);

        let mut startup = vec![
            open.map(|_| Message::Noop),
            Task::perform(async { api::settings() }, Message::SettingsLoaded),
        ];
        // Sync-on-open: kick a background sync through the normal message path
        // (so SyncStarted/Finished and errors behave as usual) once an account
        // exists. Window creation is never blocked on the network.
        if has_accounts {
            startup.push(crate::update::synced_with_prefetch(api::sync_all()));
        }
        // An initialization problem (for example an unreadable configuration)
        // is surfaced through the same toast channel as any other message.
        if let Some(error) = init_error {
            startup.push(Task::done(Message::Error(error)));
        }
        let startup = Task::batch(startup);

        (
            App {
                snapshot,
                drawer_open: false,
                selected_account,
                collapsed: HashSet::new(),
                search: String::new(),
                search_scope: SearchScope::AllAccounts,
                search_results: Vec::new(),
                size: Size::new(900.0, 700.0),
                insets: iced_android::Insets::default(),
                dark,
                list_motion: Motion::new(),
                toast: Toast::default(),
                reader_expanded: false,
                spin_phase: 0,
                move_picker_open: false,
                delete_confirm_open: false,
                restore_picker: None,
                restore_confirm: None,
                confirm_reset: false,
                accounts_open: false,
                accounts: AccountsState::default(),
                settings_open: false,
                advanced_open: false,
                settings,
                settings_sync,
                compose: None,
            },
            startup,
        )
    }

    /// A shell with default state for unit tests: no window and no API
    /// initialization, so nothing is read from or written to the real
    /// configuration or data directory.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            snapshot: ModelSnapshot::default(),
            drawer_open: false,
            selected_account: None,
            collapsed: HashSet::new(),
            search: String::new(),
            search_scope: SearchScope::AllAccounts,
            search_results: Vec::new(),
            size: Size::new(900.0, 700.0),
            insets: iced_android::Insets::default(),
            dark: true,
            list_motion: Motion::new(),
            toast: Toast::default(),
            reader_expanded: false,
            spin_phase: 0,
            move_picker_open: false,
            delete_confirm_open: false,
            restore_picker: None,
            restore_confirm: None,
            confirm_reset: false,
            accounts_open: false,
            accounts: AccountsState::default(),
            settings_open: false,
            advanced_open: false,
            settings: Settings::default(),
            settings_sync: Settings::default().sync_minutes.to_string(),
            compose: None,
        }
    }

    /// Install a fresh snapshot and derive the navigation selection in one
    /// place for both event-driven and tick refreshes.
    pub(crate) fn apply_snapshot(&mut self, snapshot: ModelSnapshot) {
        if snapshot.revision < self.snapshot.revision {
            return;
        }
        self.selected_account = selection_account(&snapshot);
        if snapshot_matches_search(&snapshot, &self.search, &self.search_scope) {
            self.search_results = snapshot.search_results.clone();
        }
        self.snapshot = snapshot;
    }
}

/// Whether a snapshot describes the current query and scope. Comparing the
/// trimmed query prevents old result sets from surviving whitespace edits or a
/// scope-chip change.
fn snapshot_matches_search(snapshot: &ModelSnapshot, query: &str, scope: &SearchScope) -> bool {
    let query = query.trim();
    !query.is_empty() && snapshot.search_query == query && &snapshot.search_scope == scope
}

/// Open the on-disk cache and return its first snapshot.
fn initialize_api() -> (ModelSnapshot, Option<String>) {
    if api::is_initialized() {
        return (api::snapshot(), None);
    }

    match api::init(crate::platform::config()) {
        // A quarantined or unreadable configuration is surfaced as a start-up
        // warning (it is emitted before the shell can subscribe to events, so
        // it is taken directly).
        Ok(()) => (api::snapshot(), api::take_startup_warning()),
        Err(error) => {
            log::error!("initialize pige api: {error}");
            (
                api::snapshot(),
                Some(format!("Could not open the local mail cache: {error}")),
            )
        }
    }
}

pub(crate) fn selection_account(snapshot: &ModelSnapshot) -> Option<AccountId> {
    if snapshot.unified_selected {
        return None;
    }
    snapshot
        .selected_folder
        .as_ref()
        .and_then(|folder| account_for_folder(snapshot, folder))
}

pub(crate) fn account_for_folder(snapshot: &ModelSnapshot, folder: &FolderId) -> Option<AccountId> {
    snapshot
        .sections
        .iter()
        .find(|section| section.folders.iter().any(|item| &item.id == folder))
        .map(|section| section.account_id.clone())
}

/// The first letter of the first and last words, uppercased.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return "?".to_string();
    };
    let last = words.last().copied().unwrap_or(first);

    let mut initials = String::new();
    if let Some(letter) = first_letter(first) {
        initials.extend(letter.to_uppercase());
    }
    if words.len() > 1
        && let Some(letter) = first_letter(last)
    {
        initials.extend(letter.to_uppercase());
    }

    if initials.is_empty() {
        "?".to_string()
    } else {
        initials
    }
}

fn first_letter(word: &str) -> Option<char> {
    word.chars()
        .find(|character| character.is_alphanumeric())
        .or_else(|| word.chars().next())
}

/// A stable avatar colour for an address, drawn from the active palette.
pub fn avatar_color(email: &str) -> Color {
    let palette = theme::active();
    let choices = [
        palette.primary_container,
        palette.secondary_container,
        palette.surface_container_highest,
        palette.error_container,
        palette.primary,
        palette.error,
    ];
    let index = (address_hash(email) % choices.len() as u64) as usize;
    choices[index]
}

/// FNV-1a: unlike `DefaultHasher`, this stays stable across Rust releases and
/// gives an address the same colour every launch.
fn address_hash(email: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in email.trim().to_ascii_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A short list-row date: time today, weekday this week, month/day this year,
/// and an ISO date for older mail.
pub fn format_list_date(now: i64, date: i64) -> String {
    if date <= 0 {
        return String::new();
    }

    let now_days = now.max(0).div_euclid(86_400);
    let date_days = date.div_euclid(86_400);
    let seconds = date.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(date_days);

    if date_days == now_days {
        return format!("{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60);
    }

    let (now_year, _, _) = civil_from_days(now_days);
    let days_ago = now_days - date_days;
    if (1..=6).contains(&days_ago) {
        return WEEKDAYS[weekday_index(date_days)].to_string();
    }
    if year == now_year {
        return format!("{} {day}", MONTHS[(month - 1) as usize]);
    }

    format!("{year:04}-{month:02}-{day:02}")
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// 0 = Sunday, from days since 1970-01-01 (which was a Thursday).
fn weekday_index(days: i64) -> usize {
    (days + 4).rem_euclid(7) as usize
}

/// Convert days since the Unix epoch to a proleptic Gregorian date.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Resolve the navigation sections, hiding folders in collapsed accounts while
/// keeping each account's roll-up visible.
pub fn sections(snapshot: &ModelSnapshot, collapsed: &HashSet<AccountId>) -> Vec<SectionView> {
    snapshot
        .sections
        .iter()
        .map(|section| SectionView {
            account_id: section.account_id.clone(),
            label: section.label.clone(),
            email: section.email.clone(),
            unread: section.unread,
            folders: if collapsed.contains(&section.account_id) {
                Vec::new()
            } else {
                section
                    .folders
                    .iter()
                    .map(|folder| SectionFolder {
                        id: folder.id.clone(),
                        view: folder_view(folder),
                    })
                    .collect()
            },
        })
        .collect()
}

fn folder_view(folder: &FolderSummary) -> FolderView {
    FolderView {
        name: folder.name.clone(),
        kind: match folder.kind {
            CoreFolderKind::Inbox => FolderKind::Inbox,
            CoreFolderKind::Sent => FolderKind::Sent,
            CoreFolderKind::Drafts => FolderKind::Drafts,
            CoreFolderKind::Trash => FolderKind::Trash,
            CoreFolderKind::Archive => FolderKind::Archive,
            CoreFolderKind::Junk => FolderKind::Spam,
            CoreFolderKind::Other => FolderKind::Custom,
        },
        unread: folder.unread,
    }
}

/// Turn one cached conversation into the plain row the list component draws.
pub fn row_view(conversation: &CoreConversationSummary) -> ListRow {
    let sender = if conversation.sender.trim().is_empty() {
        "Unknown sender".to_string()
    } else {
        conversation.sender.clone()
    };
    let subject = if conversation.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        conversation.subject.clone()
    };

    ListRow {
        initials: initials(&sender),
        avatar_color: avatar_color(&sender),
        sender,
        date: format_list_date(now_unix(), conversation.date),
        subject,
        snippet: conversation.snippet.clone(),
        unread: conversation.unread,
        has_attachment: conversation.has_attachments,
        count: conversation.message_count,
    }
}

/// Turn one search hit into the same row shape as a conversation. Search hits
/// carry only match metadata, so snippet aggregation and unread count are
/// intentionally blank.
pub fn search_row_view(hit: &SearchHit) -> ListRow {
    let sender = if hit.sender.trim().is_empty() {
        "Unknown sender".to_string()
    } else {
        hit.sender.clone()
    };
    let subject = if hit.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        hit.subject.clone()
    };

    ListRow {
        initials: initials(&sender),
        avatar_color: avatar_color(&sender),
        sender,
        date: format_list_date(now_unix(), hit.date),
        subject,
        snippet: String::new(),
        unread: false,
        has_attachment: false,
        count: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use pige_core::api::{AccountSection, FolderSummary, ModelSnapshot};
    use pige_core::mail::{FolderId, ThreadId};
    use pige_core::search::{SearchHit, SearchScope};

    #[test]
    fn initials_use_first_and_last_words() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("ada@example.com"), "A");
        assert_eq!(initials("  grace   brewster  hopper  "), "GH");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn avatar_color_is_stable_for_an_address() {
        assert_eq!(
            avatar_color("ada@example.com"),
            avatar_color("ada@example.com")
        );
        assert_ne!(
            avatar_color("ada@example.com"),
            avatar_color("grace@example.com")
        );
    }

    #[test]
    fn list_dates_show_time_today_and_year_for_old_mail() {
        // 2026-03-08 12:00 UTC, a Sunday.
        let now = 1_772_971_200;
        // 2026-03-08 09:00 UTC.
        let today_9am = 1_772_960_400;
        // 2026-03-02 09:00 UTC, the Monday six days earlier.
        let six_days_ago = 1_772_442_000;
        // 2025-03-04 09:00 UTC.
        let last_year = 1_741_078_800;

        assert_eq!(format_list_date(now, today_9am), "09:00");
        assert_eq!(format_list_date(now, six_days_ago), "Mon");
        assert_eq!(format_list_date(now, last_year), "2025-03-04");
    }

    fn folder(id: &str, name: &str, kind: CoreFolderKind, unread: u32) -> FolderSummary {
        FolderSummary {
            id: FolderId(id.into()),
            name: name.into(),
            kind,
            unread,
        }
    }

    fn two_accounts() -> ModelSnapshot {
        ModelSnapshot {
            sections: vec![
                AccountSection {
                    account_id: AccountId("personal".into()),
                    label: "Personal".into(),
                    email: "ada@example.com".into(),
                    unread: 5,
                    capabilities: Default::default(),
                    folders: vec![
                        folder("p-inbox", "Inbox", CoreFolderKind::Inbox, 3),
                        folder("p-sent", "Sent", CoreFolderKind::Sent, 0),
                    ],
                },
                AccountSection {
                    account_id: AccountId("work".into()),
                    label: "Work".into(),
                    email: "grace@example.com".into(),
                    unread: 2,
                    capabilities: Default::default(),
                    folders: vec![folder("w-inbox", "Inbox", CoreFolderKind::Inbox, 2)],
                },
            ],
            ..ModelSnapshot::default()
        }
    }

    #[test]
    fn collapsed_accounts_hide_their_folders() {
        let snapshot = two_accounts();
        let collapsed = HashSet::from([AccountId("personal".into())]);

        let views = sections(&snapshot, &collapsed);

        assert_eq!(views[0].label, "Personal");
        assert_eq!(views[0].unread, 5);
        assert!(views[0].folders.is_empty());
        assert_eq!(views[1].folders.len(), 1);
    }

    #[test]
    fn two_accounts_render_in_user_order_with_labels_and_rollups() {
        let collapsed = HashSet::new();

        let views = sections(&two_accounts(), &collapsed);

        assert_eq!(views.len(), 2);
        assert_eq!(views[0].label, "Personal");
        assert_eq!(views[0].email, "ada@example.com");
        assert_eq!(views[0].unread, 5);
        assert_eq!(views[0].folders.len(), 2);
        assert_eq!(views[0].folders[0].id, FolderId("p-inbox".into()));
        assert_eq!(views[0].folders[0].view.name, "Inbox");
        assert_eq!(views[0].folders[0].view.unread, 3);
        assert_eq!(views[1].label, "Work");
        assert_eq!(views[1].email, "grace@example.com");
        assert_eq!(views[1].unread, 2);
        assert_eq!(views[1].folders.len(), 1);
    }

    #[test]
    fn unified_selection_clears_the_account_selection() {
        let mut snapshot = two_accounts();
        snapshot.unified_selected = true;
        snapshot.selected_folder = None;

        assert!(snapshot.unified_selected);
        assert_eq!(selection_account(&snapshot), None);

        snapshot.unified_selected = false;
        snapshot.selected_folder = Some(FolderId("w-inbox".into()));
        assert_eq!(selection_account(&snapshot), Some(AccountId("work".into())));
    }

    #[test]
    fn conversation_summaries_become_list_rows() {
        let conversation = CoreConversationSummary {
            thread_id: ThreadId("thread".into()),
            account_id: AccountId("personal".into()),
            subject: "Notes on the analytical engine".into(),
            sender: "Ada Lovelace".into(),
            snippet: "The engine might compose elaborate music.".into(),
            date: 1_772_960_400,
            unread: true,
            message_count: 3,
            has_attachments: true,
        };

        let row = row_view(&conversation);

        assert_eq!(row.initials, "AL");
        assert_eq!(row.sender, "Ada Lovelace");
        assert_eq!(row.subject, "Notes on the analytical engine");
        assert_eq!(row.snippet, "The engine might compose elaborate music.");
        assert!(row.unread);
        assert!(row.has_attachment);
        assert_eq!(row.count, 3);
        assert!(!row.date.is_empty());
    }

    #[test]
    fn showing_a_toast_replaces_the_previous_one() {
        let mut toast = Toast::default();
        let first = toast.show("one".to_string());
        assert_eq!(toast.current.as_ref().unwrap().1, "one");
        let second = toast.show("two".to_string());
        assert_ne!(first, second);
        assert_eq!(toast.current.as_ref().unwrap().1, "two");
    }

    #[test]
    fn expiry_only_clears_its_own_generation() {
        let mut toast = Toast::default();
        let first = toast.show("first".to_string());
        let second = toast.show("second".to_string());

        toast.expire(first);
        assert!(toast.current.is_some(), "a stale expiry must not dismiss");

        toast.expire(second);
        assert!(toast.current.is_none());
    }

    #[test]
    fn snapshots_only_apply_to_the_matching_trimmed_query_and_scope() {
        let folder = SearchScope::Folder(FolderId("inbox".into()));
        let snapshot = ModelSnapshot {
            search_query: "cats".into(),
            search_scope: folder.clone(),
            search_results: vec![SearchHit {
                thread_id: ThreadId("thread".into()),
                account_id: AccountId("personal".into()),
                subject: "Cats".into(),
                sender: "Ada".into(),
                date: 1,
            }],
            ..ModelSnapshot::default()
        };

        assert!(snapshot_matches_search(&snapshot, "  cats  ", &folder));
        assert!(!snapshot_matches_search(
            &snapshot,
            "cats",
            &SearchScope::AllAccounts
        ));
        assert!(!snapshot_matches_search(&snapshot, "dogs", &folder));
        assert!(!snapshot_matches_search(
            &snapshot,
            "   ",
            &SearchScope::AllAccounts
        ));
    }
}
