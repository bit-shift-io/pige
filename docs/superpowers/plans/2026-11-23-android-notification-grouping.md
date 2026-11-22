# Android Notification Grouping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Group Android new-mail notifications into one Gmail-style bundle per account, with a live summary and tap-through to each thread.

**Architecture:** Rust keeps calling the notifier once per message. Kotlin posts each child silently, then rebuilds a stable per-account summary from `NotificationManagerCompat.getActiveNotifications()`. A small receiver rebuilds after swipes; `MainActivity` and the JNI tap handler distinguish child taps (open thread) from summary taps (open Inbox and clear the group).

**Tech Stack:** Kotlin, AndroidX Core 1.17.0, Rust JNI, Gradle, adb.

**Spec:** `docs/superpowers/specs/2026-11-23-android-notification-grouping-design.md`

## Global Constraints

- Android minSdk 34, targetSdk 36; one `"mail"` channel; androidx.core 1.17.0.
- Group key stays `account:<account-id>`; child id is `hash(account\0thread)`; summary id is `hash("summary\0" + account)`.
- POST_NOTIFICATIONS gating stays unchanged.
- No `pige_core` or desktop changes; the only Rust change is the Android-only JNI tap handler.
- Keep the existing per-thread tap behavior.

## Review Focus

- A burst of five new messages must produce one bundle and one alert, not five alerts.
- Swiping one child must decrement the summary; swiping the last child must remove the summary.
- Summary tap must open the account Inbox and clear that account's notifications; child tap must open the thread and decrement the summary.
- Two accounts must produce two independent bundles; clearing one must leave the other.
- A summary posted after the app process was killed must rebuild from the live tray, not from in-memory state; blank sender/subject must use fallbacks.

## File Structure

- `android/app/src/main/kotlin/bitshift/pige/NotificationSummary.kt` — pure text and id rules; no Android classes, unit tested on the JVM.
- `android/app/src/main/kotlin/bitshift/pige/NotificationPoster.kt` — channel, child posting, summary rebuild, clear/tap helpers.
- `android/app/src/main/kotlin/bitshift/pige/NotificationDismissReceiver.kt` — rebuilds a summary after a child is swiped away.
- `android/app/src/main/kotlin/bitshift/pige/MainActivity.kt` — routes child and summary taps.
- `rust/pige/src/jni.rs` — empty thread means "open the account Inbox only".
- `android/app/src/debug/kotlin/bitshift/pige/NotificationProbeReceiver.kt` — debug-only adb hook for verification.
- `android/app/src/test/kotlin/bitshift/pige/NotificationSummaryTest.kt` — JVM tests for Task 1.

## Preflight

- [ ] **Commit the approved design and plan**

```bash
git add docs/superpowers/specs/2026-11-23-android-notification-grouping-design.md docs/superpowers/plans/2026-11-23-android-notification-grouping.md
git commit -m "docs: add Android notification grouping design and plan"
```

---

### Task 1: Pure summary layout and notification ids

**Files:**
- Create: `android/app/src/main/kotlin/bitshift/pige/NotificationSummary.kt`
- Test: `android/app/src/test/kotlin/bitshift/pige/NotificationSummaryTest.kt`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `data class NotificationLine(sender: String, subject: String, postTime: Long)`
  - `NotificationSummary.title(count: Int): String`
  - `NotificationSummary.lines(entries: List<NotificationLine>): List<String>`
  - `NotificationSummary.groupKey(account: String): String`
  - `NotificationSummary.childId(account: String, thread: String): Int`
  - `NotificationSummary.summaryId(account: String): Int`
  - `NotificationSummary.MAX_LINES: Int`

- [ ] **Step 1: Write the failing test**

```kotlin
package bitshift.pige

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class NotificationSummaryTest {
    private fun entry(sender: String, subject: String, postTime: Long) =
        NotificationLine(sender, subject, postTime)

    @Test
    fun `title singular and plural`() {
        assertEquals("1 new message", NotificationSummary.title(1))
        assertEquals("3 new messages", NotificationSummary.title(3))
    }

    @Test
    fun `lines are newest first`() {
        val lines = NotificationSummary.lines(
            listOf(
                entry("Older", "One", 10),
                entry("Newest", "Two", 30),
                entry("Middle", "Three", 20),
            )
        )
        assertEquals(
            listOf("Newest — Two", "Middle — Three", "Older — One"),
            lines,
        )
    }

    @Test
    fun `lines cap at five and summarize the rest`() {
        val entries = (1..7).map { entry("Sender $it", "Subject $it", it.toLong()) }
        val lines = NotificationSummary.lines(entries)
        assertEquals(5, lines.size)
        assertEquals("Sender 7 — Subject 7", lines.first())
        assertEquals("and 3 more", lines.last())
    }

    @Test
    fun `blank sender and subject use fallbacks`() {
        assertEquals(
            listOf("New mail — (no subject)"),
            NotificationSummary.lines(listOf(entry("", "", 1))),
        )
    }

    @Test
    fun `summary id is namespaced away from child ids`() {
        assertNotEquals(
            NotificationSummary.summaryId("account-1"),
            NotificationSummary.childId("account-1", "thread-1"),
        )
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
JAVA_SRC="$(cargo metadata --locked --format-version 1 --filter-platform aarch64-linux-android --manifest-path rust/Cargo.toml | python3 -c 'import json,sys,pathlib; p=next(p for p in json.load(sys.stdin)["packages"] if p["name"]=="iced_android"); print(pathlib.Path(p["manifest_path"]).parent / "java")')"
bash android/gradlew -p android -PicedAndroidJava="$JAVA_SRC" testDebugUnitTest --tests 'bitshift.pige.NotificationSummaryTest'
```

Expected: FAIL to compile with `Unresolved reference: NotificationSummary`.

- [ ] **Step 3: Implement `NotificationSummary` in `android/app/src/main/kotlin/bitshift/pige/NotificationSummary.kt`**

Use the signatures above. Implementation rules:

- `title(1)` is `"1 new message"`; every other count is `"<count> new messages"`.
- `lines` sorts by `postTime` descending. With more than `MAX_LINES` entries, show the newest `MAX_LINES - 1` and append `"and <remaining> more"`.
- A line is `"<sender> — <subject>"`; blank sender becomes `"New mail"`, blank subject becomes `"(no subject)"`.
- `childId` hashes `"$account\u0000$thread"`; `summaryId` hashes `"summary\u0000$account"`.

- [ ] **Step 4: Run the test to verify it passes**

Run the same command as Step 2. Expected: `BUILD SUCCESSFUL` and `NotificationSummaryTest` passes.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/kotlin/bitshift/pige/NotificationSummary.kt android/app/src/test/kotlin/bitshift/pige/NotificationSummaryTest.kt
git commit -m "feat: add grouped notification summary layout"
```

---

### Task 2: Post children plus a per-account summary

**Files:**
- Modify: `android/app/src/main/kotlin/bitshift/pige/NotificationPoster.kt`
- Create: `android/app/src/main/kotlin/bitshift/pige/NotificationDismissReceiver.kt`
- Modify: `android/app/src/main/AndroidManifest.xml`
- Create: `android/app/src/debug/kotlin/bitshift/pige/NotificationProbeReceiver.kt`
- Create: `android/app/src/debug/AndroidManifest.xml`

**Interfaces:**
- Consumes: `NotificationSummary` and `NotificationLine` from Task 1.
- Produces:
  - `NotificationPoster.post(context: Context, account: String, thread: String, sender: String, subject: String)`
  - `NotificationPoster.rebuildSummary(context: Context, account: String, alert: Boolean = false)`
  - `NotificationPoster.clearAccount(context: Context, account: String)`
  - `NotificationPoster.childTapped(context: Context, account: String, thread: String)`
  - `NotificationDismissReceiver.ACTION_DISMISSED: String`

- [ ] **Step 1: Rework `NotificationPoster`**

Keep the existing channel setup and permission gate. Add these behaviors:

- `post` builds the child with `NotificationSummary.childId(account, thread)`, group `NotificationSummary.groupKey(account)`, `.setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_SUMMARY)`, `.setAutoCancel(true)`, the existing content intent, and a new delete intent. The delete intent is an explicit broadcast to `NotificationDismissReceiver` with action `NotificationDismissReceiver.ACTION_DISMISSED` and `MainActivity.EXTRA_ACCOUNT`. After `notify(id, child)`, call `rebuildSummary(context, account, alert = true)`.
- `rebuildSummary(context, account, alert = false)`:
  - Return early when POST_NOTIFICATIONS is not granted; call `ensureChannel`.
  - Read `NotificationManagerCompat.from(context).activeNotifications`, keep records whose `notification.group` equals the account group and whose `id` is not `NotificationSummary.summaryId(account)`, and sort newest first by `postTime`.
  - If no children remain, cancel the summary id, clear the account's alert timestamp, and return.
  - Map children to `NotificationLine` using `NotificationCompat.EXTRA_TITLE` / `NotificationCompat.EXTRA_TEXT` and `postTime`.
  - Build a summary with `NotificationSummary.title(count)`, `NotificationSummary.lines(...)`, `NotificationCompat.InboxStyle().setBigContentTitle(title)` plus one `addLine` per line, `.setNumber(count)`, the same group, `.setGroupSummary(true)`, `.setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_SUMMARY)`, `.setOnlyAlertOnce(true)`, `.setAutoCancel(true)`, and a content intent that carries only `MainActivity.EXTRA_ACCOUNT`.
  - When `alert` is true and at least 10 seconds have passed since this account's last summary alert, cancel the existing summary before notifying so the new one alerts; record the time. Within the window, or when `alert` is false, update silently. When `alert` is false, also call `.setSilent(true)` on the summary so a rebuild after a swipe cannot make noise.
- `clearAccount` cancels every active notification whose group matches, then clears the account's alert timestamp.
- `childTapped` cancels `NotificationSummary.childId(account, thread)` and calls `rebuildSummary(context, account)`.

- [ ] **Step 2: Add the dismissal receiver**

Create `NotificationDismissReceiver.kt`:

```kotlin
package bitshift.pige

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/// Rebuilds the account summary after the user swipes away a child notification.
class NotificationDismissReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != ACTION_DISMISSED) return
        val account = intent.getStringExtra(MainActivity.EXTRA_ACCOUNT) ?: return
        NotificationPoster.rebuildSummary(context, account)
    }

    companion object {
        const val ACTION_DISMISSED = "bitshift.pige.notification.DISMISSED"
    }
}
```

Register it inside `<application>` in `android/app/src/main/AndroidManifest.xml`:

```xml
<receiver
    android:name=".NotificationDismissReceiver"
    android:exported="false"/>
```

- [ ] **Step 3: Add the debug-only probe**

Create `android/app/src/debug/kotlin/bitshift/pige/NotificationProbeReceiver.kt`:

```kotlin
package bitshift.pige

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationManagerCompat

/// Debug-only hook for exercising notifications from adb without real mail.
/// The `debug` source set keeps this out of release builds.
class NotificationProbeReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val account = intent.getStringExtra("account") ?: "probe"
        when {
            intent.getBooleanExtra("clear", false) ->
                NotificationPoster.clearAccount(context, account)

            intent.getBooleanExtra("dismiss", false) -> {
                val thread = intent.getStringExtra("thread") ?: return
                NotificationManagerCompat.from(context)
                    .cancel(NotificationSummary.childId(account, thread))
                context.sendBroadcast(
                    Intent(context, NotificationDismissReceiver::class.java).apply {
                        action = NotificationDismissReceiver.ACTION_DISMISSED
                        putExtra(MainActivity.EXTRA_ACCOUNT, account)
                    }
                )
            }

            else -> {
                val thread = intent.getStringExtra("thread") ?: return
                NotificationPoster.post(
                    context,
                    account,
                    thread,
                    intent.getStringExtra("sender").orEmpty(),
                    intent.getStringExtra("subject").orEmpty(),
                )
            }
        }
    }
}
```

Create `android/app/src/debug/AndroidManifest.xml`:

```xml
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <application>
        <receiver
            android:name=".NotificationProbeReceiver"
            android:exported="true"/>
    </application>
</manifest>
```

- [ ] **Step 4: Build and install**

```bash
bash scripts/build.sh debug
adb install -r build/app/outputs/apk/debug/app-debug.apk
adb shell pm grant bitshift.pige android.permission.POST_NOTIFICATIONS || true
```

Expected: `BUILD SUCCESSFUL`, `Success`, and the grant command either succeeds or reports it is already granted.

- [ ] **Step 5: Verify a burst, a process restart, and a dismissal on the device**

Clear the probe account, then post five messages:

```bash
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver --es account demo --ez clear true
for i in 1 2 3 4 5; do
  adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver \
    --es account demo --es thread "t$i" --es sender "Sender $i" --es subject "Subject $i"
done
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:demo|isGroupSummary|number="
```

Expected: six records with `groupKey=account:demo` (five children + one summary), exactly one `isGroupSummary=true`, and the summary shows `number=5`.

Kill the process without clearing notifications, then post a sixth:

```bash
adb shell input keyevent KEYCODE_HOME
adb shell am kill bitshift.pige
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver \
  --es account demo --es thread t6 --es sender "Sender 6" --es subject "Subject 6"
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:demo|isGroupSummary|number="
```

Expected: seven group records and the summary shows `number=6`, rebuilt from the live tray.

Simulate a swipe dismissal of `t1` through the probe:

```bash
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver \
  --es account demo --es thread t1 --ez dismiss true
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:demo|isGroupSummary|number="
```

Expected: the summary shows `number=5`.

Open the shade once and confirm one collapsed bundle with a sender/subject preview; the alert itself is a manual check (five quick posts should make one sound, not five).

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/kotlin/bitshift/pige/NotificationPoster.kt \
        android/app/src/main/kotlin/bitshift/pige/NotificationDismissReceiver.kt \
        android/app/src/main/AndroidManifest.xml \
        android/app/src/debug/kotlin/bitshift/pige/NotificationProbeReceiver.kt \
        android/app/src/debug/AndroidManifest.xml
git commit -m "feat: group Android mail notifications by account"
```

---

### Task 3: Child and summary tap paths

**Files:**
- Modify: `android/app/src/main/kotlin/bitshift/pige/MainActivity.kt`
- Modify: `rust/pige/src/jni.rs`

**Interfaces:**
- Consumes: `NotificationPoster.childTapped` and `NotificationPoster.clearAccount` from Task 2; the existing `nativeNotificationTap(account, thread)`.
- Produces: an empty `thread` argument now means "open the account Inbox only".

- [ ] **Step 1: Route the two tap shapes in `MainActivity`**

Replace the thread requirement in `forwardNotificationTap` with:

```kotlin
private fun forwardNotificationTap(intent: Intent?) {
    val account = intent?.getStringExtra(EXTRA_ACCOUNT) ?: return
    val thread = intent.getStringExtra(EXTRA_THREAD).orEmpty()
    intent.removeExtra(EXTRA_ACCOUNT)
    intent.removeExtra(EXTRA_THREAD)
    if (thread.isEmpty()) {
        NotificationPoster.clearAccount(this, account)
    } else {
        NotificationPoster.childTapped(this, account, thread)
    }
    nativeNotificationTap(account, thread)
}
```

- [ ] **Step 2: Allow an empty thread in the JNI tap handler**

In `rust/pige/src/jni.rs`, remove the early block that rejects an empty thread. After the Inbox/unified folder selection succeeds, add:

```rust
if thread.is_empty() {
    log::info!("notification tap: opened inbox");
    return;
}
```

Keep `api::open_conversation` for non-empty threads. Update the function's doc comment to say an empty thread is a summary tap that opens the Inbox.

- [ ] **Step 3: Build and install**

```bash
bash scripts/build.sh debug
adb install -r build/app/outputs/apk/debug/app-debug.apk
```

Expected: `BUILD SUCCESSFUL`, `Success`.

- [ ] **Step 4: Verify a child tap decrements the summary**

```bash
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver --es account demo --ez clear true
for i in 1 2 3; do
  adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver \
    --es account demo --es thread "t$i" --es sender "Sender $i" --es subject "Subject $i"
done
adb shell am start -W -n bitshift.pige/.MainActivity --es account demo --es thread t1
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:demo|isGroupSummary|number="
```

Expected: `t1` is gone and the summary shows `number=2`. A missing-thread log line is fine because the probe thread is not in the real mailbox.

- [ ] **Step 5: Verify a summary tap clears the account**

```bash
adb shell am start -W -n bitshift.pige/.MainActivity --es account demo
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:demo"
```

Expected: no output; the demo group and its summary are gone.

- [ ] **Step 6: Verify accounts stay independent**

```bash
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver --es account demo --es thread d1 --es sender "Demo" --es subject "One"
adb shell am broadcast -n bitshift.pige/.NotificationProbeReceiver --es account other --es thread o1 --es sender "Other" --es subject "Two"
adb shell am start -W -n bitshift.pige/.MainActivity --es account demo
adb shell dumpsys notification --noredact | grep -E "groupKey=.*account:(demo|other)|isGroupSummary|number="
```

Expected: no `account:demo` records; one `account:other` child and one summary with `number=1`.

- [ ] **Step 7: Commit**

```bash
git add android/app/src/main/kotlin/bitshift/pige/MainActivity.kt rust/pige/src/jni.rs
git commit -m "feat: open inbox from grouped notification summary"
```

---

## Final Verification

Run the unit tests once more, then the release build to catch source-set mistakes:

```bash
JAVA_SRC="$(cargo metadata --locked --format-version 1 --filter-platform aarch64-linux-android --manifest-path rust/Cargo.toml | python3 -c 'import json,sys,pathlib; p=next(p for p in json.load(sys.stdin)["packages"] if p["name"]=="iced_android"); print(pathlib.Path(p["manifest_path"]).parent / "java")')"
bash android/gradlew -p android -PicedAndroidJava="$JAVA_SRC" testDebugUnitTest
bash scripts/build.sh release
```

Expected: tests pass; the release build succeeds and contains no `NotificationProbeReceiver`.
