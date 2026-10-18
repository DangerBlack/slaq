//! Opening a huddle in the browser.
//!
//! Huddles need two-way audio, echo cancellation, sending *and receiving*
//! screen and video: things a terminal cannot do, the last one by definition.
//! Rather than reimplementing them badly, the TUI delegates to the browser.
//!
//! The browser is almost always open already and already authenticated with
//! Slack, so the marginal cost is a tab instead of ~1 GB of Electron running
//! all day.

use crate::auth::Credentials;
use anyhow::{Context, Result};
use std::process::{Command, Stdio};
use tracing::info;

/// The URL of a huddle in a channel.
///
/// It is the same format Slack returns in `huddle_link` inside the `rooms.join`
/// response, but it is built without calling anything: opening the URL makes
/// the web client create the room, which is what we want.
pub fn huddle_url(team_id: &str, channel_id: &str) -> String {
    format!("https://app.slack.com/huddle/{team_id}/{channel_id}")
}

/// How to launch the browser.
pub enum Browser {
    /// The system's default browser, with the Slack session the user
    /// already has. No configuration, it just works.
    System,
    /// Chromium in app mode: a bare window, with no tab bar and no
    /// bookmarks, closed when the call ends. It uses the default profile, so it
    /// inherits the existing login.
    ChromiumApp(String),
    /// A command chosen by the user; `{url}` is substituted.
    Custom(String),
}

impl Browser {
    /// Chooses according to `SLAQ_BROWSER`.
    ///
    /// - unset: Chromium in app mode if present, otherwise the system browser;
    /// - `system`: always the system browser;
    /// - anything else: a custom command.
    pub fn from_env() -> Self {
        match std::env::var("SLAQ_BROWSER") {
            Ok(v) if v == "system" => Browser::System,
            Ok(v) if !v.trim().is_empty() => Browser::Custom(v),
            _ => match find_chromium() {
                Some(bin) => Browser::ChromiumApp(bin),
                None => Browser::System,
            },
        }
    }

    fn command(&self, url: &str) -> Command {
        match self {
            Browser::System => {
                let mut c = Command::new("xdg-open");
                c.arg(url);
                c
            }
            Browser::ChromiumApp(bin) => {
                let mut c = Command::new(bin);
                // `--app` opens a window with no browser chrome: it behaves like a
                // dedicated call window.
                c.arg(format!("--app={url}"));
                c
            }
            Browser::Custom(tpl) => {
                let expanded = if tpl.contains("{url}") {
                    tpl.replace("{url}", url)
                } else {
                    format!("{tpl} {url}")
                };
                let mut parts = expanded.split_whitespace();
                let mut c = Command::new(parts.next().unwrap_or("xdg-open"));
                c.args(parts);
                c
            }
        }
    }

    pub fn describe(&self) -> &str {
        match self {
            Browser::System => "browser di sistema",
            Browser::ChromiumApp(_) => "Chromium (finestra app)",
            Browser::Custom(_) => "comando personalizzato",
        }
    }
}

fn find_chromium() -> Option<String> {
    ["chromium", "chromium-browser", "google-chrome", "brave-browser"]
        .iter()
        .find_map(|b| which(b))
}

fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Works out the team id, which the huddle URL needs.
///
/// The stored credentials do not always contain it (the login flow leaves it
/// empty), so it falls back to `auth.test`.
///
/// The cached value is passed in as `cached` rather than taking a `Store`
/// here: the rusqlite connection is not `Sync`, and holding its lock across the
/// `await` would make the future non-`Send`. Persisting the result is the
/// caller's business.
pub async fn resolve_team_id(
    creds: &Credentials,
    api: &crate::api::Client,
    cached: Option<String>,
) -> Result<String> {
    if !creds.team_id.is_empty() {
        return Ok(creds.team_id.clone());
    }
    if let Some(id) = cached.filter(|c| !c.is_empty()) {
        return Ok(id);
    }

    let auth = api
        .auth_test()
        .await
        .context("auth.test failed: unable to obtain the team id")?;
    info!("team id obtained via auth.test: {}", auth.team_id);
    Ok(auth.team_id)
}

/// Opens the channel's huddle, without waiting for the browser to finish.
pub fn open(team_id: &str, channel_id: &str, browser: &Browser) -> Result<String> {
    let url = huddle_url(team_id, channel_id);

    browser
        .command(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch of {} failed", browser.describe()))?;

    info!("huddle opened in {}: {url}", browser.describe());
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_matches_slack_format() {
        assert_eq!(
            huddle_url("T00000000", "C00000000"),
            "https://app.slack.com/huddle/T00000000/C00000000"
        );
    }

    #[test]
    fn custom_command_substitutes_the_url_placeholder() {
        let b = Browser::Custom("firefox --new-window {url}".into());
        let c = b.command("https://example.test");
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(c.get_program(), "firefox");
        assert_eq!(args, ["--new-window", "https://example.test"]);
    }

    #[test]
    fn custom_command_without_placeholder_appends_the_url() {
        let b = Browser::Custom("firefox".into());
        let c = b.command("https://example.test");
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["https://example.test"]);
    }
}
