# New-mail body precache + app icon — design

Date: 2026-11-21
Status: approved approach, pending build

## Task 1 — Precache bodies of newly-arrived mail (all platforms)

### Intent and why

When a sync pass discovers new inbox mail, the OS notification can be tapped
before the message body exists locally, so the reader opens cold and downloads
on demand. The goal: the moment mail arrives, its body is downloaded too, so
tapping the notification renders the message instantly. This must work on
every OS surface — desktop, Android foreground, and Android's background
`SyncWorker` — not just Android.

### How it works today

- All sync on every platform funnels through `sync_one` in
  `rust/pige_core/src/api/mod.rs`. It returns the provider session alive; the
  notify loop (`config.notifier.notify(mail)` per `report.new_inbox`) runs
  after the gate is released, with the session still open.
- A generic ranked body prefetch (`prefetch_bodies`) exists, but it runs only
  in foreground shells after sync (UI-driven), so Android's background worker
  never prefetches anything.
- `NewMailSummary` (rust/pige_core/src/sync/engine.rs) carries
  account/folder/thread/from/subject but **not** the message id.

### Approach (picked)

Core-level, inside the sync pass — notify first, then prefetch immediately:

1. Add `message_id: MessageId` to `NewMailSummary`; the provider sync loop
   (rust/pige_core/src/sync/provider.rs) already has the id in hand.
2. In `sync_one`, right after the notify loop, run a bounded new-mail prefetch:
   for each `new_inbox` summary (newest first, capped per pass), if the
   settings toggle `prefetch_bodies` is on, the message has no cached body,
   and `header.size <= MAX_BODY_BYTES`, fetch the body through the
   already-open provider session (`body::fetch_body_with`) and cache it with
   the existing `body::cache_body_content`.
3. Each fetch goes through `scheduler.run_background` per message (an
   interactive reader open is never blocked; a busy gate skips the fetch —
   the on-demand path covers it) with its own short operation context, so a
   stalled read cannot eat the account's pass deadline.
4. Every failure is logged and swallowed: prefetch must never fail the sync.

Because every platform's sync flows through `sync_one`, Android's background
worker gets the behavior for free — no shell or Kotlin changes.

### Alternatives considered

- *Extend the UI-loop prefetch only*: misses Android background sync
  entirely (no UI loop there); needs per-shell wiring.
- *Fetch in Kotlin/WorkManager*: wrong layer; would duplicate provider
  sessions and auth outside the Rust core.

### Goals / non-goals / anti-goals

- Goal: notification tap almost always opens a warm body; works in
  background sync.
- Non-goal: prefetching attachment bytes (still on demand; descriptors arrive
  with the body as today), prefetching non-inbox folders, changing the
  generic ranked prefetch (stays as catch-up; it skips already-cached bodies).
- Anti-goal: delaying the notification for the download (decided:
  notify first, prefetch right after).

### Constraints

- Respect the existing "Prefetch bodies" settings toggle.
- Cap per pass (`NEW_MAIL_PREFETCH_LIMIT = 10`) so a 100-message morning
  burst cannot monopolize the session.
- Same revalidation discipline as the existing prefetch (account still
  present, message still in folder, coords unchanged) before caching.

### Tests

- Core test: a sync pass that reports new inbox mail also caches those
  bodies (fake provider, `fetch_body_calls` observed).
- Toggle off → no fetch. Already-cached or oversized message → skipped.
- Prefetch failure → sync still succeeds.

## Task 2 — App icon on desktop and Android

### Intent

Ship a real app icon: desktop window/taskbar icon and Android launcher icon,
from `assets/app_icon_2.svg` (user-picked; newest vector, matches the target
render).

### Findings

- Both SVGs contain the full origami-pigeon design, but their drop-shadow
  `filter="url(#…)"` attribute makes renderers (ImageMagick, Inkscape) drop
  the bird group. Stripping the attribute renders correctly.
- Desktop is iced (daemon); the window opens in `App::new` via
  `window::open(window::Settings)`, and `window::Settings.icon` accepts an
  `iced::window::icon::Icon` built from raw RGBA. Iced's `image` feature
  already puts the `image` crate (PNG) in the tree, so decoding an embedded
  PNG adds no new compile cost.
- Android has **no `android:icon`** and no mipmap resources — default icon.

### Plan

1. Fix `assets/app_icon_2.svg` by removing the broken filter attribute (keep
   everything else byte-identical).
2. Rasterize with Inkscape into committed assets:
   - `assets/app_icon_256.png` (desktop window icon source),
   - Android launcher mipmaps `ic_launcher.png` at 48/72/96/144/192 px in
     `android/app/src/main/res/mipmap-{mdpi,hdpi,xhdpi,xxhdpi,xxxhdpi}/`.
3. Desktop: add `image` (png only, default-features off) to `pige`; embed
   `app_icon_256.png` with `include_bytes!`, decode to RGBA, set
   `window::Settings.icon` in `App::new` (non-Android only; Android ignores
   window icons). Decode failure falls back to no icon — never blocks
   startup.
4. Android: add `android:icon="@mipmap/ic_launcher"` to `<application>`.
5. Non-goals: adaptive icon layers, notification small icon, store listing
   assets.
