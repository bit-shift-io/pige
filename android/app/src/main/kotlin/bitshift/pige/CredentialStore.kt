package bitshift.pige

// Adapted from qlaunch (bitshift, MIT): Rust owns all data; Kotlin only performs
// OS operations. Credentials are encrypted with a non-exportable Android
// Keystore AES/GCM key before they touch the filesystem.
import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import java.io.File
import java.io.IOException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/// Android Keystore credential storage.
///
/// Each account is one file, `filesDir/credentials/<account>.bin`, containing
/// the GCM IV followed by the ciphertext. The AES key never leaves the Keystore,
/// so the file is useless without the app's Keystore entry. A Keystore failure
/// throws; Rust turns that into a `CredentialsUnavailable` message so the UI can
/// offer a session-only password instead of writing plaintext.
object CredentialStore {
    private const val DIRECTORY = "credentials"
    private const val KEY_ALIAS = "pige-credentials"
    private const val KEYSTORE = "AndroidKeyStore"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128
    private const val KEY_BITS = 256

    // Account ids are UUID-shaped. Reject anything that could escape the
    // credentials directory even if a future account type uses another id.
    private val ACCOUNT_ID = Regex("[A-Za-z0-9_-]{1,128}")

    @Synchronized
    fun get(context: Context, account: String): String? {
        val file = fileFor(context, account)
        if (!file.isFile) return null
        val bytes = AtomicFile(file).readFully()
        if (bytes.size <= IV_BYTES) {
            throw IOException("credential file is truncated")
        }
        val iv = bytes.copyOfRange(0, IV_BYTES)
        val ciphertext = bytes.copyOfRange(IV_BYTES, bytes.size)
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, iv))
        return String(cipher.doFinal(ciphertext), Charsets.UTF_8)
    }

    @Synchronized
    fun set(context: Context, account: String, secret: String) {
        val cipher = Cipher.getInstance(TRANSFORMATION)
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val ciphertext = cipher.doFinal(secret.toByteArray(Charsets.UTF_8))

        val file = fileFor(context, account)
        file.parentFile?.mkdirs()
        val atomic = AtomicFile(file)
        val out = atomic.startWrite()
        try {
            out.write(cipher.iv)
            out.write(ciphertext)
            atomic.finishWrite(out)
        } catch (error: Throwable) {
            atomic.failWrite(out)
            throw error
        }
    }

    @Synchronized
    fun delete(context: Context, account: String) {
        val file = fileFor(context, account)
        // AtomicFile can leave `.bak`/`.new` siblings behind. Removing only the
        // base name would let `readFully` resurrect a deleted secret.
        for (candidate in listOf(file, File(file.path + ".bak"), File(file.path + ".new"))) {
            if (candidate.exists() && !candidate.delete()) {
                throw IOException("could not delete ${candidate.name}")
            }
        }
    }

    private fun fileFor(context: Context, account: String): File {
        require(ACCOUNT_ID.matches(account)) { "invalid account id" }
        return File(File(context.filesDir, DIRECTORY), "$account.bin")
    }

    private fun key(): SecretKey {
        val keyStore = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (keyStore.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let {
            return it.secretKey
        }

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(KEY_BITS)
                .build()
        )
        return generator.generateKey()
    }
}
