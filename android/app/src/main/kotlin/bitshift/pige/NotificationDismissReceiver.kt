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
