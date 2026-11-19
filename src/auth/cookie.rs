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

    let db_version: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'version'",
        [],
        |row| row.get(0),
    ).context("Failed to read Slack cookie database version")?;
    let db_version = db_version.parse().context("Invalid Slack cookie database version")?;
    let cookie_bytes = cookie_value_without_host_digest(&decrypted, db_version)?;
    let cookie_str = String::from_utf8(cookie_bytes.to_vec())
        .context("Decrypted cookie is not valid UTF-8")?;

    info!("Successfully decrypted d cookie");
    Ok(cookie_str)
}

fn cookie_value_without_host_digest(value: &[u8], db_version: i64) -> Result<&[u8]> {
    if db_version < 24 {
        return Ok(value);
    }
    use sha2::{Digest, Sha256};
    let expected = Sha256::digest(b".slack.com");
    if value.len() < expected.len() || &value[..expected.len()] != expected.as_slice() {
        anyhow::bail!("Slack cookie host digest missing or invalid");
    }
    Ok(&value[expected.len()..])
}

/// Get the Slack Safe Storage password from the OS keychain.
///
/// On Linux, Electron's safeStorage stores the AES key in the login keyring
/// under service "Slack Safe Storage" with account "Slack Key" or "Slack".
fn get_slack_safe_storage_password() -> Result<String> {
    #[cfg(target_os = "linux")]
    if let Some(password) = get_linux_slack_safe_storage_password()? {
        return Ok(password);
    }

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

/// Electron's Linux safeStorage item uses libsecret attributes, not the
/// service/username attributes used by the Rust keyring crate.
#[cfg(target_os = "linux")]
fn get_linux_slack_safe_storage_password() -> Result<Option<String>> {
    use secret_service::{blocking::SecretService, EncryptionType};
    use std::collections::HashMap;

    let service = SecretService::connect(EncryptionType::Dh)
        .context("Failed to connect to Secret Service")?;
    let items = service
        .search_items(HashMap::from([
            ("application", "Slack"),
            ("xdg:schema", "chrome_libsecret_os_crypt_password_v2"),
        ]))
        .context("Failed to find Slack safeStorage item")?;
    let count = items.unlocked.len() + items.locked.len();
    if count > 1 {
        anyhow::bail!("Multiple Slack safeStorage items found; refusing ambiguous lookup");
    }
    let item = match items.unlocked.first().or_else(|| items.locked.first()) {
        Some(item) => item,
        None => return Ok(None),
    };
    if items.unlocked.is_empty() {
        item.unlock().context("Failed to unlock Slack safeStorage item")?;
    }
    let secret = item.get_secret().context("Failed to read Slack safeStorage item")?;
    Ok(Some(String::from_utf8(secret).context("Slack safeStorage password is not UTF-8")?))
}

/// Decrypt a Chromium/Electron encrypted cookie value.
///
/// Format: `<3-byte version prefix "v10" or "v11"> <ciphertext>`
/// Encryption: AES-128-CBC with PKCS7 padding
/// Key derivation: PBKDF2-SHA1 with salt "saltysalt", one iteration on Linux
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
    let iterations = if cfg!(target_os = "linux") { 1 } else { 1003 };
    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
        password,
        b"saltysalt",
        iterations,
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
        let iterations = if cfg!(target_os = "linux") { 1 } else { 1003 };
        pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password, b"saltysalt", iterations, &mut key).unwrap();
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

    #[test]
    fn validates_cookie_host_digest_for_version_24() {
        use sha2::{Digest, Sha256};
        let mut value = Sha256::digest(b".slack.com").to_vec();
        value.extend_from_slice(b"xoxd-test");
        assert_eq!(cookie_value_without_host_digest(&value, 24).unwrap(), b"xoxd-test");
        value[0] ^= 1;
        assert!(cookie_value_without_host_digest(&value, 24).is_err());
        assert_eq!(cookie_value_without_host_digest(b"legacy", 23).unwrap(), b"legacy");
    }
}
