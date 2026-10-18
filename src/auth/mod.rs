//! Authentication module — xoxc/xoxd credential extraction and storage.
//!
//! Slack's desktop/web clients authenticate using a pair of secrets:
//! - `xoxc`: A session token (Bearer token for API requests)
//! - `xoxd`: The `d` httpOnly cookie (sent with every request)
//!
//! These can be:
//! 1. Extracted from Slack Desktop's LevelDB + SQLite cookie store
//! 2. Provided manually by the user
//! 3. Stored securely in the OS keychain

pub mod cookie;
pub mod token;
#[cfg(target_os = "linux")]
mod browser;

/// Complete the login by driving the installed Chromium/Chrome over CDP
/// (`slaq login --browser`).
///
/// Linux only: it launches a dedicated temporary profile, follows the OAuth
/// flow and returns the session secrets, which are stored like the manual path.
/// Other platforms report that the path is not implemented yet.
pub async fn login_via_browser(workspace: Option<&str>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        browser::run(workspace).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace;
        anyhow::bail!(
            "`login --browser` is not implemented yet on this platform (needs Chromium/Chrome)"
        )
    }
}

use anyhow::{Context, Result};
pub use token::Credentials;
use tracing::info;

/// Load credentials from the OS keychain for the given workspace.
pub fn load_credentials(workspace: Option<&str>) -> Result<Credentials> {
    let ws = workspace.unwrap_or("app.slack.com");
    info!("Loading credentials for workspace: {ws}");

    let creds =
        token::load_credentials(ws).context("Failed to load credentials")?;

    // Verify credentials by making a lightweight auth.test call
    info!(
        "Loaded credentials for user {} on team {}",
        creds.user_id, creds.team_id
    );

    Ok(creds)
}

/// Store credentials in the OS keychain.
pub fn store_credentials(workspace: &str, xoxc: &str, xoxd: &str) -> Result<()> {
    info!("Storing credentials for workspace: {workspace}");
    token::store_credentials(workspace, xoxc, xoxd)
        .context("Failed to store credentials")?;
    Ok(())
}

/// Removes the stored session for a workspace: logout.
pub fn delete_credentials(workspace: &str) -> Result<()> {
    info!("Removing credentials for workspace: {workspace}");
    token::delete_credentials(workspace)
}

/// Extract credentials from Slack Desktop's local storage and store them.
pub async fn extract_from_desktop() -> Result<Credentials> {
    info!("Extracting credentials from Slack Desktop...");

    let slack_dir = detect_slack_dir()?;
    info!("Found Slack Desktop at: {slack_dir:?}");

    let xoxd = cookie::extract_d_cookie(&slack_dir)
        .context("Failed to extract d cookie from Slack Desktop")?;

    let teams = token::extract_from_leveldb(&slack_dir)
        .context("Failed to extract tokens from Slack Desktop LevelDB")?;

    if teams.is_empty() {
        anyhow::bail!("No teams found in Slack Desktop LevelDB");
    }

    // Resolve workspace URLs and store each team's credentials
    for team in &teams {
        let ws = if team.url.is_empty() {
            anyhow::bail!(
                "Could not resolve workspace URL for token; \
                 try providing --workspace manually"
            );
        } else {
            &team.url
        };

        let creds = Credentials {
            xoxc: team.token.clone(),
            xoxd: xoxd.clone(),
            workspace_url: ws.clone(),
            user_id: team.user_id.clone(),
            team_id: team.team_id.clone(),
        };

        // Verify and store
        let client = crate::api::Client::new(creds.clone());
        match client.auth_test().await {
            Ok(auth) => {
                let final_creds = Credentials {
                    user_id: auth.user_id,
                    team_id: auth.team_id,
                    workspace_url: auth.url,
                    ..creds
                };
                store_credentials(&final_creds.workspace_url, &final_creds.xoxc, &final_creds.xoxd)?;
                info!("Stored credentials for {}", final_creds.workspace_url);
            }
            Err(e) => {
                tracing::warn!("Skipping team {}: auth test failed: {e}", team.url);
            }
        }
    }

    // Return the first valid credential set
    load_credentials(None)
}

fn detect_slack_dir() -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;

    // Check common Linux paths for Slack Desktop
    let candidates = vec![
        format!("{home}/.config/Slack"),
        format!("{home}/snap/slack/current/.config/Slack"),
        format!("{home}/.var/app/com.slack.Slack/config/Slack"),
    ];

    for path in &candidates {
        let p = std::path::Path::new(path);
        if p.join("Local Storage").join("leveldb").exists()
            || p.join("Cookies").exists()
        {
            return Ok(p.to_path_buf());
        }
    }

    anyhow::bail!(
        "Could not find Slack Desktop config directory. \
         Tried: {:?}. Is Slack Desktop installed and logged in?",
        candidates
    )
}
