package bitshift.pige

// Adapted from qlaunch (bitshift, MIT).
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.util.Log
import androidx.core.content.FileProvider
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import java.io.File
import java.util.concurrent.TimeUnit

// Everything the Rust core asks the OS for. Rust owns all data and logic; this
// class only answers questions and performs OS actions. Methods are `@JvmStatic`
// so Rust finds them as static methods on `bitshift/pige/PigeBridge`.
object PigeBridge {
    /// WorkManager's minimum periodic interval. Changing the sync interval in
    /// settings calls [scheduleSync] to replace this work.
    private const val MIN_SYNC_MINUTES = 15
    const val DEFAULT_SYNC_MINUTES = MIN_SYNC_MINUTES
    private const val WORK_NAME = "pige-sync"

    private var appContext: Context? = null

    @JvmStatic
    fun install(context: Context) {
        install(context, schedule = true)
    }

    /// Worker entry point: installs the context without re-enqueueing the
    /// periodic work from inside its own run.
    @JvmStatic
    fun installForWorker(context: Context) {
        install(context, schedule = false)
    }

    private fun install(context: Context, schedule: Boolean) {
        appContext = context.applicationContext
        if (schedule) {
            try {
                scheduleSync(DEFAULT_SYNC_MINUTES)
            } catch (error: Throwable) {
                // WorkManager being unavailable must not prevent the app from
                // opening; the next successful launch will try again.
                Log.w("pige", "schedule background sync failed", error)
            }
        }
    }

    @JvmStatic
    fun getAppContext(): Context =
        appContext ?: throw IllegalStateException("PigeBridge.install was not called")

    /// The app's private files directory, where Rust keeps `pige.db` and the
    /// encrypted credential files.
    @JvmStatic
    fun dataDir(): String = getAppContext().filesDir.absolutePath

    /// The app's private cache directory, where Rust keeps downloaded
    /// attachment bytes.
    @JvmStatic
    fun cacheDir(): String = getAppContext().cacheDir.absolutePath

    @JvmStatic
    fun getPassword(account: String): String? =
        CredentialStore.get(getAppContext(), account)

    @JvmStatic
    fun setPassword(account: String, secret: String) {
        CredentialStore.set(getAppContext(), account, secret)
    }

    @JvmStatic
    fun deletePassword(account: String) {
        CredentialStore.delete(getAppContext(), account)
    }

    @JvmStatic
    fun openUrl(url: String) {
        val context = getAppContext()
        context.startActivity(
            Intent(Intent.ACTION_VIEW, Uri.parse(url))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }

    @JvmStatic
    fun openFile(path: String) {
        val context = getAppContext()
        val uri = FileProvider.getUriForFile(
            context,
            "${context.packageName}.fileprovider",
            File(path),
        )
        val mimeType = context.contentResolver.getType(uri) ?: "application/octet-stream"
        context.startActivity(
            Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, mimeType)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
        )
    }

    @JvmStatic
    fun notify(account: String, thread: String, sender: String, subject: String) {
        NotificationPoster.post(getAppContext(), account, thread, sender, subject)
    }

    /// Replace the unique periodic sync with one that runs every `minutes`
    /// (clamped to WorkManager's 15-minute minimum) while connected.
    @JvmStatic
    fun scheduleSync(minutes: Int) {
        val interval = minutes.coerceAtLeast(MIN_SYNC_MINUTES).toLong()
        val request = PeriodicWorkRequestBuilder<SyncWorker>(interval, TimeUnit.MINUTES)
            .setConstraints(
                Constraints.Builder()
                    .setRequiredNetworkType(NetworkType.CONNECTED)
                    .build()
            )
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 1, TimeUnit.MINUTES)
            .build()
        WorkManager.getInstance(getAppContext()).enqueueUniquePeriodicWork(
            WORK_NAME,
            ExistingPeriodicWorkPolicy.UPDATE,
            request,
        )
    }
}
