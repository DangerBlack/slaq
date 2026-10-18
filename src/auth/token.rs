use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::info;

/// Slack session credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// xoxc session token (Bearer token for API auth)
    pub xoxc: String,
    /// xoxd session cookie (the `d` httpOnly cookie)
    pub xoxd: String,
    /// Workspace URL (e.g., myworkspace.slack.com)
    pub workspace_url: String,
    /// Authenticated user ID
    pub user_id: String,
    /// Team/workspace ID
    pub team_id: String,
}

/// `Debug` that never prints the secrets.
///
/// The tokens are account-owning credentials: a `{:?}` on this struct in a log
/// line, a panic message or a crash report would hand them to whoever reads it.
/// Only the non-secret fields are shown.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("xoxc", &"<redacted>")
            .field("xoxd", &"<redacted>")
            .field("workspace_url", &self.workspace_url)
            .field("user_id", &self.user_id)
            .field("team_id", &self.team_id)
            .finish()
    }
}

/// A team entry from Slack's LevelDB local storage.
#[derive(Debug, Clone)]
pub struct TeamEntry {
    pub url: String,
    pub token: String,
    pub user_id: String,
    pub team_id: String,
}

/// Service name for keyring entries.
const KEYRING_SERVICE: &str = "slaq";

/// Get the config directory path for file-based credential storage.
fn config_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(format!("{home}/.config/slaq")))
}

/// Get the credential file path.
fn creds_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("credentials.json"))
}

/// Stored credential entry (per workspace).
#[derive(Serialize, Deserialize)]
struct StoredCredentials {
    workspaces: HashMap<String, StoredEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredEntry {
    xoxc: String,
    xoxd: String,
}

/// Load credentials — try OS keychain first, fall back to file.
pub fn load_credentials(workspace: &str) -> Result<Credentials> {
    // Try keychain first
    if let Ok(creds) = load_from_keychain(workspace) {
        return Ok(creds);
    }

    // Fall back to file
    load_from_file(workspace)
}

/// Store credentials — try OS keychain first, fall back to file.
pub fn store_credentials(workspace: &str, xoxc: &str, xoxd: &str) -> Result<()> {
    // Try keychain first and verify it actually persisted
    if store_in_keychain(workspace, xoxc, xoxd).is_ok() {
        if load_from_keychain(workspace).is_ok() {
            info!("Credentials stored securely in OS keychain");
            return Ok(());
        }
    }

    // Fall back to file
    store_in_file(workspace, xoxc, xoxd)?;
    info!("Credentials stored in file (keychain unavailable)");
    Ok(())
}

/// Removes the stored session for a workspace: the keychain entries and,
/// if present, the file entry. A keychain that refuses the deletion does not
/// stop the logout, which must work on a machine without a keyring too.
pub fn delete_credentials(workspace: &str) -> Result<()> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &format!("xoxc:{workspace}")) {
        let _ = entry.delete_credential();
    }
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, "xoxd") {
        let _ = entry.delete_credential();
    }

    let path = creds_file()?;
    if !path.exists() {
        return Ok(());
    }

    let data = std::fs::read_to_string(&path).context("Failed to read credentials file")?;
    let mut stored: StoredCredentials = serde_json::from_str(&data).unwrap_or(StoredCredentials {
        workspaces: HashMap::new(),
    });
    stored.workspaces.remove(workspace);

    if stored.workspaces.is_empty() {
        std::fs::remove_file(&path).context("Failed to remove credentials file")?;
    } else {
        write_credentials_file(&path, &stored)?;
    }
    Ok(())
}

/// Load credentials from the OS keychain only.
fn load_from_keychain(workspace: &str) -> Result<Credentials> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &format!("xoxc:{workspace}"))
        .map_err(|e| anyhow::anyhow!("keyring error: {e}"))?;
    let xoxc = entry
        .get_password()
        .context("xoxc token not found in keychain")?;

    let entry = keyring::Entry::new(KEYRING_SERVICE, "xoxd")
        .map_err(|e| anyhow::anyhow!("keyring error: {e}"))?;
    let xoxd = entry
        .get_password()
        .context("xoxd cookie not found in keychain")?;

    info!("Loaded credentials from OS keychain");
    Ok(Credentials {
        xoxc,
        xoxd,
        workspace_url: workspace.to_string(),
        user_id: String::new(),
        team_id: String::new(),
    })
}

/// Store credentials in the OS keychain only.
fn store_in_keychain(workspace: &str, xoxc: &str, xoxd: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &format!("xoxc:{workspace}"))
        .map_err(|e| anyhow::anyhow!("keyring error: {e}"))?;
    entry
        .set_password(xoxc)
        .context("Failed to store xoxc token in keychain")?;

    let entry = keyring::Entry::new(KEYRING_SERVICE, "xoxd")
        .map_err(|e| anyhow::anyhow!("keyring error: {e}"))?;
    entry
        .set_password(xoxd)
        .context("Failed to store xoxd cookie in keychain")?;

    Ok(())
}

/// Load credentials from file.
fn load_from_file(workspace: &str) -> Result<Credentials> {
    let path = creds_file()?;
    if !path.exists() {
        anyhow::bail!(
            "No credentials found for {workspace}. Run `slack login --xoxc ... --xoxd ...` first"
        );
    }

    let data = std::fs::read_to_string(&path).context("Failed to read credentials file")?;
    let stored: StoredCredentials =
        serde_json::from_str(&data).context("Failed to parse credentials file")?;

    let entry = stored
        .workspaces
        .get(workspace)
        .with_context(|| format!("No credentials found for workspace: {workspace}"))?;

    info!("Loaded credentials from file");
    Ok(Credentials {
        xoxc: entry.xoxc.clone(),
        xoxd: entry.xoxd.clone(),
        workspace_url: workspace.to_string(),
        user_id: String::new(),
        team_id: String::new(),
    })
}

/// Store credentials in file.
fn store_in_file(workspace: &str, xoxc: &str, xoxd: &str) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir).context("Failed to create config directory")?;
    // Owner-only: the file itself is 0600, this keeps the directory from
    // listing or a planted entry from mattering.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = creds_file()?;
    let mut stored: StoredCredentials = if path.exists() {
        let data = std::fs::read_to_string(&path)?;
        serde_json::from_str(&data).unwrap_or(StoredCredentials {
            workspaces: HashMap::new(),
        })
    } else {
        StoredCredentials {
            workspaces: HashMap::new(),
        }
    };

    stored.workspaces.insert(
        workspace.to_string(),
        StoredEntry {
            xoxc: xoxc.to_string(),
            xoxd: xoxd.to_string(),
        },
    );

    write_credentials_file(&path, &stored)
}

/// Writes the credential store with owner-only permissions **at creation**.
///
/// `std::fs::write` followed by a `chmod` leaves a window in which the secrets
/// on disk are world-readable. `OpenOptions::mode(0o600)` creates the file
/// already restricted, and together with `O_NOFOLLOW` it refuses to write
/// through a symlink planted in the config directory.
fn write_credentials_file(path: &std::path::Path, stored: &StoredCredentials) -> Result<()> {
    use std::io::Write as _;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `mode` applies only when the file is created; an existing one keeps
        // its own mode, so an already-0600 file is not widened.
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts
        .open(path)
        .context("Failed to create credentials file")?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, stored).context("Failed to serialize credentials")?;
    writer.flush().context("Failed to write credentials file")?;
    Ok(())
}

/// Extract team tokens from Slack Desktop's LevelDB local storage.
///
/// Slack stores xoxc tokens in Chromium's LevelDB-backed localStorage
/// under the key `localConfig_v2`. The value is a JSON blob with team info.
pub fn extract_from_leveldb(slack_dir: &std::path::Path) -> Result<Vec<TeamEntry>> {
    let leveldb_dir = slack_dir.join("Local Storage").join("leveldb");
    if !leveldb_dir.exists() {
        anyhow::bail!("LevelDB directory not found: {leveldb_dir:?}");
    }

    // Read LevelDB log files looking for localConfig_v2
    let mut teams = Vec::new();
    let mut reader = std::fs::read_dir(&leveldb_dir).context("Failed to read LevelDB directory")?;

    while let Some(entry) = reader.next().transpose()? {
        let path = entry.path();
        if path
            .extension()
            .map_or(true, |ext| ext != "log" && ext != "ldb")
        {
            continue;
        }

        let content = match std::fs::read(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Search for localConfig_v2 in the raw bytes
        if let Ok(text) = String::from_utf8(content.clone()) {
            if let Some(teams_data) = extract_teams_from_config(&text) {
                teams.extend(teams_data);
            }
        } else {
            let text = content
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(teams_data) = extract_teams_from_config(&text) {
                teams.extend(teams_data);
            }
        }
    }

    // Deduplicate by URL
    teams.sort_by(|a, b| a.url.cmp(&b.url));
    teams.dedup_by(|a, b| a.url == b.url);

    info!("Found {} team(s) in Slack Desktop LevelDB", teams.len());
    Ok(teams)
}

/// Parse team entries from a JSON config string containing localConfig_v2 data.
fn extract_teams_from_config(text: &str) -> Option<Vec<TeamEntry>> {
    // Find the localConfig_v2 JSON blob
    let start = text.find(r#""localConfig_v2""#)?;
    // Find the actual JSON object after the key
    let json_start = text[start..].find('{')?;
    let json_str = &text[start + json_start..];

    // Find the matching closing brace (simplified — assumes valid JSON)
    let mut depth = 0;
    let mut end = 0;
    for (i, ch) in json_str.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        return None;
    }

    let json_text = &json_str[..end];

    // Parse the JSON to extract team tokens
    #[derive(Deserialize)]
    struct LocalConfig {
        #[serde(default)]
        teams: HashMap<String, TeamConfigRaw>,
    }

    #[derive(Deserialize)]
    struct TeamConfigRaw {
        #[serde(default)]
        token: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        team_id: String,
        #[serde(default)]
        user_id: String,
    }

    match serde_json::from_str::<LocalConfig>(json_text) {
        Ok(config) => {
            let teams: Vec<TeamEntry> = config
                .teams
                .into_iter()
                .map(|(_id, team)| TeamEntry {
                    url: team.url,
                    token: team.token,
                    user_id: team.user_id,
                    team_id: team.team_id,
                })
                .filter(|t| !t.token.is_empty())
                .collect();
            Some(teams)
        }
        Err(e) => {
            tracing::warn!("Failed to parse localConfig_v2: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_teams_from_config() {
        let json = r#"{"localConfig_v2":{"teams":{"T01ABC":{"token":"xoxc-test-token","url":"test.slack.com","team_id":"T01ABC","user_id":"U01XYZ"}}}}"#;
        let result = extract_teams_from_config(json);
        assert!(result.is_some());
        let teams = result.unwrap();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].token, "xoxc-test-token");
        assert_eq!(teams[0].url, "test.slack.com");
    }

    #[test]
    fn test_extract_teams_from_nested_json() {
        let json = r#"some_prefix_before{"localConfig_v2":{"teams":{"T01ABC":{"token":"xoxc-nested","url":"nested.slack.com","team_id":"T01ABC","user_id":"U01XYZ"}}}}some_suffix_after"#;
        let result = extract_teams_from_config(json);
        assert!(result.is_some());
        let teams = result.unwrap();
        assert_eq!(teams[0].token, "xoxc-nested");
    }
}
