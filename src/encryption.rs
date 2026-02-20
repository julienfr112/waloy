use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use rand::RngCore;

use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"WEN\x01";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| Error::Other(format!("argon2 key derivation: {e}")))?;
    Ok(key)
}

/// Encrypt data with AES-256-GCM using a passphrase.
/// Output format: MAGIC (4) + salt (16) + nonce (12) + ciphertext+tag
pub fn encrypt(data: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = derive_key(passphrase, &salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| Error::Other(format!("aes-gcm init: {e}")))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, data)
        .map_err(|e| Error::Other(format!("encryption: {e}")))?;

    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt data. Auto-detects encrypted data via magic bytes.
/// Returns data unchanged if not encrypted.
pub fn decrypt(data: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    if data.len() < MAGIC.len() + SALT_LEN + NONCE_LEN || &data[..4] != MAGIC {
        return Ok(data.to_vec());
    }

    let salt = &data[4..4 + SALT_LEN];
    let nonce_bytes = &data[4 + SALT_LEN..4 + SALT_LEN + NONCE_LEN];
    let ciphertext = &data[4 + SALT_LEN + NONCE_LEN..];

    let key = derive_key(passphrase, salt)?;
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| Error::Other(format!("aes-gcm init: {e}")))?;
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| Error::Other(format!("decryption: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let data = b"hello world, this is a test of encryption";
        let passphrase = "my-secret-key";

        let encrypted = encrypt(data, passphrase).unwrap();
        assert_ne!(&encrypted, data);
        assert_eq!(&encrypted[..4], MAGIC);

        let decrypted = decrypt(&encrypted, passphrase).unwrap();
        assert_eq!(&decrypted, data);
    }

    #[test]
    fn test_decrypt_unencrypted_passthrough() {
        let data = b"plain text data";
        let result = decrypt(data, "any-key").unwrap();
        assert_eq!(&result, data);
    }

    #[test]
    fn test_wrong_passphrase_fails() {
        let data = b"secret data";
        let encrypted = encrypt(data, "correct-key").unwrap();
        let result = decrypt(&encrypted, "wrong-key");
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_non_encrypted_data_ge_32_bytes_passthrough() {
        // Data is >= MAGIC + SALT_LEN + NONCE_LEN (32 bytes) but has wrong magic
        let data = vec![0xAA; 64];
        let result = decrypt(&data, "any-key").unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn encrypt_decrypt_empty_data() {
        let encrypted = encrypt(b"", "key").unwrap();
        assert_eq!(&encrypted[..4], MAGIC);
        let decrypted = decrypt(&encrypted, "key").unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn encrypt_produces_different_ciphertext_each_call() {
        let data = b"same input";
        let e1 = encrypt(data, "key").unwrap();
        let e2 = encrypt(data, "key").unwrap();
        // Different salt+nonce means different ciphertext
        assert_ne!(e1, e2);
    }
}
