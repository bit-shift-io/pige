package bitshift.pige

// Adapted from qlaunch (bitshift, MIT).
import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.PixelFormat
import android.os.Bundle
import io.github.santuzius.icedandroid.IcedActivity

class MainActivity : IcedActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Install the bridge and the core before NativeActivity starts
        // android_main. Loading on the Java thread lets JNI_OnLoad cache the
        // PigeBridge class for every later callback thread.
        System.loadLibrary("pige")
        PigeBridge.install(applicationContext)
        nativeInit()
        super.onCreate(savedInstanceState)
        // NativeActivity.onCreate resets this to RGB_565. Set it afterwards,
        // before the surface is created, or Vulkan's inherited alpha is opaque.
        window.setFormat(PixelFormat.RGBA_8888)
        requestNotificationPermission()
        forwardNotificationTap(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        forwardNotificationTap(intent)
    }

    /// Forward a notification's account/thread extras exactly once, then drop
    /// them so a configuration change or recreation cannot reopen the reader.
    private fun forwardNotificationTap(intent: Intent?) {
        val account = intent?.getStringExtra(EXTRA_ACCOUNT) ?: return
        val thread = intent.getStringExtra(EXTRA_THREAD) ?: return
        intent.removeExtra(EXTRA_ACCOUNT)
        intent.removeExtra(EXTRA_THREAD)
        nativeNotificationTap(account, thread)
    }

    private fun requestNotificationPermission() {
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(
                arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                REQUEST_NOTIFICATIONS,
            )
        }
    }

    // Older back dispatch; IcedActivity handles predictive back through iced.
    @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
    override fun onBackPressed() {
        nativeBack()
    }

    private external fun nativeInit()
    private external fun nativeBack()
    private external fun nativeNotificationTap(account: String, thread: String)

    // Retained for the pre-existing Rust export; nativeBack is the entry point
    // used now. Both dispatch the same Back event.
    @Suppress("unused")
    private external fun onBackPressedEvent()

    companion object {
        const val EXTRA_ACCOUNT = "account"
        const val EXTRA_THREAD = "thread"
        private const val REQUEST_NOTIFICATIONS = 1
    }
}
