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

/// One "Mail" channel; notifications are grouped by account and tap through to
/// `MainActivity` with the account and thread ids. Android 13+ requires the
/// runtime POST_NOTIFICATIONS permission, so posting is a quiet no-op until the
/// user grants it.
object NotificationPoster {
    private const val CHANNEL_ID = "mail"
    private const val CHANNEL_NAME = "Mail"

    @SuppressLint("MissingPermission")
    fun post(context: Context, account: String, thread: String, sender: String, subject: String) {
        if (context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        ensureChannel(context)

        val tap = Intent(context, MainActivity::class.java).apply {
            putExtra(MainActivity.EXTRA_ACCOUNT, account)
            putExtra(MainActivity.EXTRA_THREAD, thread)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val id = "$account\u0000$thread".hashCode()
        val pending = PendingIntent.getActivity(
            context,
            id,
            tap,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        val title = sender.ifBlank { "New mail" }
        val text = subject.ifBlank { "(no subject)" }
        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mail)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setGroup("account:$account")
            .setAutoCancel(true)
            .setContentIntent(pending)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .build()
        NotificationManagerCompat.from(context).notify(id, notification)
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
