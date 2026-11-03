package bitshift.pige

// Adapted from qlaunch (bitshift, MIT): the worker starts a headless Rust
// process, syncs, and lets WorkManager decide when to retry.
import android.content.Context
import android.util.Log
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters

/// Periodic background sync. WorkManager may start this process without an
/// activity, so the worker loads the library, installs the bridge, and lets
/// `nativeSyncAll` lazily initialise the core before syncing.
class SyncWorker(context: Context, params: WorkerParameters) : CoroutineWorker(context, params) {
    override suspend fun doWork(): Result {
        return try {
            System.loadLibrary("pige")
            PigeBridge.installForWorker(applicationContext)
            when (nativeSyncAll()) {
                0 -> Result.success()
                else -> Result.retry()
            }
        } catch (error: Throwable) {
            Log.w("pige", "background sync failed", error)
            Result.retry()
        }
    }

    private external fun nativeSyncAll(): Int
}
