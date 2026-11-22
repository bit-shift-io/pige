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
