//! Secrets at rest: sealed credentials, password hashes, session tokens.
//!
//! # Threat model
//!
//! The database holds router passwords and a GitHub token, which are the keys to
//! someone's network. So it must not hold them in a readable form: a `pg_dump`,
//! a stolen volume snapshot or a careless backup of the database is assumed to
//! happen sooner or later.
//!
//! Everything sensitive is therefore sealed with XChaCha20-Poly1305 under a key
//! that lives **outside** the database, supplied as `DONDUDE_MASTER_KEY`. The
//! process refuses to start without it — a silent fallback to plaintext would be
//! the worst possible outcome, since nothing would appear broken.
//!
//! XChaCha20 (24-byte nonce) rather than ChaCha20 (12-byte) so that nonces can
//! be drawn at random per message without having to track a counter.
//!
//! Two things are deliberately *not* sealed:
//!
//! * Operator login passwords are hashed with Argon2id, not encrypted — nothing
//!   ever needs to read them back.
//! * Session cookies are stored as a SHA-256 digest. The token is 256 random
//!   bits, so there is no dictionary to attack and a plain digest is enough;
//!   Argon2 on every request would only buy latency.

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Environment variable holding the base64 master key.
pub const MASTER_KEY_ENV: &str = "DONDUDE_MASTER_KEY";

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// The key that protects every stored credential.
pub struct MasterKey([u8; KEY_LEN]);

// Never let the key material reach a log line or a panic message.
impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    /// Load the key from the environment.
    pub fn from_env() -> Result<Self> {
        let encoded = std::env::var(MASTER_KEY_ENV).map_err(|_| {
            Error::Crypto(format!(
                "{MASTER_KEY_ENV} is not set. Generate one with `dondude keygen` and pass it to \
             the container; without it stored credentials cannot be read."
            ))
        })?;
        Self::from_base64(encoded.trim())
    }

    pub fn from_base64(encoded: &str) -> Result<Self> {
        let bytes = B64
            .decode(encoded)
            .map_err(|e| Error::Crypto(format!("{MASTER_KEY_ENV} is not valid base64: {e}")))?;
        let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| {
            Error::Crypto(format!(
                "{MASTER_KEY_ENV} must decode to exactly {KEY_LEN} bytes"
            ))
        })?;
        Ok(Self(bytes))
    }

    /// Mint a fresh key, base64-encoded for pasting into a compose file.
    pub fn generate() -> Result<String> {
        let mut bytes = [0u8; KEY_LEN];
        fill_random(&mut bytes)?;
        Ok(B64.encode(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::from(self.0))
    }

    /// Encrypt a secret for storage. Output is base64 of `nonce || ciphertext`.
    pub fn seal(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce)?;
        let ciphertext = self
            .cipher()
            .encrypt(&XNonce::from(nonce), plaintext.as_bytes())
            .map_err(|_| Error::Crypto("could not seal the secret".into()))?;

        let mut framed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        framed.extend_from_slice(&nonce);
        framed.extend_from_slice(&ciphertext);
        Ok(B64.encode(framed))
    }

    /// Decrypt a stored secret.
    ///
    /// Failure here almost always means the master key changed, so the message
    /// says so rather than reporting an opaque authentication error.
    pub fn open(&self, sealed: &str) -> Result<String> {
        let framed = B64
            .decode(sealed)
            .map_err(|e| Error::Crypto(format!("stored secret is not valid base64: {e}")))?;
        if framed.len() <= NONCE_LEN {
            return Err(Error::Crypto("stored secret is truncated".into()));
        }
        let (nonce, ciphertext) = framed.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce)
            .map_err(|_| Error::Crypto("stored secret has a malformed nonce".into()))?;
        let plaintext = self.cipher().decrypt(&nonce, ciphertext).map_err(|_| {
            Error::Crypto(format!(
                "could not decrypt a stored secret — is {MASTER_KEY_ENV} the same key that \
                     was used to save it?"
            ))
        })?;
        String::from_utf8(plaintext)
            .map_err(|_| Error::Crypto("decrypted secret is not valid UTF-8".into()))
    }
}

/// Hash an operator's login password (Argon2id, default parameters).
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    fill_random(&mut salt)?;
    let salt = SaltString::encode_b64(&salt)
        .map_err(|e| Error::Crypto(format!("could not encode a password salt: {e}")))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::Crypto(format!("could not hash the password: {e}")))?
        .to_string())
}

/// Check a login password against a stored PHC hash.
///
/// A malformed stored hash verifies as `false` rather than erroring: an
/// unreadable hash must not become a way to bypass the check.
pub fn verify_password(password: &str, stored: &str) -> bool {
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(error) => {
            tracing::warn!(%error, "stored password hash is unreadable");
            false
        }
    }
}

/// A fresh session token: 256 random bits, URL-safe, for the cookie value.
pub fn session_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes)?;
    Ok(B64URL.encode(bytes))
}

/// The digest stored in place of a session token.
pub fn token_digest(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    B64URL.encode(hasher.finalize())
}

fn fill_random(buffer: &mut [u8]) -> Result<()> {
    getrandom::fill(buffer)
        .map_err(|e| Error::Crypto(format!("the system random source failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::from_base64(&MasterKey::generate().unwrap()).unwrap()
    }

    #[test]
    fn a_sealed_secret_round_trips() {
        let key = key();
        let sealed = key.seal("s3cret-router-password").unwrap();
        assert_eq!(key.open(&sealed).unwrap(), "s3cret-router-password");
    }

    #[test]
    fn sealing_the_same_secret_twice_gives_different_ciphertext() {
        // Equal ciphertexts would leak which devices share a password.
        let key = key();
        assert_ne!(key.seal("same").unwrap(), key.seal("same").unwrap());
    }

    #[test]
    fn another_key_cannot_open_it_and_says_why() {
        let sealed = key().seal("secret").unwrap();
        let error = key().open(&sealed).unwrap_err().to_string();
        assert!(error.contains(MASTER_KEY_ENV), "unhelpful error: {error}");
    }

    #[test]
    fn tampering_with_the_ciphertext_is_detected() {
        let key = key();
        let sealed = key.seal("secret").unwrap();
        let mut raw = B64.decode(&sealed).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        assert!(key.open(&B64.encode(raw)).is_err());
    }

    #[test]
    fn truncated_and_malformed_input_is_rejected_not_panicked_on() {
        let key = key();
        assert!(key.open("").is_err());
        assert!(key.open("!!!not base64!!!").is_err());
        assert!(key.open(&B64.encode([0u8; 8])).is_err());
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        assert!(MasterKey::from_base64(&B64.encode([0u8; 16])).is_err());
        assert!(MasterKey::from_base64("not-base64!").is_err());
    }

    #[test]
    fn passwords_verify_only_against_themselves() {
        let hash = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("Correct horse", &hash));
        assert!(!verify_password("", &hash));
        // Two hashes of one password differ: the salt is per-hash.
        assert_ne!(hash, hash_password("correct horse").unwrap());
    }

    #[test]
    fn a_corrupt_stored_hash_denies_access() {
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn session_tokens_are_unique_and_digests_are_stable() {
        let a = session_token().unwrap();
        let b = session_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(token_digest(&a), token_digest(&a));
        assert_ne!(token_digest(&a), token_digest(&b));
        // The digest must not be the token itself.
        assert_ne!(token_digest(&a), a);
    }
}
