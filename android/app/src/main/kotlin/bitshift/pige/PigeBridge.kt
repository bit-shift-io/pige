package bitshift.pige

// Adapted from qlaunch (bitshift, MIT).
import android.content.ContentUris
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Environment
import android.provider.MediaStore
import android.provider.Settings
import androidx.core.content.FileProvider
import org.json.JSONArray
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
    private const val WORK_NAME = "pige-sync"

    private var appContext: Context? = null

    /// Install the application context. Background sync is scheduled by Rust
    /// from the persisted settings after the core initializes (see
    /// `nativeInit`), not from a hardcoded default here.
    @JvmStatic
    fun install(context: Context) {
        appContext = context.applicationContext
    }

    /// Worker entry point: installs the context without re-enqueueing the
    /// periodic work from inside its own run.
    @JvmStatic
    fun installForWorker(context: Context) {
        install(context)
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

    /// The app-private directory holding `pige.json`.
    @JvmStatic
    fun configDir(): String = File(getAppContext().filesDir, "config").absolutePath

    /// Whether this install holds "All files access" (`MANAGE_EXTERNAL_STORAGE`).
    /// Android attributes a Downloads file to the install that wrote it, so a
    /// reflash hides an earlier install's backups from the scoped MediaStore
    /// query until this is granted.
    @JvmStatic
    fun hasAllFilesAccess(): Boolean = Environment.isExternalStorageManager()

    /// Opens the system "All files access" screen for this app. There is no
    /// result: when the user grants it and returns, Restore is checked again.
    @JvmStatic
    fun requestAllFilesAccess() {
        val context = getAppContext()
        val perApp = Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION)
            .setData(Uri.parse("package:${context.packageName}"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            context.startActivity(perApp)
        } catch (e: Exception) {
            // Some builds only expose the global list, not the per-app page.
            context.startActivity(
                Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            )
        }
    }

    private fun downloadsCollection(): Uri =
        MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)

    /// Writes a config backup into the shared Downloads collection, visible in
    /// Files and any other app. The name is validated by Rust before it gets
    /// here. MediaStore needs no storage permission for the app's own files.
    @JvmStatic
    fun writeBackup(fileName: String, body: String) {
        val resolver = getAppContext().contentResolver
        val collection = downloadsCollection()
        resolver.delete(
            collection,
            "${MediaStore.Downloads.DISPLAY_NAME} = ?",
            arrayOf(fileName),
        )
        val values = ContentValues().apply {
            put(MediaStore.Downloads.DISPLAY_NAME, fileName)
            put(MediaStore.Downloads.MIME_TYPE, "application/json")
            put(MediaStore.Downloads.IS_PENDING, 1)
        }
        val uri = resolver.insert(collection, values) ?: error("Could not create $fileName")
        try {
            resolver.openOutputStream(uri)?.use { it.write(body.toByteArray(Charsets.UTF_8)) }
                ?: error("Could not write $fileName")
            values.clear()
            values.put(MediaStore.Downloads.IS_PENDING, 0)
            resolver.update(uri, values, null, null)
        } catch (error: Exception) {
            resolver.delete(uri, null, null)
            throw error
        }
    }

    /// The names of this app's config backups in Downloads, as a JSON array.
    /// Rust filters the names to the exact backup shape and sorts them.
    @JvmStatic
    fun listBackups(): String {
        val resolver = getAppContext().contentResolver
        val names = JSONArray()
        resolver.query(
            downloadsCollection(),
            arrayOf(MediaStore.Downloads.DISPLAY_NAME),
            "${MediaStore.Downloads.DISPLAY_NAME} LIKE ?",
            arrayOf("pige-backup-%"),
            null,
        )?.use { cursor ->
            val index = cursor.getColumnIndexOrThrow(MediaStore.Downloads.DISPLAY_NAME)
            while (cursor.moveToNext()) names.put(cursor.getString(index))
        }
        return names.toString()
    }

    /// Reads one config backup back as text. Throws if it is not there.
    @JvmStatic
    fun readBackup(fileName: String): String {
        val resolver = getAppContext().contentResolver
        val collection = downloadsCollection()
        resolver.query(
            collection,
            arrayOf(MediaStore.Downloads._ID),
            "${MediaStore.Downloads.DISPLAY_NAME} = ?",
            arrayOf(fileName),
            null,
        )?.use { cursor ->
            if (cursor.moveToFirst()) {
                val id = cursor.getLong(cursor.getColumnIndexOrThrow(MediaStore.Downloads._ID))
                val uri = ContentUris.withAppendedId(collection, id)
                resolver.openInputStream(uri)?.use { stream ->
                    return stream.readBytes().toString(Charsets.UTF_8)
                }
            }
        }
        error("No backup named $fileName")
    }

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

    /// Cancel the unique periodic sync, for when the user turns background sync
    /// off. Safe to call when no work is enqueued.
    @JvmStatic
    fun cancelSync() {
        WorkManager.getInstance(getAppContext()).cancelUniqueWork(WORK_NAME)
    }
}
