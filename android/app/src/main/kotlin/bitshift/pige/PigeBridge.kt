package bitshift.pige

// Adapted from qlaunch (bitshift, MIT).
import android.content.Context

// Everything the Rust core asks the OS for. For now it only stores the
// application context; the IMAP/SMTP bridges land in later tasks.
object PigeBridge {
    private var appContext: Context? = null

    @JvmStatic
    fun install(context: Context) {
        appContext = context.applicationContext
    }

    @JvmStatic
    fun getAppContext(): Context =
        appContext ?: throw IllegalStateException("PigeBridge.install was not called")
}
