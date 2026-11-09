package bitshift.pige

// S2 §9.4: Android Google authorization uses Google Identity Services
// `AuthorizationClient` from Google Play services. There is no custom URI
// scheme, OOB code, loopback redirect, or WebView. Consent returns through the
// platform PendingIntent; the result is bound to the Rust attempt id and sent
// back through the validated `GoogleAuthorization.nativeResult` JNI payload.
//
// Live consent return, Activity recreation, silent renewal, and revocation
// remain BLOCKED on a registered Android OAuth client and disposable account
// (docs/provider-feasibility.md §9.7). This helper is the documented scaffold;
// PigeBridge/MainActivity only launch it from the foreground account UI, never
// from the background sync worker.
import android.app.Activity
import android.content.Intent
import android.util.Log
import com.google.android.gms.auth.api.identity.AuthorizationClient
import com.google.android.gms.auth.api.identity.AuthorizationRequest
import com.google.android.gms.auth.api.identity.AuthorizationResult
import com.google.android.gms.auth.api.identity.Identity
import com.google.android.gms.common.api.ApiException
import com.google.android.gms.common.api.CommonStatusCodes
import com.google.android.gms.common.api.Scope
import org.json.JSONArray
import org.json.JSONObject

object GoogleAuthorization {
    const val REQUEST_CODE = 0x4741 // "GA"
    private const val TAG = "pige"

    // GIS reports no expiry field. Google access tokens are issued for about one
    // hour; use a conservative 55 minutes. The Rust token source refreshes on
    // demand before this bound.
    private const val ACCESS_TOKEN_LIFETIME_MS = 55L * 60L * 1000L

    // Survives an Activity configuration change in the same process. Process
    // death discards the Rust attempt too, so the user must start again.
    private var currentAttempt: String? = null
    private var client: AuthorizationClient? = null

    /**
     * Launch the supported Google consent flow. `serverClientId` is the Web
     * application client id and is supplied only when the user configured the
     * server-side `serverAuthCode` exchange; without it no device refresh token
     * is requested or stored.
     */
    fun authorize(
        activity: Activity,
        attempt: String,
        scopes: List<String>,
        serverClientId: String?,
    ) {
        currentAttempt = attempt
        val request = AuthorizationRequest.builder()
            .setRequestedScopes(scopes.map { Scope(it) })
            .apply {
                if (serverClientId != null) {
                    requestOfflineAccess(serverClientId)
                }
            }
            .build()

        val authorizationClient = Identity.getAuthorizationClient(activity)
        client = authorizationClient
        authorizationClient
            .authorize(request)
            .addOnSuccessListener { result ->
                if (result.hasResolution()) {
                    launchResolution(activity, attempt, result)
                } else {
                    finish(attempt, result)
                }
            }
            .addOnFailureListener { error ->
                Log.w(TAG, "Google authorization failed", error)
                deliverFailure(attempt, error)
            }
    }

    /** Handle the consent Activity result. Returns true when this helper owned it. */
    fun onActivityResult(
        activity: Activity,
        requestCode: Int,
        resultCode: Int,
        data: Intent?,
    ): Boolean {
        if (requestCode != REQUEST_CODE) return false
        val attempt = currentAttempt
        if (attempt == null) {
            Log.w(TAG, "Google authorization result without an active attempt")
            return true
        }
        if (resultCode != Activity.RESULT_OK || data == null) {
            currentAttempt = null
            deliverCancelled(attempt)
            return true
        }
        val result = try {
            (client ?: Identity.getAuthorizationClient(activity))
                .getAuthorizationResultFromIntent(data)
        } catch (error: Exception) {
            Log.w(TAG, "Google authorization result was invalid", error)
            currentAttempt = null
            deliverFailure(attempt, error)
            return true
        }
        if (result.hasResolution()) {
            currentAttempt = attempt
            launchResolution(activity, attempt, result)
        } else {
            currentAttempt = null
            finish(attempt, result)
        }
        return true
    }

    fun cancel() {
        currentAttempt = null
    }

    /** Cancel only the attempt named by Rust; a newer attempt is untouched. */
    fun cancel(attempt: String) {
        if (currentAttempt == attempt) {
            currentAttempt = null
        }
    }

    /**
     * Report that the platform UI could not be launched at all (for example no
     * foreground Activity). Rust turns this into a safe retryable message.
     */
    fun reportFailure(attempt: String) {
        if (currentAttempt == attempt) {
            currentAttempt = null
        }
        deliverFailure(attempt, IllegalStateException("no foreground activity"))
    }

    fun restoreAttempt(attempt: String?) {
        currentAttempt = attempt
    }

    fun savedAttempt(): String? = currentAttempt

    private fun launchResolution(
        activity: Activity,
        attempt: String,
        result: AuthorizationResult,
    ) {
        val pending = result.pendingIntent
        if (pending == null) {
            currentAttempt = null
            deliverFailure(attempt, IllegalStateException("missing consent resolution"))
            return
        }
        currentAttempt = attempt
        activity.startIntentSenderForResult(
            pending.intentSender,
            REQUEST_CODE,
            null,
            0,
            0,
            0,
        )
    }

    private fun finish(attempt: String, result: AuthorizationResult) {
        val token = result.accessToken
        if (token.isNullOrEmpty()) {
            deliverFailure(attempt, IllegalStateException("no access token"))
            return
        }
        val granted = result.grantedScopes
        // GIS carries the consented account on the result; without it Rust has
        // no email/name and labels the account the generic "Google account".
        val identity = JSONObject().apply {
            result.toGoogleSignInAccount()?.let { account ->
                account.id?.let { put("userId", it) }
                account.email?.let { put("email", it) }
                account.displayName?.let { put("displayName", it) }
            }
        }
        val payload = JSONObject()
            .put("attempt", attempt)
            .put(
                "result",
                JSONObject()
                    .put("kind", "authorized")
                    .put("accessToken", token)
                    .put("expiresAtMs", System.currentTimeMillis() + ACCESS_TOKEN_LIFETIME_MS)
                    .put("scopes", JSONArray(granted))
                    .put("identity", identity)
                    .apply {
                        result.serverAuthCode?.let { put("serverAuthCode", it) }
                    },
            )
            .toString()
        nativeResult(payload)
    }

    private fun deliverCancelled(attempt: String) {
        val payload = JSONObject()
            .put("attempt", attempt)
            .put("result", JSONObject().put("kind", "cancelled"))
            .toString()
        nativeResult(payload)
    }

    private fun deliverFailure(attempt: String, error: Throwable) {
        val cancelled = (error as? ApiException)?.statusCode == CommonStatusCodes.CANCELED
        if (cancelled) {
            deliverCancelled(attempt)
            return
        }
        // The host never sends provider text across the bridge; Rust chooses the
        // user-facing safe message.
        val payload = JSONObject()
            .put("attempt", attempt)
            .put("result", JSONObject().put("kind", "failed"))
            .toString()
        nativeResult(payload)
    }

    // Implemented in the Rust JNI bridge (`jni.rs`). It validates the frozen
    // payload and rejects unknown fields before queueing it for the active
    // GoogleAuthentication.
    private external fun nativeResult(payload: String)
}
