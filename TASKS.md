# Tasks: LiteMail Implementation Steps for Agy

## Phase 1: Project Scaffolding & Dependencies
- [ ] Task 1.1: Initialize Cargo workspace and crate structure (`src/main.rs`, `src/app.rs`, `src/db/`, `src/net/`, `src/ui/`).
- [ ] Task 1.2: Populate `Cargo.toml` with required dependencies (`iced`, `tokio`, `async-imap`, `lettre`, `oauth2`, `rusqlite`, `mail-parse`, `ammonia`, `iced_wry`).
- [ ] Task 1.3: Verify initial `cargo check` compiles cleanly with zero warnings.

## Phase 2: Authentication & Network Backend
- [ ] Task 2.1: Implement Google OAuth2 loopback server flow (`oauth2` crate) to securely fetch and store tokens.
- [ ] Task 2.2: Build async IMAP connection manager (`async-imap`) to authenticate and fetch folder lists (Inbox, Sent, Trash, Archive).
- [ ] Task 2.3: Implement background sync worker to download message headers, snippets, and MIME parts into a local SQLite database (`rusqlite`).

## Phase 3: Responsive UI & State Machine (Iced)
- [ ] Task 3.1: Define the core `Model`, `Message`, and `State` enums for the Elm architecture.
- [ ] Task 3.2: Implement responsive layout switchers:
  * Mobile single-pane navigation stack (Folders -> List -> Reader).
  * Desktop/Tablet 3-pane master-detail layout (Sidebar + Message List + Reader).
- [ ] Task 3.3: Style and construct the message list view (sender avatars, unread indicators, snippets, and hover/click states).
- [ ] Task 3.4: Add the Floating Action Button (FAB) and compose screen modal/view.

## Phase 4: Message Parsing & HTML Rendering
- [ ] Task 4.1: Implement MIME part extraction (`mail-parse`) to separate plain text and HTML payloads.
- [ ] Task 4.2: Integrate `ammonia` HTML sanitizer to strip dangerous scripts, forms, and tracking pixels.
- [ ] Task 4.3: Implement the email reader view using `iced_wry` for rich HTML rendering (with a fallback text parser option).

## Phase 5: Optimization & Compilation Check
- [ ] Task 5.1: Verify local caching performance, ensuring instant UI rendering from SQLite on startup.
- [ ] Task 5.2: Run final cross-platform compilation checks (`cargo build --release`) for Linux and verify Android target configurations (`cargo-apk`).