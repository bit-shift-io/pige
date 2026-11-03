package bitshift.pige

// Adapted from qlaunch (bitshift, MIT).
import android.graphics.PixelFormat
import android.os.Bundle
import io.github.santuzius.icedandroid.IcedActivity

class MainActivity : IcedActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Install the bridge before NativeActivity starts android_main.
        // Loading on the Java thread lets the native library resolve the class.
        System.loadLibrary("pige")
        PigeBridge.install(applicationContext)
        super.onCreate(savedInstanceState)
        // NativeActivity.onCreate resets this to RGB_565. Set it afterwards,
        // before the surface is created, or Vulkan's inherited alpha is opaque.
        window.setFormat(PixelFormat.RGBA_8888)
    }

    // Older back dispatch; IcedActivity handles predictive back through iced.
    @Suppress("DEPRECATION", "OVERRIDE_DEPRECATION")
    override fun onBackPressed() {
        onBackPressedEvent()
    }

    private external fun onBackPressedEvent()
}
