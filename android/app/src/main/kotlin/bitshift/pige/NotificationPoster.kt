package bitshift.pige

// Adapted from qlaunch (bitshift, MIT): Rust decides what to notify; Kotlin
// draws it in the system tray.
import android.Manifest
import android.annotation.SuppressLint
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat

/// One "Mail" channel; notifications are grouped per account and tap through
/// to `MainActivity` with the account and thread ids. Android 13+ requires the
/// runtime POST_NOTIFICATIONS permission, so posting is a quiet no-op until the
/// user grants it.
object NotificationPoster {
    private const val CHANNEL_ID = "mail"
    private const val CHANNEL_NAME = "Mail"
    private const val ALERT_WINDOW_MS = 10_000L

    /// Last time each account's summary was allowed to alert. A sync pass can
    /// post several messages in a row; only the first one in the window rings.
    private val lastAlertAt = mutableMapOf<String, Long>()

    @SuppressLint("MissingPermission")
    fun post(context: Context, account: String, thread: String, sender: String, subject: String) {
        if (!canPost(context)) return
        ensureChannel(context)

        val id = NotificationSummary.childId(account, thread)
        val title = sender.ifBlank { "New mail" }
        val text = subject.ifBlank { "(no subject)" }
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mail)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setGroup(NotificationSummary.groupKey(account))
            .setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_SUMMARY)
            .setAutoCancel(true)
            .setContentIntent(tapIntent(context, account, thread, id))
            .setDeleteIntent(dismissIntent(context, account, id))
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        NotificationManagerCompat.from(context).notify(id, notification)
        rebuildSummary(context, account, alert = true)
    }

    /// Rebuild the account's group summary from the notifications still in the
    /// tray. `alert` is true only for a new message, so a swipe or a tap can
    /// refresh the count without making noise.
    @SuppressLint("MissingPermission")
    fun rebuildSummary(context: Context, account: String, alert: Boolean = false) {
        if (!canPost(context)) return
        ensureChannel(context)

        val manager = NotificationManagerCompat.from(context)
        val group = NotificationSummary.groupKey(account)
        val summaryId = NotificationSummary.summaryId(account)
        val children = manager.activeNotifications
            .filter { it.notification.group == group && it.id != summaryId }
            .sortedByDescending { it.postTime }
        if (children.isEmpty()) {
            manager.cancel(summaryId)
            synchronized(lastAlertAt) { lastAlertAt.remove(account) }
            return
        }

        val count = children.size
        val title = NotificationSummary.title(count)
        val lines = NotificationSummary.lines(
            children.map { child ->
                NotificationLine(
                    sender = child.notification.extras
                        .getCharSequence(NotificationCompat.EXTRA_TITLE)?.toString().orEmpty(),
                    subject = child.notification.extras
                        .getCharSequence(NotificationCompat.EXTRA_TEXT)?.toString().orEmpty(),
                    postTime = child.postTime,
                )
            }
        )
        val style = NotificationCompat.InboxStyle().setBigContentTitle(title)
        lines.forEach(style::addLine)

        val shouldAlert = alert && alertNow(account)

        // Never cancel the summary to force an alert: Android removes the
        // group's children along with a canceled summary.
        val summary = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mail)
            .setContentTitle(title)
            .setContentText(lines.firstOrNull().orEmpty())
            .setStyle(style)
            .setNumber(count)
            .setGroup(group)
            .setGroupSummary(true)
            .setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_SUMMARY)
            .setOnlyAlertOnce(!shouldAlert)
            .setAutoCancel(true)
            .setContentIntent(tapIntent(context, account, null, summaryId))
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .apply { if (!shouldAlert) setSilent(true) }
            .build()
        manager.notify(summaryId, summary)
    }

    /// Drop every notification in the account's group, for a summary tap.
    fun clearAccount(context: Context, account: String) {
        val manager = NotificationManagerCompat.from(context)
        val group = NotificationSummary.groupKey(account)
        manager.activeNotifications
            .filter { it.notification.group == group }
            .forEach { manager.cancel(it.id) }
        synchronized(lastAlertAt) { lastAlertAt.remove(account) }
    }

    /// A child tap opens its thread; refresh the summary after the system
    /// auto-cancels that child.
    fun childTapped(context: Context, account: String, thread: String) {
        NotificationManagerCompat.from(context)
            .cancel(NotificationSummary.childId(account, thread))
        rebuildSummary(context, account)
    }

    private fun canPost(context: Context): Boolean =
        context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) ==
            PackageManager.PERMISSION_GRANTED

    /// True when this account has not alerted within the last 10 seconds.
    private fun alertNow(account: String): Boolean {
        val now = System.currentTimeMillis()
        synchronized(lastAlertAt) {
            val last = lastAlertAt[account]
            if (last != null && now - last < ALERT_WINDOW_MS) return false
            lastAlertAt[account] = now
            return true
        }
    }

    private fun tapIntent(
        context: Context,
        account: String,
        thread: String?,
        requestCode: Int,
    ): PendingIntent {
        val tap = Intent(context, MainActivity::class.java).apply {
            putExtra(MainActivity.EXTRA_ACCOUNT, account)
            if (thread != null) putExtra(MainActivity.EXTRA_THREAD, thread)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        return PendingIntent.getActivity(
            context,
            requestCode,
            tap,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun dismissIntent(context: Context, account: String, requestCode: Int): PendingIntent {
        val dismiss = Intent(context, NotificationDismissReceiver::class.java).apply {
            action = NotificationDismissReceiver.ACTION_DISMISSED
            putExtra(MainActivity.EXTRA_ACCOUNT, account)
        }
        return PendingIntent.getBroadcast(
            context,
            requestCode,
            dismiss,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
    }

    private fun ensureChannel(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        if (manager.getNotificationChannel(CHANNEL_ID) == null) {
            manager.createNotificationChannel(
                NotificationChannel(
                    CHANNEL_ID,
                    CHANNEL_NAME,
                    NotificationManager.IMPORTANCE_DEFAULT,
                ).apply {
                    description = "New mail"
                    enableVibration(true)
                }
            )
        }
    }
}
