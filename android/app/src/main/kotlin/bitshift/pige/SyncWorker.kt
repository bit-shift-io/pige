package bitshift.pige

// Adapted from qlaunch (bitshift, MIT): the worker starts a headless Rust
// process, syncs, and lets WorkManager decide when to retry.
import android.content.Context
import android.util.Log
import androidx.work.Worker
import androidx.work.WorkerParameters

/// Periodic background maintenance. WorkManager may start this process without
/// an activity, so the worker loads the library, installs the bridge, and lets
/// `nativeSyncAll` lazily initialise the core before mail sync, contact sync and
/// bounded history learning. WorkManager's periodic minimum stays 15 minutes;
/// this is best-effort maintenance, not a promise of continuous background work.
///
/// A synchronous `Worker` (not `CoroutineWorker`) is deliberate: `onStopped` is
/// still overridable, so WorkManager stopping this run can reach the native
/// slice directly. The JNI call itself is blocking on WorkManager's worker
/// thread.
class SyncWorker(context: Context, params: WorkerParameters) : Worker(context, params) {
    override fun doWork(): Result {
        return try {
            System.loadLibrary("pige")
            PigeBridge.installForWorker(applicationContext)
            when (nativeSyncAll()) {
                0 -> Result.success()
                else -> Result.retry()
            }
        } catch (error: Throwable) {
            Log.w("pige", "background maintenance failed", error)
            Result.retry()
        }
    }

    /// WorkManager stopped or cancelled this run. Cancel the native slice so it
    /// exits at the next page boundary; the last committed checkpoint survives.
    override fun onStopped() {
        PigeBridge.cancelMaintenance()
        super.onStopped()
    }

    private external fun nativeSyncAll(): Int
}
