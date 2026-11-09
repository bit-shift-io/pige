//! Message body + attachment encryption/decryption.

use std::io::Read;

use super::keys::AddressKeys;
use crate::error::{Error, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use proton_crypto::crypto::{
    DataEncoding, Decryptor, DecryptorSync, Encryptor, EncryptorSync, PGPMessage, PGPProviderSync,
    SessionKeyAlgorithm, Signer, SignerSync, VerificationError, VerifiedDataReader,
};
use zeroize::Zeroizing;

/// Signature verification verdict for a decrypted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// A valid signature was verified against the provided sender keys.
    Verified,
    /// The message carried no signature.
    Unsigned,
    /// Signature presence could not be checked (no verification keys supplied).
    Unverified,
    /// A signature was present but failed verification.
    Invalid,
}

/// A bounded decryption failure, classified so callers can keep an integrity
/// failure distinct from an unsupported format or an over-limit payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecryptFailure {
    /// The decrypted output exceeded the caller's limit before it was fully
    /// materialized.
    TooLarge {
        /// The caller's plaintext byte cap.
        limit: usize,
    },
    /// The encrypted packet stream was corrupt or failed its integrity check.
    Integrity(String),
    /// The ciphertext could not be decrypted with any supplied key (or used a
    /// format this backend does not implement).
    Crypto(String),
}

impl std::fmt::Display for DecryptFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { limit } => {
                write!(formatter, "decrypted data exceeds the {limit} byte limit")
            }
            Self::Integrity(detail) => {
                write!(formatter, "encrypted data failed its integrity check: {detail}")
            }
            Self::Crypto(detail) => write!(formatter, "decryption failed: {detail}"),
        }
    }
}

impl std::error::Error for DecryptFailure {}

impl From<DecryptFailure> for Error {
    fn from(failure: DecryptFailure) -> Self {
        match failure {
            DecryptFailure::TooLarge { .. } => Error::Other(failure.to_string()),
            DecryptFailure::Integrity(detail) | DecryptFailure::Crypto(detail) => {
                Error::Crypto(detail)
            }
        }
    }
}

/// Classify a rustpgp error string. The backend surfaces SEIPD/MDC failures as
/// generic IO/decryption errors, so keep the integrity wording distinct from a
/// format or key-selection failure.
fn classify_crypto_error(error: impl std::fmt::Display) -> DecryptFailure {
    let detail = error.to_string();
    let lower = detail.to_ascii_lowercase();
    if lower.contains("modification detection") || lower.contains("mdc") {
        DecryptFailure::Integrity(detail)
    } else {
        DecryptFailure::Crypto(detail)
    }
}

/// Read at most `max_output + 1` bytes so an expanding (compressed) payload is
/// rejected as soon as it crosses the cap instead of being fully materialized.
fn read_bounded(
    reader: &mut impl Read,
    max_output: usize,
) -> std::result::Result<Vec<u8>, DecryptFailure> {
    let mut out = Vec::new();
    let mut limited = reader.take((max_output as u64).saturating_add(1));
    limited
        .read_to_end(&mut out)
        .map_err(|error| DecryptFailure::Integrity(error.to_string()))?;
    if out.len() > max_output {
        return Err(DecryptFailure::TooLarge { limit: max_output });
    }
    Ok(out)
}

/// A serialized session key (exported bytes + algorithm) so it can survive
/// between the encrypt step and per-recipient wrapping.
#[derive(Clone)]
pub struct SessionKeyMaterial {
    /// Raw session-key bytes (zeroized on drop).
    pub key: Zeroizing<Vec<u8>>,
    /// The session key's symmetric algorithm.
    pub algo: SessionKeyAlgorithm,
}

/// Result of encrypting an outbound attachment.
pub struct UploadedAttachment {
    /// Key packet wrapped to the sender's own address key.
    pub key_packet: Vec<u8>,
    /// Symmetric data packet.
    pub data_packet: Vec<u8>,
    /// Detached signature over the plaintext (binary).
    pub signature: Vec<u8>,
    /// Session key material (to re-wrap per recipient at send time).
    pub material: SessionKeyMaterial,
}

fn import_privs<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
) -> Result<Vec<P::PrivateKey>> {
    let mut out = Vec::new();
    for k in &addr.keys {
        use secrecy::ExposeSecret;
        if let Ok(pk) = provider.private_key_import(
            k.armored.as_bytes(),
            k.passphrase.expose_secret().as_bytes(),
            DataEncoding::Armor,
        ) {
            out.push(pk);
        }
    }
    if out.is_empty() {
        return Err(Error::Crypto("no usable private key for address".into()));
    }
    Ok(out)
}

/// Decrypt a message body; signature failures are non-fatal (returned as a verdict).
pub fn decrypt_body<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    verify_pubs_armored: &[String],
    armored_body: &str,
) -> Result<(String, Verdict)> {
    let (bytes, verdict) =
        decrypt_body_bounded(provider, addr, verify_pubs_armored, armored_body, usize::MAX)?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), verdict))
}

/// Decrypt a message body while refusing to return plaintext above
/// `max_output` bytes. The caller still bounds the encrypted input; this guard
/// stops a decompression expansion from becoming a cache/heap growth vector.
pub fn decrypt_body_bounded<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    verify_pubs_armored: &[String],
    armored_body: &str,
    max_output: usize,
) -> std::result::Result<(Vec<u8>, Verdict), DecryptFailure> {
    let privs = import_privs(provider, addr)
        .map_err(|error| DecryptFailure::Crypto(error.to_string()))?;
    let pubs: Vec<P::PublicKey> = verify_pubs_armored
        .iter()
        .filter_map(|a| {
            provider
                .public_key_import(a.as_bytes(), DataEncoding::Armor)
                .ok()
        })
        .collect();
    tracing::debug!(target: "proton_core::crypto", address = %addr.email, decrypt_keys = privs.len(), verify_keys = pubs.len(), armored_len = armored_body.len(), "decrypting message body");

    let mut dec = provider.new_decryptor().with_decryption_keys(privs.iter());
    let have_verifier = !pubs.is_empty();
    if have_verifier {
        dec = dec.with_verification_keys(pubs.iter());
    }
    let mut reader = dec
        .decrypt_stream(armored_body.as_bytes(), DataEncoding::Armor)
        .map_err(classify_crypto_error)?;
    // The reader decrypts and decompresses on demand, so `take(max+1)` stops a
    // compression bomb after one byte over the cap rather than materializing
    // the whole expansion first.
    let body = read_bounded(&mut reader, max_output)?;

    let verdict = if !have_verifier {
        Verdict::Unverified
    } else {
        match reader.verification_result() {
            Ok(_) => Verdict::Verified,
            Err(VerificationError::NotSigned(_)) => Verdict::Unsigned,
            Err(VerificationError::NoVerifier(_)) => Verdict::Unverified,
            Err(_) => Verdict::Invalid,
        }
    };
    tracing::debug!(target: "proton_core::crypto", verdict = ?verdict, plaintext_len = body.len(), "message body decrypted");
    Ok((body, verdict))
}

/// Decrypt an attachment: recover the session key from its key packet, then
/// symmetric-decrypt the data packet.
pub fn decrypt_attachment<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    key_packets_b64: &str,
    data_packet: &[u8],
) -> Result<Vec<u8>> {
    Ok(decrypt_attachment_bounded(
        provider,
        addr,
        key_packets_b64,
        data_packet,
        usize::MAX,
    )?)
}

/// Decrypt an attachment while refusing plaintext above `max_output` bytes.
pub fn decrypt_attachment_bounded<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    key_packets_b64: &str,
    data_packet: &[u8],
    max_output: usize,
) -> std::result::Result<Vec<u8>, DecryptFailure> {
    let privs = import_privs(provider, addr)
        .map_err(|error| DecryptFailure::Crypto(error.to_string()))?;
    tracing::debug!(target: "proton_core::crypto", address = %addr.email, data_packet = data_packet.len(), "decrypting attachment (unwrap session key → decrypt data packet)");
    let kp = STANDARD
        .decode(key_packets_b64)
        .map_err(|e| DecryptFailure::Crypto(format!("bad attachment key packet: {e}")))?;
    let sk = provider
        .new_decryptor()
        .with_decryption_keys(privs.iter())
        .decrypt_session_key(&kp)
        .map_err(|e| DecryptFailure::Crypto(format!("attachment session key decrypt failed: {e}")))?;
    let mut reader = provider
        .new_decryptor()
        .with_session_key(sk)
        .decrypt_stream(data_packet, DataEncoding::Bytes)
        .map_err(classify_crypto_error)?;
    // Bound the decompressed attachment before materializing it, matching the
    // body path so an encrypted attachment bomb cannot exhaust memory.
    read_bounded(&mut reader, max_output)
}

/// Encrypt + sign a draft body to the sender's own address key (armored).
pub fn encrypt_self_draft<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    body: &str,
) -> Result<String> {
    let privs = import_privs(provider, addr)?;
    tracing::debug!(target: "proton_core::crypto", address = %addr.email, body_len = body.len(), "encrypting + signing draft body to self");
    let pubk = provider
        .private_key_to_public_key(&privs[0])
        .map_err(|e| Error::Crypto(format!("derive pubkey: {e}")))?;
    let msg = provider
        .new_encryptor()
        .with_encryption_key(&pubk)
        .with_signing_key(&privs[0])
        .with_utf8()
        .encrypt(body.as_bytes())
        .map_err(|e| Error::Crypto(format!("draft encrypt failed: {e}")))?;
    let armored = msg
        .armor()
        .map_err(|e| Error::Crypto(format!("armor failed: {e}")))?;
    String::from_utf8(armored).map_err(|e| Error::Crypto(format!("armor utf8: {e}")))
}

/// Encrypt a body for transport with a fresh session key (data packet only,
/// signed by the sender). Returns the session-key material + the data packet.
pub fn encrypt_for_transport<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    body: &str,
) -> Result<(SessionKeyMaterial, Vec<u8>)> {
    let privs = import_privs(provider, addr)?;
    tracing::debug!(target: "proton_core::crypto", address = %addr.email, body_len = body.len(), "encrypting transport body (fresh AES-256 session key, signed, no key packet)");
    let sk = provider
        .session_key_generate(SessionKeyAlgorithm::Aes256)
        .map_err(|e| Error::Crypto(format!("session key gen: {e}")))?;
    let material = {
        let (bytes, algo) = provider
            .session_key_export(&sk)
            .map_err(|e| Error::Crypto(format!("sk export: {e}")))?;
        SessionKeyMaterial {
            key: Zeroizing::new(bytes.as_ref().to_vec()),
            algo,
        }
    };
    let data_packet = provider
        .new_encryptor()
        .with_session_key(sk)
        .with_signing_key(&privs[0])
        .with_utf8()
        .encrypt_raw(body.as_bytes(), DataEncoding::Bytes)
        .map_err(|e| Error::Crypto(format!("transport body encrypt failed: {e}")))?;
    Ok((material, data_packet))
}

/// Encrypt an outbound attachment (data packet + key packet to self + detached sig).
pub fn encrypt_attachment<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    bytes: &[u8],
) -> Result<UploadedAttachment> {
    let privs = import_privs(provider, addr)?;
    tracing::debug!(target: "proton_core::crypto", address = %addr.email, bytes = bytes.len(), "encrypting attachment (session key + data packet + detached signature)");
    let selfpub = provider
        .private_key_to_public_key(&privs[0])
        .map_err(|e| Error::Crypto(format!("derive pubkey: {e}")))?;
    let sk = provider
        .session_key_generate(SessionKeyAlgorithm::Aes256)
        .map_err(|e| Error::Crypto(format!("session key gen: {e}")))?;
    let material = {
        let (skb, algo) = provider
            .session_key_export(&sk)
            .map_err(|e| Error::Crypto(format!("sk export: {e}")))?;
        SessionKeyMaterial {
            key: Zeroizing::new(skb.as_ref().to_vec()),
            algo,
        }
    };
    let data_packet = provider
        .new_encryptor()
        .with_session_key(sk)
        .encrypt_raw(bytes, DataEncoding::Bytes)
        .map_err(|e| Error::Crypto(format!("attachment encrypt failed: {e}")))?;
    let signature = provider
        .new_signer()
        .with_signing_key(&privs[0])
        .sign_detached(bytes, DataEncoding::Bytes)
        .map_err(|e| Error::Crypto(format!("attachment sign failed: {e}")))?;
    let key_packet = {
        let imported = provider
            .session_key_import(&material.key, material.algo)
            .map_err(|e| Error::Crypto(format!("sk import: {e}")))?;
        provider
            .new_encryptor()
            .with_encryption_key(&selfpub)
            .encrypt_session_key(&imported)
            .map_err(|e| Error::Crypto(format!("wrap attachment sk: {e}")))?
    };
    Ok(UploadedAttachment {
        key_packet,
        data_packet,
        signature,
        material,
    })
}

/// Wrap a session key to a recipient's armored public key (→ a key packet).
pub fn wrap_session_key<P: PGPProviderSync>(
    provider: &P,
    recipient_pub_armored: &str,
    mat: &SessionKeyMaterial,
) -> Result<Vec<u8>> {
    tracing::trace!(target: "proton_core::crypto", "wrapping session key to a recipient public key");
    let sk = provider
        .session_key_import(&mat.key, mat.algo)
        .map_err(|e| Error::Crypto(format!("sk import: {e}")))?;
    let pubk = provider
        .public_key_import(recipient_pub_armored.as_bytes(), DataEncoding::Armor)
        .map_err(|e| Error::Crypto(format!("recipient pubkey import: {e}")))?;
    provider
        .new_encryptor()
        .with_encryption_key(&pubk)
        .encrypt_session_key(&sk)
        .map_err(|e| Error::Crypto(format!("wrap session key: {e}")))
}

/// Wrap a session key to the sender's own address key (→ the body key packet).
pub fn wrap_session_key_to_self<P: PGPProviderSync>(
    provider: &P,
    addr: &AddressKeys,
    mat: &SessionKeyMaterial,
) -> Result<Vec<u8>> {
    let privs = import_privs(provider, addr)?;
    let selfpub = provider
        .private_key_to_public_key(&privs[0])
        .map_err(|e| Error::Crypto(format!("derive pubkey: {e}")))?;
    let sk = provider
        .session_key_import(&mat.key, mat.algo)
        .map_err(|e| Error::Crypto(format!("sk import: {e}")))?;
    provider
        .new_encryptor()
        .with_encryption_key(&selfpub)
        .encrypt_session_key(&sk)
        .map_err(|e| Error::Crypto(format!("wrap session key to self: {e}")))
}

/// Re-key an attachment session key from one address ring to another (for
/// forwarding): decrypt the key packet with `src`, re-wrap to `dst`'s key.
pub fn rewrap_attachment_session_key<P: PGPProviderSync>(
    provider: &P,
    src: &AddressKeys,
    dst: &AddressKeys,
    key_packets_b64: &str,
) -> Result<Vec<u8>> {
    let src_privs = import_privs(provider, src)?;
    let dst_privs = import_privs(provider, dst)?;
    let dstpub = provider
        .private_key_to_public_key(&dst_privs[0])
        .map_err(|e| Error::Crypto(format!("derive pubkey: {e}")))?;
    let kp = STANDARD
        .decode(key_packets_b64)
        .map_err(|e| Error::Crypto(format!("bad key packet: {e}")))?;
    let sk = provider
        .new_decryptor()
        .with_decryption_keys(src_privs.iter())
        .decrypt_session_key(&kp)
        .map_err(|e| Error::Crypto(format!("decrypt att session key: {e}")))?;
    provider
        .new_encryptor()
        .with_encryption_key(&dstpub)
        .encrypt_session_key(&sk)
        .map_err(|e| Error::Crypto(format!("rewrap att session key: {e}")))
}

/// Generate a fresh AES-256 session key; return its material + base64 token.
pub fn new_session_key<P: PGPProviderSync>(provider: &P) -> Result<(SessionKeyMaterial, String)> {
    let sk = provider
        .session_key_generate(SessionKeyAlgorithm::Aes256)
        .map_err(|e| Error::Crypto(format!("session key gen: {e}")))?;
    let (bytes, algo) = provider
        .session_key_export(&sk)
        .map_err(|e| Error::Crypto(format!("sk export: {e}")))?;
    let token = STANDARD.encode(bytes.as_ref());
    Ok((
        SessionKeyMaterial {
            key: Zeroizing::new(bytes.as_ref().to_vec()),
            algo,
        },
        token,
    ))
}

/// Symmetric-encrypt text under a password (armored) — the EO `EncToken`.
pub fn encrypt_text_with_password<P: PGPProviderSync>(
    provider: &P,
    password: &str,
    text: &str,
) -> Result<String> {
    let msg = provider
        .new_encryptor()
        .with_passphrase(password)
        .with_utf8()
        .encrypt(text.as_bytes())
        .map_err(|e| Error::Crypto(format!("password encrypt: {e}")))?;
    let armored = msg
        .armor()
        .map_err(|e| Error::Crypto(format!("armor: {e}")))?;
    String::from_utf8(armored).map_err(|e| Error::Crypto(format!("armor utf8: {e}")))
}

/// Wrap a session key under a password → a password-protected key packet
/// (the EO `BodyKeyPacket`).
pub fn wrap_session_key_with_password<P: PGPProviderSync>(
    provider: &P,
    password: &str,
    mat: &SessionKeyMaterial,
) -> Result<Vec<u8>> {
    let sk = provider
        .session_key_import(&mat.key, mat.algo)
        .map_err(|e| Error::Crypto(format!("sk import: {e}")))?;
    provider
        .new_encryptor()
        .with_passphrase(password)
        .encrypt_session_key(&sk)
        .map_err(|e| Error::Crypto(format!("password-wrap session key: {e}")))
}

/// Map a session-key algorithm to its API string.
pub fn algo_name(algo: SessionKeyAlgorithm) -> &'static str {
    match algo {
        SessionKeyAlgorithm::Aes256 => "aes256",
        SessionKeyAlgorithm::Aes128 => "aes128",
        _ => "aes256",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::{AddressKeys, StoredKey};
    use proton_crypto::crypto::{
        AccessKeyInfo, KeyGenerator, KeyGeneratorAlgorithm, KeyGeneratorSync, PGPProviderSync,
        SessionKey, VerifiedData, Verifier, VerifierSync,
    };
    use secrecy::SecretString;

    /// Generate a fresh address key; return (AddressKeys, its armored public key).
    fn make_address<P: PGPProviderSync>(provider: &P, email: &str) -> (AddressKeys, String) {
        let pass = "test-passphrase";
        let priv_key = provider
            .new_key_generator()
            .with_user_id("Test", email)
            .with_algorithm(KeyGeneratorAlgorithm::ECC)
            .generate()
            .unwrap();
        let locked = provider
            .private_key_export(&priv_key, pass, DataEncoding::Armor)
            .unwrap()
            .as_ref()
            .to_vec();
        let armored = String::from_utf8(locked).unwrap();
        let pubk = provider.private_key_to_public_key(&priv_key).unwrap();
        let pub_armored = String::from_utf8(
            provider
                .public_key_export(&pubk, DataEncoding::Armor)
                .unwrap()
                .as_ref()
                .to_vec(),
        )
        .unwrap();
        let _ = pubk.key_id();
        let ak = AddressKeys {
            address_id: "addr1".into(),
            email: email.into(),
            keys: vec![StoredKey {
                armored,
                passphrase: SecretString::from(pass.to_string()),
            }],
        };
        (ak, pub_armored)
    }

    #[test]
    fn self_draft_roundtrip_verifies() {
        let provider = proton_crypto::new_pgp_provider();
        let (addr, pub_armored) = make_address(&provider, "me@proton.me");
        let armored = encrypt_self_draft(&provider, &addr, "hello body").unwrap();
        let (body, verdict) = decrypt_body(&provider, &addr, &[pub_armored], &armored).unwrap();
        assert_eq!(body, "hello body");
        assert_eq!(verdict, Verdict::Verified);
    }

    #[test]
    fn unverified_when_no_sender_key() {
        let provider = proton_crypto::new_pgp_provider();
        let (addr, _pub) = make_address(&provider, "me@proton.me");
        let armored = encrypt_self_draft(&provider, &addr, "secret").unwrap();
        let (body, verdict) = decrypt_body(&provider, &addr, &[], &armored).unwrap();
        assert_eq!(body, "secret");
        assert_eq!(verdict, Verdict::Unverified);
    }

    /// S3 finding: a body signed by key A but verified against a *different*
    /// key B surfaces as `Unverified` (no verifier for that key id), not
    /// `Invalid`. `Invalid` is reachable only for a same-key-id, tampered
    /// signature, which the public `decrypt_body` API does not let us craft.
    #[test]
    fn unknown_signer_is_unverified_not_verified() {
        let provider = proton_crypto::new_pgp_provider();
        let (addr_a, _pub_a) = make_address(&provider, "a@proton.me");
        let (_addr_b, pub_b) = make_address(&provider, "b@proton.me");
        let armored = encrypt_self_draft(&provider, &addr_a, "signed by a").unwrap();
        let (_body, verdict) = decrypt_body(&provider, &addr_a, &[pub_b], &armored).unwrap();
        assert_eq!(verdict, Verdict::Unverified);
        assert_ne!(verdict, Verdict::Verified);
    }

    /// The underlying primitive *does* reject a bad detached signature, so the
    /// `Invalid` verdict has a real signal source (used for address-key token
    /// signatures); this is provider-level, not the integrated body path.
    #[test]
    fn provider_rejects_bad_detached_signature() {
        let provider = proton_crypto::new_pgp_provider();
        let (_addr_a, pub_a) = make_address(&provider, "a@proton.me");
        let pubk = provider
            .public_key_import(pub_a.as_bytes(), DataEncoding::Armor)
            .unwrap();
        // A zero-length detached signature must be rejected, not accepted.
        let result = provider
            .new_verifier()
            .with_verification_keys([&pubk])
            .verify_detached(b"data", b"", DataEncoding::Armor);
        assert!(result.is_err(), "empty detached signature must not verify");
    }

    #[test]
    fn unsigned_body_is_unsigned_not_verified() {
        // Encrypt only (no signing key) then decrypt with a verifier supplied.
        let provider = proton_crypto::new_pgp_provider();
        let (addr, pub_armored) = make_address(&provider, "me@proton.me");
        let pubk = provider
            .public_key_import(pub_armored.as_bytes(), DataEncoding::Armor)
            .unwrap();
        let msg = provider
            .new_encryptor()
            .with_encryption_key(&pubk)
            .with_utf8()
            .encrypt(b"unsigned body")
            .unwrap();
        let armored = String::from_utf8(msg.armor().unwrap()).unwrap();
        let (body, verdict) = decrypt_body(&provider, &addr, &[pub_armored], &armored).unwrap();
        assert_eq!(body, "unsigned body");
        assert_eq!(verdict, Verdict::Unsigned);
    }

    #[test]
    fn attachment_roundtrip() {
        let provider = proton_crypto::new_pgp_provider();
        let (addr, _pub) = make_address(&provider, "me@proton.me");
        let data = b"attachment payload bytes";
        let up = encrypt_attachment(&provider, &addr, data).unwrap();
        let kp_b64 = STANDARD.encode(&up.key_packet);
        let out = decrypt_attachment(&provider, &addr, &kp_b64, &up.data_packet).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn password_session_key_and_text_roundtrip() {
        let provider = proton_crypto::new_pgp_provider();
        let pw = "s3cret-eo";
        // EncToken: text under password.
        let armored = encrypt_text_with_password(&provider, pw, "hello eo").unwrap();
        let dec = provider
            .new_decryptor()
            .with_passphrase(pw)
            .decrypt(armored.as_bytes(), DataEncoding::Armor)
            .unwrap();
        assert_eq!(dec.as_bytes(), b"hello eo");
        // BodyKeyPacket: session key under password.
        let (mat, token) = new_session_key(&provider).unwrap();
        assert!(!token.is_empty());
        let kp = wrap_session_key_with_password(&provider, pw, &mat).unwrap();
        let sk = provider
            .new_decryptor()
            .with_passphrase(pw)
            .decrypt_session_key(&kp)
            .unwrap();
        assert_eq!(sk.export().as_ref(), &mat.key[..]);
    }

    #[test]
    fn transport_body_wraps_and_decrypts() {
        let provider = proton_crypto::new_pgp_provider();
        let (addr, _pub) = make_address(&provider, "me@proton.me");
        let (mat, data_packet) = encrypt_for_transport(&provider, &addr, "transport body").unwrap();
        let key_packet = wrap_session_key_to_self(&provider, &addr, &mat).unwrap();
        // Reassemble as an attachment-style decrypt to prove the packets pair up.
        let kp_b64 = STANDARD.encode(&key_packet);
        let out = decrypt_attachment(&provider, &addr, &kp_b64, &data_packet).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "transport body");
    }
}
