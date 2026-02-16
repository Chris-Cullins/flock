use std::fs;
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use anyhow::{Context, Result, bail};
use argon2::Argon2;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const ENCRYPTED_KEY_EXT: &str = ".enc";

/// Derive a 256-bit key from a passphrase using Argon2id.
fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow::anyhow!("argon2 key derivation failed: {}", e))?;
    Ok(key)
}

/// Encrypt a plaintext signing key hex string.
/// Returns salt(16) || nonce(12) || ciphertext.
pub fn encrypt_key(plaintext_hex: &str, passphrase: &str) -> Result<Vec<u8>> {
    use rand::RngCore;
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let derived = derive_key(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&derived)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {}", e))?;
    let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext_hex.as_bytes())
        .map_err(|e| anyhow::anyhow!("encryption failed: {}", e))?;

    let mut result = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    result.extend_from_slice(&salt);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt an encrypted key blob back to the plaintext hex string.
pub fn decrypt_key(data: &[u8], passphrase: &str) -> Result<String> {
    if data.len() < SALT_LEN + NONCE_LEN + 1 {
        bail!("encrypted key data too short");
    }
    let salt = &data[..SALT_LEN];
    let nonce_bytes = &data[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ciphertext = &data[SALT_LEN + NONCE_LEN..];

    let derived = derive_key(passphrase, salt)?;
    let cipher = Aes256Gcm::new_from_slice(&derived)
        .map_err(|e| anyhow::anyhow!("failed to create cipher: {}", e))?;
    let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong passphrase?)"))?;

    String::from_utf8(plaintext).context("decrypted key is not valid UTF-8")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// No signing key exists.
    None,
    /// Plaintext key at the standard path.
    Plaintext,
    /// Encrypted key (.enc file present, plaintext removed).
    Encrypted,
}

/// Get the encrypted key file path from the plaintext key path.
pub fn encrypted_key_path(plaintext_path: &Path) -> std::path::PathBuf {
    let mut p = plaintext_path.as_os_str().to_owned();
    p.push(ENCRYPTED_KEY_EXT);
    std::path::PathBuf::from(p)
}

/// Check whether the signing key is plaintext, encrypted, or absent.
pub fn key_status(plaintext_path: &Path) -> KeyStatus {
    let enc_path = encrypted_key_path(plaintext_path);
    if plaintext_path.exists() {
        KeyStatus::Plaintext
    } else if enc_path.exists() {
        KeyStatus::Encrypted
    } else {
        KeyStatus::None
    }
}

/// Encrypt the signing key: read plaintext, encrypt, write .enc, remove plaintext.
pub fn encrypt_signing_key(plaintext_path: &Path, passphrase: &str) -> Result<()> {
    let enc_path = encrypted_key_path(plaintext_path);
    if !plaintext_path.exists() {
        bail!("no plaintext signing key found at {}", plaintext_path.display());
    }
    let plaintext_hex = fs::read_to_string(plaintext_path)
        .with_context(|| format!("failed to read {}", plaintext_path.display()))?;
    let encrypted = encrypt_key(plaintext_hex.trim(), passphrase)?;
    fs::write(&enc_path, &encrypted)
        .with_context(|| format!("failed to write {}", enc_path.display()))?;
    fs::remove_file(plaintext_path)
        .with_context(|| format!("failed to remove plaintext key {}", plaintext_path.display()))?;
    Ok(())
}

/// Decrypt the signing key: read .enc, decrypt, write plaintext, remove .enc.
pub fn decrypt_signing_key(plaintext_path: &Path, passphrase: &str) -> Result<()> {
    let enc_path = encrypted_key_path(plaintext_path);
    if !enc_path.exists() {
        bail!("no encrypted signing key found at {}", enc_path.display());
    }
    let data = fs::read(&enc_path)
        .with_context(|| format!("failed to read {}", enc_path.display()))?;
    let plaintext_hex = decrypt_key(&data, passphrase)?;
    fs::write(plaintext_path, &plaintext_hex)
        .with_context(|| format!("failed to write {}", plaintext_path.display()))?;
    fs::remove_file(&enc_path)
        .with_context(|| format!("failed to remove encrypted key {}", enc_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key_hex = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let passphrase = "test-passphrase";
        let encrypted = encrypt_key(key_hex, passphrase).expect("encrypt");
        let decrypted = decrypt_key(&encrypted, passphrase).expect("decrypt");
        assert_eq!(decrypted, key_hex);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let key_hex = "abcdef0123456789";
        let encrypted = encrypt_key(key_hex, "correct").expect("encrypt");
        let err = decrypt_key(&encrypted, "wrong").expect_err("should fail");
        assert!(format!("{:#}", err).contains("wrong passphrase"));
    }

    #[test]
    fn file_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_path = dir.path().join("ed25519.sk");
        let key_hex = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        fs::write(&key_path, key_hex).expect("write key");

        encrypt_signing_key(&key_path, "pass").expect("encrypt");
        assert!(!key_path.exists());
        assert!(encrypted_key_path(&key_path).exists());
        assert_eq!(key_status(&key_path), KeyStatus::Encrypted);

        decrypt_signing_key(&key_path, "pass").expect("decrypt");
        assert!(key_path.exists());
        assert!(!encrypted_key_path(&key_path).exists());
        assert_eq!(key_status(&key_path), KeyStatus::Plaintext);

        let recovered = fs::read_to_string(&key_path).expect("read");
        assert_eq!(recovered, key_hex);
    }
}
