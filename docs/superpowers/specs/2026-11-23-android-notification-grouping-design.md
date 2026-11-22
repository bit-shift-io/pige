# Android notification grouping — design

Date: 2026-11-23
Status: approved, pending build

## Intent and why

Each new inbox message currently posts its own notification, so a burst of mail
fills the shade. Group them per account like Gmail: one collapsed bundle with a
summary ("N new messages") that expands to the individual messages, each still
tapping through to its thread.

## How it works today

- Rust's sync loop calls `config.notifier.notify(mail)` once per new inbox
  message (`rust/pige_core/src/api/mod.rs:4062`).
- `AndroidNotifier` forwards account/thread/sender/subject over JNI
  (`rust/pige_core/src/platform/android.rs:422`) to `PigeBridge.notify` →
  `NotificationPoster.post`.
- `NotificationPoster` posts one notification per account+thread, id =
  hash(account\0thread), with `.setGroup("account:$account")` but **no group
  summary**. Android 14 therefore does not render the Gmail-style collapsed
  bundle.
- Tapping a notification goes to `MainActivity` with account+thread, and
  `nativeNotificationTap` selects the account's Inbox and opens the thread. An
  empty thread is currently rejected (`rust/pige/src/jni.rs:223`).

## Approach (picked): Kotlin-side summary from the live tray

Keep Rust posting one notification per message. In `NotificationPoster`:

1. Post the child as today, plus `setGroupAlertBehavior(GROUP_ALERT_SUMMARY)` so
   a burst of five alerts once, not five times.
2. Rebuild a stable per-account summary (`"summary\0$account".hashCode()`,
   `setGroupSummary(true)`, same group key) from
   `NotificationManagerCompat.getActiveNotifications()`: count live children,
   InboxStyle lines "Sender — Subject" newest first (max 5, then "and N more"),
   title "N new message(s)", `setNumber(count)`.
3. Summary tap uses the existing notification intent with an empty thread; Rust
   selects that account's Inbox (or unified) and skips opening a conversation.
   Tapping the summary also clears that account's notifications.
4. Keep the count honest: cancel the tapped child and rebuild the summary in
   `MainActivity`; add a non-exported `BroadcastReceiver` wired via each child's
   `setDeleteIntent` to rebuild after a swipe or "clear all".
5. When no children remain, cancel the summary.

Why: no Rust core changes, and the count comes from what is actually in the
tray, so it stays right across multiple sync passes.

## Alternatives considered

- **Rust batch notify:** pass the whole `new_inbox` slice to Kotlin and post
  children + summary together. Exact count for one pass, but still drifts when
  older notifications remain, and expands the JNI/`Notifier` surface plus
  fake/API tests.
- **Summary-only per account (no children):** simplest tray, but loses the
  expandable per-message stack the user picked.

## Goals / non-goals / anti-goals

- Goal: one collapsed bundle per account on Android 14, expandable to
  individual messages; summary count matches live children; each child still
  opens its thread.
- Non-goal: grouping across accounts; changing the desktop Linux notifier;
  changing sync or notification settings.
- Anti-goal: five sounds/vibrations for a five-message sync; stale summary
  counts after a swipe; a summary tap that lands nowhere.

## Constraints

- minSdk 34 / targetSdk 36; androidx.core 1.17.0 provides
  `getActiveNotifications`.
- POST_NOTIFICATIONS gating stays as-is.
- One "Mail" channel stays; groups are per account.
- The summary id namespace must not collide with child ids.

## Verification

- Build/install on the attached device (iPlay60 mini Turbo, Android 14) via
  `scripts/build.sh` + `adb install`, or `run_adb.sh`.
- Trigger several new inbox messages (real account sync); confirm with
  `adb shell dumpsys notification --noredact` that children share group key
  `account:<id>`, the summary has `isGroupSummary=true`, and the count matches.
- Check the shade: one bundle per account, expands to children; summary opens
  the Inbox; tapping a child opens its thread and the summary count drops;
  swiping a child drops it too.
- Desktop behavior unchanged (fake/Linux notifier untouched).

## Left to the builder

- Exact summary wording ("N new messages" vs "N new").
- Whether to show the account email in the summary. The bridge currently passes
  only the opaque account id, so this would need an extra argument. Default: no
  account label.
