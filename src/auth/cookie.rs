use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

/// Extract the `d` (xoxd) cookie from Slack Desktop's SQLite cookie store.
///
/// Chromium/Electron stores cookies in a SQLite database at `{slack_dir}/Cookies`.
/// The cookie value is AES-128-CBC encrypted using Electron's safeStorage,
/// with the key stored in the OS keychain under "Slack Safe Storage".
///
/// On Linux, the key is derived from a password stored in the login keyring.
pub fn extract_d_cookie(slack_dir: &Path) -> Result<String> {
    let cookies_db = slack_dir.join("Cookies");
    if !cookies_db.exists() {
        anyhow::bail!("Cookies database not found: {cookies_db:?}");
    }

    info!("Reading cookies from: {cookies_db:?}");

    // Read the encrypted cookie value from SQLite
    let conn = rusqlite::Connection::open(&cookies_db)
        .context("Failed to open Slack Cookies database")?;

    let encrypted_value: Vec<u8> = conn
        .query_row(
            "SELECT encrypted_value FROM cookies WHERE host_key = '.slack.com' AND name = 'd'",
            [],
            |row| row.get(0),
        )
        .context("d cookie not found in Slack Cookies database. Is Slack logged in?")?;

    info!("Found encrypted d cookie ({} bytes)", encrypted_value.len());

    // Decrypt using the keyring-stored password
    let password = get_slack_safe_storage_password()
        .context("Failed to get Slack Safe Storage password from keyring")?;

    let decrypted = decrypt_chromium_cookie(&encrypted_value, password.as_bytes())?;

    let cookie_str = String::from_utf8(decrypted)
        .context("Decrypted cookie is not valid UTF-8")?;

    info!("Successfully decrypted d cookie");
    Ok(cookie_str)
}

/// Get the Slack Safe Storage password from the OS keychain.
///
/// On Linux, Electron's safeStorage stores the AES key in the login keyring
/// under service "Slack Safe Storage" with account "Slack Key" or "Slack".
fn get_slack_safe_storage_password() -> Result<String> {
    let service = "Slack Safe Storage";

    // Try multiple possible account names
    let accounts = ["Slack Key", "Slack", "Slack App Store Key"];

    for account in &accounts {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| anyhow::anyhow!("keyring error: {e}"))?;
        if let Ok(password) = entry.get_password() {
            info!("Found Slack Safe Storage password with account: {account}");
            return Ok(password);
        }
    }

    anyhow::bail!(
        "Slack Safe Storage password not found in keyring. \
         Is Slack Desktop installed and logged in?"
    )
}

/// Decrypt a Chromium/Electron encrypted cookie value.
///
/// Format: `<3-byte version prefix "v10" or "v11"> <ciphertext>`
/// Encryption: AES-128-CBC with PKCS7 padding
/// Key derivation: PBKDF2-SHA1 with salt "saltysalt", 1003 iterations
/// IV: 16 bytes of ASCII space (0x20)
pub fn decrypt_chromium_cookie(encrypted: &[u8], password: &[u8]) -> Result<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    if encrypted.len() < 4 {
        anyhow::bail!("encrypted cookie too short ({})", encrypted.len());
    }

    let prefix = &encrypted[..3];
    if prefix != b"v10" && prefix != b"v11" {
        anyhow::bail!("unexpected cipher prefix: {prefix:?}");
    }

    let ciphertext = &encrypted[3..];

    let mut key = [0u8; 16];
    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
        password,
        b"saltysalt",
        1003,
        &mut key,
    )
    .map_err(|e| anyhow::anyhow!("PBKDF2 failed: {e}"))?;

    let iv = [0x20u8; 16];
    let mut buf = ciphertext.to_vec();
    let plaintext = Aes128CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow::anyhow!("AES-CBC decryption failed: {e}"))?;

    Ok(plaintext.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decrypt_chromium_cookie_too_short() {
        let result = decrypt_chromium_cookie(&[0u8; 2], b"password");
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_chromium_cookie_bad_prefix() {
        let result = decrypt_chromium_cookie(&[0u8; 10], b"password");
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_chromium_cookie_roundtrip() {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

        let password = b"test-key";
        let plaintext = b"xoxd-test-cookie-value";

        // Encrypt
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password, b"saltysalt", 1003, &mut key).unwrap();
        let iv = [0x20u8; 16];
        let mut buf = plaintext.to_vec();
        buf.extend_from_slice(&[0u8; 16]); // padding room
        let ct = Aes128CbcEnc::new(&key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plaintext.len())
            .unwrap();

        let mut encrypted = Vec::with_capacity(3 + ct.len());
        encrypted.extend_from_slice(b"v10");
        encrypted.extend_from_slice(ct);

        // Decrypt
        let decrypted = decrypt_chromium_cookie(&encrypted, password).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
