# Project Plan: LiteMail (Rust + Iced Cross-Platform Email Client)

## Objective
Build a featherlight, ultra-fast cross-platform email client (Linux desktop & Android) that mimics the core UX of the official Gmail app, featuring a responsive 3-pane layout for desktops/tablets and a single-pane stack for mobile devices, backed by a local SQLite cache and IMAP/SMTP over OAuth2.

## Tech Stack
* **UI Framework:** `iced` (Elm architecture, native GPU rendering via wgpu)
* **Async Runtime:** `tokio`
* **Network / Email Protocols:** `async-imap`, `lettre` (SMTP), `oauth2`
* **Storage & Caching:** `rusqlite` (with FTS5 full-text search)
* **HTML Parsing & Sanitization:** `mail-parse`, `ammonia`, and `iced_wry` (or Markdown fallback)

---

## Architecture & Layout Strategy
1. **Responsive Viewport State:**
   * Dynamic window width check in `view()`:
     * `< 800px`: Single-pane stack (Folders -> Message List -> Reader) with navigation state machine.
     * `>= 800px`: 3-pane Master-Detail layout (Sidebar Drawer + Message List + Reader).
2. **Data Layer (Background Worker):**
   * Background Tokio task handling IMAP sync and OAuth token refresh.
   * Local SQLite database acting as the single source of truth for offline-first responsiveness.
3. **UI Components:**
   * Custom message row layout (avatar initials, unread bolding, sender/subject, snippet, timestamps).
   * Floating Action Button (FAB) for composition.
   * Integrated HTML/Text reader pane.