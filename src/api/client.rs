use crate::auth::Credentials;
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

/// Tells a rate limit — which is worth retrying — from everything else,
/// which is not.
#[derive(Debug)]
enum CallError {
    RateLimited { retry_after: Duration },
    Other(anyhow::Error),
}

impl From<CallError> for anyhow::Error {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Other(e) => e,
            CallError::RateLimited { retry_after } => anyhow::anyhow!(
                "rate limit did not clear after {}s of waiting",
                retry_after.as_secs()
            ),
        }
    }
}

/// `Retry-After` in seconds, with a fallback if it is missing or absurd.
fn retry_after_of(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s <= 300)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(30))
}

/// Slack Web API client.
///
/// Authenticates using xoxc/xoxd session credentials against Slack's
/// private `/api/*` endpoints. This is the same protocol Slack's own
/// web and desktop clients use internally.
#[derive(Debug, Clone)]
pub struct Client {
    /// Session credentials
    creds: Credentials,
    /// HTTP client with browser-like headers
    http: reqwest::Client,
    /// Base API URL
    base_url: String,
}

/// Generic Slack API response envelope.
#[derive(Debug, Deserialize)]
pub struct SlackResponse<T> {
    pub ok: bool,
    #[serde(flatten)]
    pub data: Option<T>,
    pub error: Option<String>,
}

/// Response from auth.test.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTestResponse {
    pub user_id: String,
    pub team_id: String,
    pub url: String,
    pub team: String,
    pub user: String,
}

/// Response from chat.postMessage.
#[derive(Debug, Deserialize)]
pub struct PostMessageResponse {
    pub channel: String,
    pub ts: String,
    pub message: Option<serde_json::Value>,
}

/// Response from client.getWebSocketURL.
#[derive(Debug, Clone, Deserialize)]
pub struct WebSocketUrlResponse {
    #[serde(default)]
    pub primary_websocket_url: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub routing_context: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    /// Direct messages **do not have** this field: without `default` the
    /// deserialisation of the whole page fails as soon as one appears.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub name_normalized: String,
    #[serde(default)]
    pub topic: ChannelTopicPurpose,
    #[serde(default)]
    pub purpose: ChannelTopicPurpose,
    #[serde(default)]
    pub num_members: i64,
    #[serde(default)]
    pub is_channel: bool,
    #[serde(default)]
    pub is_group: bool,
    #[serde(default)]
    pub is_im: bool,
    #[serde(default)]
    pub is_mpim: bool,
    #[serde(default)]
    pub is_archived: bool,
    /// A space shared with another organisation (Slack Connect): Slack
    /// groups them separately, and whoever writes in them does not appear in
    /// `users.list`.
    #[serde(default)]
    pub is_ext_shared: bool,
    #[serde(default)]
    pub is_general: bool,
    #[serde(default)]
    pub is_member: bool,
    /// DMs only: the user on the other side. DMs have no `name`.
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResponseMetadata {
    pub next_cursor: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChannelTopicPurpose {
    pub value: Option<String>,
    pub creator: Option<String>,
    pub last_set: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub real_name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub profile: UserProfile,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub is_app_user: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UserProfile {
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub real_name: String,
    #[serde(default)]
    pub image_72: String,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub status_emoji: String,
}

impl UserInfo {
    /// The name to show for this person.
    ///
    /// The **full name** wins over the nickname: Slack's `display_name` is a
    /// chosen alias, and the real client shows the person's real name. It falls
    /// back to the display name, then the username, only when there is nothing
    /// better — never showing a bare handle when a name exists.
    pub fn full_name(&self) -> String {
        let full = if !self.real_name.is_empty() {
            &self.real_name
        } else {
            &self.profile.real_name
        };
        if !full.is_empty() {
            return full.clone();
        }
        let nick = if !self.display_name.is_empty() {
            &self.display_name
        } else {
            &self.profile.display_name
        };
        if !nick.is_empty() {
            return nick.clone();
        }
        self.name.clone()
    }
}

/// A user's "do not disturb" state, from `dnd.teamInfo`.
///
/// `dnd_enabled` says the user **has** a quiet schedule configured, not that
/// they are quiet right now: to know that you look at whether the current
/// moment falls between the two timestamps. See `app::in_quiet_hours`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DndInfo {
    #[serde(default)]
    pub dnd_enabled: bool,
    #[serde(default)]
    pub next_dnd_start_ts: i64,
    #[serde(default)]
    pub next_dnd_end_ts: i64,
    /// A manual pause ("snooze"), distinct from the recurring schedule.
    #[serde(default)]
    pub snooze_enabled: bool,
}

/// The `client.counts` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Counts {
    #[serde(default)]
    pub channels: Vec<CountEntry>,
    #[serde(default)]
    pub ims: Vec<CountEntry>,
    #[serde(default)]
    pub mpims: Vec<CountEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CountEntry {
    pub id: String,
    #[serde(default)]
    pub has_unreads: bool,
    #[serde(default)]
    pub mention_count: i64,
    /// The `ts` of the last message: it allows sorting by recent activity
    /// instead of alphabetically, which for DMs is the sensible order.
    #[serde(default)]
    pub latest: String,
    /// How far Slack has us reading in this conversation.
    #[serde(default)]
    pub last_read: String,
}

impl Counts {
    pub fn all(&self) -> impl Iterator<Item = &CountEntry> {
        self.channels.iter().chain(&self.ims).chain(&self.mpims)
    }
}

/// A `search.messages` result.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchHit {
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub text: String,
    /// Present on bot messages, where `user` is missing.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub permalink: String,
    /// Present when the hit is a reply inside a thread: points at the root.
    /// It is what lets the front end open the local thread instead of the web.
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub channel: SearchChannel,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchChannel {
    #[serde(default)]
    pub id: String,
    /// Empty for direct messages: there `id` is all you have.
    #[serde(default)]
    pub name: String,
}

/// One entry of the "Later" list (`saved.list`): a reference to a saved
/// message, not the message itself.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SavedItem {
    /// The channel the saved message is in.
    #[serde(default)]
    pub item_id: String,
    /// `message` for a saved message.
    #[serde(default)]
    pub item_type: String,
    #[serde(default)]
    pub ts: String,
    /// `in_progress`, `completed` or `archived`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub date_created: i64,
}

/// The counters that come with a `saved.list` page.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SavedCounts {
    #[serde(default)]
    pub total_count: i64,
    #[serde(default)]
    pub uncompleted_count: i64,
    #[serde(default)]
    pub completed_count: i64,
    #[serde(default)]
    pub archived_count: i64,
}

/// A thread from the "Threads" inbox (`subscriptions.thread.getView`): the
/// root message and its latest replies, as raw Slack messages.
pub struct ThreadItem {
    /// The conversation the thread lives in.
    pub channel: String,
    pub root: serde_json::Value,
    pub replies: Vec<serde_json::Value>,
}

/// A section of the sidebar.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelSection {
    #[serde(default)]
    pub channel_section_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub emoji: String,
    /// `standard` for the ones the user created; the others are system ones.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub channel_ids_page: ChannelIdsPage,
    /// The sections are a linked list: the sidebar order is rebuilt by
    /// following this field, not the order of the array.
    #[serde(default)]
    pub next_channel_section_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelIdsPage {
    #[serde(default)]
    pub channel_ids: Vec<String>,
}

impl ChannelSection {
    pub fn channel_ids(&self) -> &[String] {
        &self.channel_ids_page.channel_ids
    }

    /// Whether it is a folder the user created.
    pub fn is_user_section(&self) -> bool {
        self.kind == "standard"
    }
}

/// Response from screenhero.rooms.create (Slack's internal call API).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ScreenheroRoomsCreateResponse {
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    /// The channel this call belongs to
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Every other field the server returns. The huddle protocol is not
    /// documented: everything we cannot name yet ends up here, and it is the most
    /// interesting part of the response.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Response from calls.add.
#[derive(Debug, Clone, Deserialize)]
pub struct CallsAddResponse {
    pub id: String,
    #[serde(default)]
    pub external_unique_id: String,
    #[serde(default)]
    pub join_url: String,
}

impl Client {
    /// Create a new API client with the given credentials.
    pub fn new(creds: Credentials) -> Self {
        let base_url = format!("https://{}/api", creds.workspace_url.trim_start_matches("https://").trim_end_matches('/'));

        let http = reqwest::Client::builder()
            // We introduce ourselves as what we are. An unofficial client
            // shows up in the workspace audit log anyway: disguising it as Slack
            // Desktop would turn an explainable line into a compromised-credentials
            // alert, and it falls among the "technical measures" the API ToS forbid
            // circumventing.
            .user_agent(crate::USER_AGENT)
            // Without a timeout a connection that stalls hangs the client
            // forever: it happened, and with no error message at all.
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    reqwest::header::ORIGIN,
                    "https://app.slack.com".parse().unwrap(),
                );
                headers.insert(
                    reqwest::header::REFERER,
                    "https://app.slack.com/".parse().unwrap(),
                );
                headers.insert(
                    reqwest::header::ACCEPT_LANGUAGE,
                    "en-US,en;q=0.9".parse().unwrap(),
                );
                headers
            })
            .timeout(Duration::from_secs(30))
            .cookie_store(true)
            .build()
            .expect("Failed to build HTTP client");

        Client {
            creds,
            http,
            base_url,
        }
    }

    /// Build the cookie header value for authentication.
    fn cookie_header(&self) -> String {
        format!("d={}; d-s={}", self.creds.xoxd, Self::d_s_value())
    }

    /// Generate the d-s cookie value (current unix timestamp - 10).
    fn d_s_value() -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(10);
        now.to_string()
    }

    /// Fetch a file's bytes into memory, capped at `max` bytes. The session
    /// cookie is sent only to Slack's own hosts (a file URL comes from message
    /// metadata; a third-party host must never receive it), and the body is read
    /// incrementally so a huge file is refused instead of buffered whole.
    pub async fn download_bytes(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        let mut req = self.http.get(url);
        if crate::net::is_slack_host(url) {
            req = req.header(reqwest::header::COOKIE, self.cookie_header());
        }
        let mut resp = req
            .send()
            .await
            .context("file request failed")?
            .error_for_status()
            .context("invalid response")?;
        if let Some(len) = resp.content_length() {
            if len as usize > max {
                anyhow::bail!("file exceeds {max} bytes");
            }
        }
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.context("file read failed")? {
            if out.len() + chunk.len() > max {
                anyhow::bail!("file exceeds {max} bytes");
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// How many times to retry when Slack imposes a rate limit.
    const RATE_LIMIT_RETRIES: u32 = 5;

    /// Make a POST request to a Slack API endpoint.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<(&str, &str)>,
    ) -> Result<T> {
        // Paginated methods easily go past Slack's limits
        // (`conversations.list` is Tier 2, 20 requests a minute). Without waiting
        // and retrying, the loading fails halfway — and insisting is the pattern
        // that gets session tokens invalidated.
        let mut attempt = 0;
        loop {
            match self.call_once::<T>(method, params.clone()).await {
                Err(CallError::RateLimited { retry_after })
                    if attempt < Self::RATE_LIMIT_RETRIES =>
                {
                    attempt += 1;
                    tracing::info!(
                        "{method}: rate limited, waiting {}s (attempt {attempt})",
                        retry_after.as_secs()
                    );
                    tokio::time::sleep(retry_after).await;
                }
                Err(e) => return Err(e.into()),
                Ok(v) => return Ok(v),
            }
        }
    }

    async fn call_once<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<(&str, &str)>,
    ) -> std::result::Result<T, CallError> {
        let url = format!("{}/{}", self.base_url, method);

        let mut form: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 1);
        form.push(("token", &self.creds.xoxc));
        form.extend(params);

        let response = self
            .http
            .post(&url)
            .header("Cookie", self.cookie_header())
            .form(&form)
            .send()
            .await
            .map_err(|e| CallError::Other(anyhow::anyhow!("request to {method} failed: {e}")))?;

        let status = response.status();

        // The limit can arrive as a 429 with `Retry-After`, or as a 200 body
        // with `error: "ratelimited"`: both have to be handled.
        if status.as_u16() == 429 {
            return Err(CallError::RateLimited {
                retry_after: retry_after_of(response.headers()),
            });
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| CallError::Other(anyhow::anyhow!("unreadable response from {method}: {e}")))?;

        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let error = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown_error");
            if error == "ratelimited" {
                return Err(CallError::RateLimited {
                    retry_after: Duration::from_secs(30),
                });
            }
            return Err(CallError::Other(anyhow::anyhow!(
                "Slack API {method} error: {error} (HTTP {status})"
            )));
        }

        serde_json::from_value(body)
            .map_err(|e| CallError::Other(anyhow::anyhow!("{method} not deserializable: {e}")))
    }

    /// Verify credentials by calling auth.test.
    pub async fn auth_test(&self) -> Result<AuthTestResponse> {
        #[derive(Deserialize)]
        struct RawAuthTest {
            ok: bool,
            #[serde(default)]
            user_id: String,
            #[serde(default)]
            team_id: String,
            #[serde(default)]
            url: String,
            #[serde(default)]
            team: String,
            #[serde(default)]
            user: String,
            #[serde(default)]
            error: Option<String>,
        }

        let url = format!("{}/auth.test", self.base_url);
        let response = self
            .http
            .post(&url)
            .header("Cookie", self.cookie_header())
            .form(&[("token", &self.creds.xoxc)])
            .send()
            .await
            .context("auth.test HTTP request failed")?;

        let raw: RawAuthTest = response
            .json()
            .await
            .context("Failed to parse auth.test response")?;

        if !raw.ok {
            anyhow::bail!(
                "auth.test failed: {}",
                raw.error.unwrap_or_else(|| "unknown".into())
            );
        }

        Ok(AuthTestResponse {
            user_id: raw.user_id,
            team_id: raw.team_id,
            url: raw.url,
            team: raw.team,
            user: raw.user,
        })
    }

    /// Get a WebSocket URL for real-time events.
    ///
    /// Uses Slack's internal `client.getWebSocketURL` endpoint which
    /// works with xoxc tokens and Enterprise Grid workspaces. Falls
    /// back to `rtm.connect` if the internal endpoint fails.
    pub async fn get_websocket_url(&self) -> Result<WebSocketUrlResponse> {
        // Try internal desktop endpoint first
        let endpoints = ["client.getWebSocketURL", "rtm.connect"];

        for endpoint in &endpoints {
            let url = format!("{}/{}", self.base_url, endpoint);
            let response = self
                .http
                .post(&url)
                .header("Cookie", self.cookie_header())
                .form(&[("token", &self.creds.xoxc)])
                .send()
                .await;

            if let Ok(resp) = response {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                if body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let ws_url = body
                        .get("primary_websocket_url")
                        .or_else(|| body.get("url"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let routing = body
                        .get("routing_context")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    return Ok(WebSocketUrlResponse {
                        primary_websocket_url: ws_url,
                        url: None,
                        routing_context: routing,
                    });
                }
            }
        }

        anyhow::bail!("Failed to obtain WebSocket URL from Slack");
    }

    /// Build the full WebSocket URL with auth parameters.
    pub fn build_ws_url(&self, ws_info: &WebSocketUrlResponse) -> Result<String> {
        let base = ws_info
            .primary_websocket_url
            .as_deref()
            .or(ws_info.url.as_deref())
            .context("No WebSocket URL in response")?;

        let routing = ws_info.routing_context.as_deref().unwrap_or("");

        let params = [
            ("token", self.creds.xoxc.as_str()),
            ("sync_desync", "1"),
            ("slack_client", "desktop"),
            (
                "start_args",
                "?agent=client&org_wide_aware=true&agent_version=1695430938&eac_cache_ts=true&cache_ts=0&name_tagging=true&only_self_subteams=true&connect_only=true&ms_latest=true",
            ),
            ("no_query_on_subscribe", "1"),
            ("flannel", "3"),
            ("lazy_channels", "1"),
            ("gateway_server", routing),
            ("batch_presence_aware", "1"),
        ];

        let query: String = params
            .iter()
            .map(|(k, v)| {
                let k: String = url::form_urlencoded::byte_serialize(k.as_bytes()).collect();
                let v: String = url::form_urlencoded::byte_serialize(v.as_bytes()).collect();
                format!("{k}={v}")
            })
            .collect::<Vec<_>>()
            .join("&");

        Ok(format!("{base}?{query}"))
    }

    /// Send a message to a channel.
    pub async fn chat_post_message(
        &self,
        channel: &str,
        text: &str,
    ) -> Result<PostMessageResponse> {
        self.call(
            "chat.postMessage",
            vec![("channel", channel), ("text", text)],
        )
        .await
    }

    pub async fn chat_post_message_thread(
        &self,
        channel: &str,
        text: &str,
        thread_ts: &str,
    ) -> Result<PostMessageResponse> {
        self.call(
            "chat.postMessage",
            vec![
                ("channel", channel),
                ("text", text),
                ("thread_ts", thread_ts),
            ],
        )
        .await
    }

    /// Runs a slash command (`/remind`, `/giphy`, …).
    ///
    /// Slack executes it server-side: whatever it posts to the channel arrives
    /// over the WebSocket, and it may answer with an ephemeral `response` meant
    /// only for us (returned so the caller can show it). An unknown command
    /// comes back as an `error`, which [`Self::call`] turns into an `Err`.
    pub async fn chat_command(
        &self,
        channel: &str,
        command: &str,
        text: &str,
    ) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            response: Option<String>,
        }
        let resp: Resp = self
            .call(
                "chat.command",
                vec![("command", command), ("text", text), ("channel", channel)],
            )
            .await?;
        Ok(resp.response)
    }

    /// A channel's pinned messages (`pins.list`). Each item's `message` is a
    /// raw Slack message, parsed by the caller like any other.
    pub async fn pins_list(&self, channel: &str) -> Result<Vec<serde_json::Value>> {
        #[derive(Deserialize)]
        struct Item {
            #[serde(default)]
            message: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            items: Vec<Item>,
        }
        let resp: Resp = self.call("pins.list", vec![("channel", channel)]).await?;
        Ok(resp.items.into_iter().filter_map(|i| i.message).collect())
    }

    /// Pins a message to its channel.
    pub async fn pins_add(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("pins.add", vec![("channel", channel), ("timestamp", ts)])
            .await?;
        Ok(())
    }

    /// Removes a pin.
    pub async fn pins_remove(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("pins.remove", vec![("channel", channel), ("timestamp", ts)])
            .await?;
        Ok(())
    }

    /// The "Later" list (`saved.list`): references to saved messages, newest
    /// first, with the counters Slack reports. The messages themselves are
    /// fetched separately.
    pub async fn saved_list(&self, count: u32) -> Result<(Vec<SavedItem>, SavedCounts)> {
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            saved_items: Vec<SavedItem>,
            #[serde(default)]
            counts: SavedCounts,
        }
        let n = count.to_string();
        let resp: Resp = self
            .call("saved.list", vec![("count", n.as_str())])
            .await?;
        Ok((resp.saved_items, resp.counts))
    }

    /// Saves a message for later (`saved.add`). "Later" is Slack's internal
    /// list, not part of the public Web API.
    pub async fn saved_add(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "saved.add",
                vec![("item_type", "message"), ("item_id", channel), ("ts", ts)],
            )
            .await?;
        Ok(())
    }

    /// Removes a message from the "Later" list.
    pub async fn saved_delete(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "saved.delete",
                vec![("item_type", "message"), ("item_id", channel), ("ts", ts)],
            )
            .await?;
        Ok(())
    }

    /// The "Threads" inbox: the threads the user is subscribed to (replied to
    /// or mentioned in), newest activity first, with the count of unread
    /// replies. `subscriptions.thread.getView` is internal.
    pub async fn my_threads(&self, limit: u32) -> Result<(Vec<ThreadItem>, i64)> {
        #[derive(Deserialize)]
        struct RawThread {
            root_msg: serde_json::Value,
            #[serde(default)]
            latest_replies: Vec<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            threads: Vec<RawThread>,
            #[serde(default)]
            total_unread_replies: i64,
        }
        let n = limit.to_string();
        let resp: Resp = self
            .call("subscriptions.thread.getView", vec![("limit", n.as_str())])
            .await?;
        let items = resp
            .threads
            .into_iter()
            .map(|t| ThreadItem {
                channel: t
                    .root_msg
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                root: t.root_msg,
                replies: t.latest_replies,
            })
            .collect();
        Ok((items, resp.total_unread_replies))
    }

    /// List conversations (channels) the user has access to.
    pub async fn list_channels(&self, all: bool) -> Result<Vec<ChannelInfo>> {
        let mut all_channels = Vec::new();
        let mut cursor: Option<String> = None;

        // DMs and groups have to be asked for explicitly: without them the
        // sidebar folders stay empty, because that is where the user puts them.
        let types = if all {
            "public_channel,private_channel,im,mpim"
        } else {
            "public_channel,im,mpim"
        };

        loop {
            let mut params = vec![
                ("types", types),
                ("limit", "200"),
                ("exclude_archived", "true"),
            ];

            if let Some(ref c) = cursor {
                params.push(("cursor", c));
            }

            #[derive(Deserialize)]
            struct ConversationsListResponse {
                ok: bool,
                #[serde(default)]
                channels: Vec<ChannelInfo>,
                #[serde(default)]
                response_metadata: ResponseMetadata,
                #[serde(default)]
                error: Option<String>,
            }

            // `users.conversations` and not `conversations.list`: the second
            // enumerates the whole workspace — on a large installation that is
            // hundreds of pages and ends up rate-limited — while this one returns
            // only the user's conversations, which is what a sidebar needs.
            // Measured: 4 requests instead of 30+.
            let resp: ConversationsListResponse =
                self.call("users.conversations", params).await?;

            all_channels.extend(resp.channels);
            tracing::debug!("users.conversations: {} so far", all_channels.len());

            if resp.response_metadata.next_cursor.is_empty() {
                break;
            }
            cursor = Some(resp.response_metadata.next_cursor);
        }

        Ok(all_channels)
    }

    /// Details of a single conversation: name, topic, purpose, member count.
    ///
    /// Works for channels, private groups and DMs alike; a DM has no `name`
    /// (the `user` field is set instead) and empty topic and purpose, which
    /// the defaults in [`ChannelInfo`] already tolerate.
    pub async fn conversations_info(&self, channel: &str) -> Result<ChannelInfo> {
        #[derive(Deserialize)]
        struct Resp {
            channel: ChannelInfo,
        }
        let resp: Resp = self
            .call("conversations.info", vec![("channel", channel)])
            .await?;
        Ok(resp.channel)
    }

    /// The members of a conversation: user ids, not names.
    ///
    /// `conversations.members` is paginated with a cursor; a large channel
    /// needs several calls. Only the first page is fetched here, up to
    /// `limit` (Slack caps it at 1000): the full list of a big workspace is
    /// not worth the requests, and the panel says when what is shown is
    /// shorter than `num_members`.
    pub async fn conversations_members(&self, channel: &str, limit: u32) -> Result<Vec<String>> {
        let limit = limit.to_string();
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            members: Vec<String>,
        }
        let resp: Resp = self
            .call(
                "conversations.members",
                vec![("channel", channel), ("limit", limit.as_str())],
            )
            .await?;
        Ok(resp.members)
    }

    /// Get channel history.
    pub async fn conversations_history(
        &self,
        channel: &str,
        limit: Option<u32>,
    ) -> Result<Vec<serde_json::Value>> {
        self.conversations_history_range(channel, limit, None, None)
            .await
    }

    /// `conversations.history` with a time window.
    ///
    /// `oldest` returns only the messages *after* that `ts` (incremental sync
    /// forwards), `latest` only those *before* (backfill backwards). Both
    /// exclusive, as per the Slack API.
    pub async fn conversations_history_range(
        &self,
        channel: &str,
        limit: Option<u32>,
        oldest: Option<&str>,
        latest: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let limit = limit.unwrap_or(100).to_string();
        #[derive(Deserialize)]
        struct HistoryResponse {
            ok: bool,
            #[serde(default)]
            messages: Vec<serde_json::Value>,
            #[serde(default)]
            error: Option<String>,
        }

        let mut params = vec![("channel", channel), ("limit", limit.as_str())];
        if let Some(o) = oldest {
            params.push(("oldest", o));
        }
        if let Some(l) = latest {
            params.push(("latest", l));
        }

        let resp: HistoryResponse = self.call("conversations.history", params).await?;

        Ok(resp.messages)
    }

    /// Uploads a file and shares it in a channel.
    ///
    /// `files.upload` is retired. The current flow has three steps: ask for an
    /// upload URL, send the content there, and complete by naming the channel.
    /// Verified that it accepts an `xoxc` token.
    pub async fn upload_file(
        &self,
        channel: &str,
        filename: &str,
        bytes: Vec<u8>,
        comment: Option<&str>,
        // The root message's `ts`, to attach inside a thread.
        thread_ts: Option<&str>,
    ) -> Result<String> {
        #[derive(Deserialize)]
        struct UploadUrl {
            ok: bool,
            #[serde(default)]
            upload_url: String,
            #[serde(default)]
            file_id: String,
            #[serde(default)]
            error: Option<String>,
        }

        let len = bytes.len().to_string();
        let step1: UploadUrl = self
            .call(
                "files.getUploadURLExternal",
                vec![("filename", filename), ("length", &len)],
            )
            .await
            .context("request for the upload URL failed")?;

        let mut req = self.http.post(&step1.upload_url);
        // The session cookie goes to Slack's own hosts only: the URL comes from
        // Slack, but a poisoned response must not leak the session (the same rule
        // as `download_bytes`).
        if crate::net::is_slack_host(&step1.upload_url) {
            req = req.header("Cookie", self.cookie_header());
        }
        let status = req
            .body(bytes)
            .send()
            .await
            .context("failed to send the content")?
            .status();
        if !status.is_success() {
            anyhow::bail!("upload rejected: HTTP {status}");
        }

        // `files` is JSON, not a repeated form field.
        let files = serde_json::json!([{ "id": &step1.file_id, "title": filename }])
            .to_string();
        let mut params = vec![("files", files.as_str()), ("channel_id", channel)];
        if let Some(c) = comment.filter(|c| !c.trim().is_empty()) {
            params.push(("initial_comment", c));
        }
        // Without this the file ends up in the channel even when you were
        // replying in a thread.
        if let Some(t) = thread_ts.filter(|t| !t.is_empty()) {
            params.push(("thread_ts", t));
        }
        let _: serde_json::Value = self
            .call("files.completeUploadExternal", params)
            .await
            .context("failed to complete the upload")?;
        Ok(step1.file_id)
    }

    /// Searches the workspace's messages.
    pub async fn search_messages(&self, query: &str, count: u32) -> Result<Vec<SearchHit>> {
        #[derive(Deserialize, Default)]
        struct Matches {
            #[serde(default)]
            matches: Vec<SearchHit>,
        }
        #[derive(Deserialize)]
        struct Resp {
            ok: bool,
            #[serde(default)]
            messages: Matches,
            #[serde(default)]
            error: Option<String>,
        }
        let n = count.to_string();
        let r: Resp = self
            .call(
                "search.messages",
                vec![("query", query), ("count", &n), ("sort", "timestamp")],
            )
            .await?;
        Ok(r.messages.matches)
    }

    /// The permanent link to a message, built locally (no round-trip).
    pub fn permalink(&self, channel: &str, ts: &str, thread_ts: Option<&str>) -> String {
        let host = self
            .creds
            .workspace_url
            .trim_start_matches("https://")
            .trim_end_matches('/');
        let digits: String = ts.chars().filter(char::is_ascii_digit).collect();
        match thread_ts.filter(|t| !t.is_empty()) {
            Some(root) => {
                let root: String = root.chars().filter(char::is_ascii_digit).collect();
                format!(
                    "https://{host}/archives/{channel}/p{digits}?thread_ts={root}&cid={channel}"
                )
            }
            None => format!("https://{host}/archives/{channel}/p{digits}"),
        }
    }

    /// The permanent link to a message.
    pub async fn chat_get_permalink(&self, channel: &str, ts: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Resp {
            ok: bool,
            #[serde(default)]
            permalink: String,
            #[serde(default)]
            error: Option<String>,
        }
        let r: Resp = self
            .call(
                "chat.getPermalink",
                vec![("channel", channel), ("message_ts", ts)],
            )
            .await?;
        Ok(r.permalink)
    }

    /// Edits one of your own messages.
    pub async fn chat_update(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "chat.update",
                vec![("channel", channel), ("ts", ts), ("text", text)],
            )
            .await?;
        Ok(())
    }

    /// Deletes one of your own messages.
    pub async fn chat_delete(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("chat.delete", vec![("channel", channel), ("ts", ts)])
            .await?;
        Ok(())
    }

    /// Adds a reaction to a message.
    ///
    /// `already_reacted` is not an error: it means it was there already, which is
    /// the state we wanted.
    pub async fn reactions_add(&self, channel: &str, ts: &str, name: &str) -> Result<()> {
        match self
            .call::<serde_json::Value>(
                "reactions.add",
                vec![("channel", channel), ("timestamp", ts), ("name", name)],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("already_reacted") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Removes a reaction. `no_reaction` is benign, for the same reason.
    pub async fn reactions_remove(&self, channel: &str, ts: &str, name: &str) -> Result<()> {
        match self
            .call::<serde_json::Value>(
                "reactions.remove",
                vec![("channel", channel), ("timestamp", ts), ("name", name)],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("no_reaction") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Marks the conversation read up to `ts`.
    pub async fn conversations_mark(&self, channel: &str, ts: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("conversations.mark", vec![("channel", channel), ("ts", ts)])
            .await?;
        Ok(())
    }

    /// The sidebar Slack considers active, with the unread state.
    ///
    /// An internal endpoint, a single request. It serves to tell the conversations
    /// Slack really shows from the whole history `users.conversations` returns: on
    /// this workspace 116 against 494, because the `mpdm-…` groups pile up over the
    /// years.
    pub async fn client_counts(&self) -> Result<Counts> {
        self.call("client.counts", vec![]).await
    }

    /// Opens (or retrieves) the direct conversation with a user.
    ///
    /// It is needed to write to a colleague you have never spoken to: that DM does
    /// not exist until you open it, so it would not appear in
    /// `conversations.list`.
    pub async fn conversations_open(&self, user_id: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Opened {
            id: String,
        }
        #[derive(Deserialize)]
        struct OpenResponse {
            ok: bool,
            #[serde(default)]
            channel: Option<Opened>,
            #[serde(default)]
            error: Option<String>,
        }
        let resp: OpenResponse = self
            .call("conversations.open", vec![("users", user_id)])
            .await?;
        resp.channel
            .map(|c| c.id)
            .context("conversations.open did not return a channel")
    }

    /// Opens a conversation with one or more users, creating it if needed.
    ///
    /// One user is a direct message, several a multi-person one:
    /// `conversations.open` takes a comma-separated list and returns the group
    /// already there when it exists.
    pub async fn open_conversation(&self, users: &[String]) -> Result<String> {
        #[derive(Deserialize)]
        struct Opened {
            id: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            channel: Opened,
        }
        let joined = users.join(",");
        let resp: Resp = self
            .call("conversations.open", vec![("users", &joined)])
            .await?;
        Ok(resp.channel.id)
    }

    /// Creates a channel and returns its id and (normalised) name.
    pub async fn create_channel(&self, name: &str, is_private: bool) -> Result<(String, String)> {
        #[derive(Deserialize)]
        struct Created {
            id: String,
            #[serde(default)]
            name: String,
        }
        #[derive(Deserialize)]
        struct Resp {
            channel: Created,
        }
        let private = if is_private { "true" } else { "false" };
        let resp: Resp = self
            .call(
                "conversations.create",
                vec![("name", name), ("is_private", private)],
            )
            .await?;
        Ok((resp.channel.id, resp.channel.name))
    }

    /// Renames a channel.
    pub async fn rename_channel(&self, channel: &str, name: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "conversations.rename",
                vec![("channel", channel), ("name", name)],
            )
            .await?;
        Ok(())
    }

    /// Invites users to a channel.
    pub async fn invite_to_channel(&self, channel: &str, users: &[String]) -> Result<()> {
        let joined = users.join(",");
        let _: serde_json::Value = self
            .call(
                "conversations.invite",
                vec![("channel", channel), ("users", &joined)],
            )
            .await?;
        Ok(())
    }

    /// Removes a user from a channel.
    pub async fn kick_from_channel(&self, channel: &str, user: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "conversations.kick",
                vec![("channel", channel), ("user", user)],
            )
            .await?;
        Ok(())
    }

    /// Sets a channel's topic.
    pub async fn set_channel_topic(&self, channel: &str, topic: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call(
                "conversations.setTopic",
                vec![("channel", channel), ("topic", topic)],
            )
            .await?;
        Ok(())
    }

    /// Leaves a channel.
    pub async fn leave_channel(&self, channel: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("conversations.leave", vec![("channel", channel)])
            .await?;
        Ok(())
    }

    /// Archives a channel (reversible).
    pub async fn archive_channel(&self, channel: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("conversations.archive", vec![("channel", channel)])
            .await?;
        Ok(())
    }

    /// The sidebar sections: the "folders" channels are organised into.
    ///
    /// An internal endpoint. Besides the ones the user created (`type:
    /// "standard"`) it returns the system sections — Direct Messages, Starred,
    /// Apps — which in the sidebar are generated and contain no `channel_ids`.
    pub async fn channel_sections(&self) -> Result<Vec<ChannelSection>> {
        #[derive(Deserialize)]
        struct SectionsResponse {
            ok: bool,
            #[serde(default)]
            channel_sections: Vec<ChannelSection>,
            #[serde(default)]
            error: Option<String>,
        }
        let resp: SectionsResponse = self.call("users.channelSections.list", vec![]).await?;
        Ok(resp.channel_sections)
    }

    /// The workspace's custom emoji: name -> image URL.
    ///
    /// Aliases (`"alias:other_name"`) are resolved, so the caller gets only real
    /// URLs.
    pub async fn emoji_list(&self) -> Result<std::collections::HashMap<String, String>> {
        #[derive(Deserialize)]
        struct EmojiResponse {
            ok: bool,
            #[serde(default)]
            emoji: std::collections::HashMap<String, String>,
            #[serde(default)]
            error: Option<String>,
        }
        let resp: EmojiResponse = self.call("emoji.list", vec![]).await?;

        let raw = resp.emoji;
        let resolved = raw
            .iter()
            .filter_map(|(name, target)| {
                let url = match target.strip_prefix("alias:") {
                    Some(alias) => raw.get(alias)?,
                    None => target,
                };
                url.starts_with("http").then(|| (name.clone(), url.clone()))
            })
            .collect();
        Ok(resolved)
    }

    /// A single user, even one who is not on our team.
    ///
    /// It is needed for whoever writes in spaces shared through Slack Connect:
    /// those people do not appear in `users.list`, which lists only our own
    /// organisation.
    ///
    /// The response is `{ok, user:{…}}`: reading it as if the user were at the top
    /// level fails with "missing field `id`", which is what it used to do.
    pub async fn users_info(&self, user: &str) -> Result<UserInfo> {
        #[derive(Deserialize)]
        struct Resp {
            user: UserInfo,
        }
        let r: Resp = self.call("users.info", vec![("user", user)]).await?;
        Ok(r.user)
    }

    /// Sets our own status: emoji and text, without an expiration.
    ///
    /// `emoji` is the shortcode with the colons, as Slack returns it
    /// (`:palm_tree:`), and an empty string clears it. The profile is sent as a
    /// JSON string in the `profile` form field, which is how the web client
    /// calls this endpoint.
    pub async fn users_profile_set(&self, emoji: &str, text: &str) -> Result<()> {
        let profile = serde_json::json!({
            "status_text": text,
            "status_emoji": emoji,
            "status_expiration": 0,
        })
        .to_string();
        let _: serde_json::Value = self
            .call("users.profile.set", vec![("profile", profile.as_str())])
            .await?;
        Ok(())
    }

    /// Sets our presence: `auto` means active, `away` means away.
    pub async fn users_set_presence(&self, presence: &str) -> Result<()> {
        let _: serde_json::Value = self
            .call("users.setPresence", vec![("presence", presence)])
            .await?;
        Ok(())
    }

    /// Our **own** presence as Slack reports it (`active` / `away`): no `user`
    /// argument means "me". Logged after each assert, because `setPresence(auto)`
    /// is not a force-active — it clears a *manual* away and hands the rest back to
    /// Slack (see `docs/presence.md`).
    pub async fn users_get_presence(&self) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Reply {
            #[serde(default)]
            presence: String,
        }
        let r: Reply = self.call("users.getPresence", Vec::new()).await?;
        Ok(r.presence)
    }

    pub async fn conversations_replies(
        &self,
        channel: &str,
        ts: &str,
    ) -> Result<Vec<serde_json::Value>> {
        #[derive(Deserialize)]
        struct RepliesResponse {
            ok: bool,
            #[serde(default)]
            messages: Vec<serde_json::Value>,
            #[serde(default)]
            error: Option<String>,
        }
        let resp: RepliesResponse = self
            .call(
                "conversations.replies",
                vec![("channel", channel), ("ts", ts)],
            )
            .await?;
        Ok(resp.messages)
    }

/// Joins (or creates) a huddle: `rooms.join`.
///
/// It is the method the official client uses. The response contains, under
/// `call.free_willy`, the AWS `CreateMeeting` / `CreateAttendee` responses to
/// feed to the Chime SDK — see `docs/huddle-protocol.md`.
///
/// It returns the raw body: the protocol is undocumented and until we know it
/// well every discarded field is information lost.
pub async fn rooms_join(
    &self,
    channel_id: &str,
    regions: &str,
    multidevice: bool,
) -> Result<serde_json::Value> {
    self.call_raw(
        "rooms.join",
        vec![
            ("channel_id", channel_id),
            ("regions", regions),
            ("multidevice", if multidevice { "true" } else { "false" }),
        ],
    )
    .await
}

/// Leaves a huddle: `rooms.leave`.
///
/// The errors `channel_not_found`, `room_not_found`, `invalid_channel_id` and
/// `attendee_not_found` are benign (the official client ignores them): leaving
/// a room that is no longer there is the normal case, not a failure.
pub async fn rooms_leave(
    &self,
    channel_id: &str,
    call_id: &str,
    attendee_id: &str,
    reason: &str,
) -> Result<serde_json::Value> {
    self.call_raw(
        "rooms.leave",
        vec![
            ("channel_id", channel_id),
            ("call_id", call_id),
            ("attendee_id", attendee_id),
            ("reason", reason),
        ],
    )
    .await
}

/// Like [`Client::call`] but returns the raw JSON.
///
/// `call` deserialises into a type and discards the rest; here everything is
/// needed, including the body of `ok: false` responses — Slack's error code is
/// often the most useful information when mapping an internal endpoint.
pub async fn call_raw(
    &self,
    method: &str,
    params: Vec<(&str, &str)>,
) -> Result<serde_json::Value> {
    let url = format!("{}/{}", self.base_url, method);

    let mut form: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 1);
    form.push(("token", &self.creds.xoxc));
    form.extend(params);

    let response = self
        .http
        .post(&url)
        .header("Cookie", self.cookie_header())
        .form(&form)
        .send()
        .await
        .context(format!("HTTP request to {method} failed"))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context(format!("Failed to parse {method} response"))?;

    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_error");
        anyhow::bail!("Slack API {method} error: {error} (HTTP {status})");
    }

    Ok(body)
}

/// Create a ScreenHero room for a call (Slack's internal call API).
///
/// This is what the official Slack web/desktop client uses — NOT `calls.add`.
/// Works with xoxc session tokens because it's an internal endpoint.
///
/// If `use_account_host` is true, sends to `https://account.slack.com/api/`
/// instead of the workspace-specific URL. Per HackerOne reports, some
/// screenhero endpoints only exist on the account-level API server.
pub async fn screenhero_rooms_create(
    &self,
    channel_id: &str,
    use_account_host: bool,
) -> Result<ScreenheroRoomsCreateResponse> {
    let base = if use_account_host {
        "https://account.slack.com/api".to_string()
    } else {
        self.base_url.clone()
    };
    let url = format!("{}/screenhero.rooms.create", base);
    let response = self
        .http
        .post(&url)
        .header("Cookie", self.cookie_header())
        .form(&[
            ("token", self.creds.xoxc.as_str()),
            ("channel", channel_id),
            ("regions", "west|east|ap|eu"),
            ("protocol", "1.0"),
        ])
        .send()
        .await
        .context("screenhero.rooms.create HTTP request failed")?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .context("Failed to parse screenhero.rooms.create response")?;

    if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown_error");
        anyhow::bail!("screenhero.rooms.create error: {error} (HTTP {status})");
    }

    serde_json::from_value(body)
        .context("Failed to deserialize screenhero.rooms.create response")
}

/// Register a call in a channel via Slack's Calls API.
pub async fn calls_add(
    &self,
    external_unique_id: &str,
    join_url: &str,
    channel_id: &str,
    title: Option<&str>,
) -> Result<CallsAddResponse> {
    let mut params: Vec<(&str, &str)> = vec![
        ("external_unique_id", external_unique_id),
        ("join_url", join_url),
        ("channel_id", channel_id),
    ];
    if let Some(title) = title {
        params.push(("title", title));
    }

    #[derive(Deserialize)]
    struct RawResponse {
        ok: bool,
        #[serde(default)]
        call: Option<CallsAddResponse>,
        #[serde(default)]
        error: Option<String>,
    }

    let raw: RawResponse = self.call("calls.add", params).await?;
    raw.call.context("calls.add returned ok but no call data")
}

/// End a call registered via calls.add.
pub async fn calls_end(&self, call_id: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct RawResponse {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    }

    let raw: RawResponse = self
        .call("calls.end", vec![("id", call_id)])
        .await?;

    if !raw.ok {
        anyhow::bail!(
            "calls.end error: {}",
            raw.error.unwrap_or_else(|| "unknown".into())
        );
    }
    Ok(())
}

/// Add a participant to a registered call.
pub async fn calls_participants_add(
    &self,
    call_id: &str,
    users: &[&str],
) -> Result<()> {
    let users_json = serde_json::to_string(users)?;

    #[derive(Deserialize)]
    struct RawResponse {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    }

    let raw: RawResponse = self
        .call(
            "calls.participants.add",
            vec![("id", call_id), ("users", &users_json)],
        )
        .await?;

    if !raw.ok {
        anyhow::bail!(
            "calls.participants.add error: {}",
            raw.error.unwrap_or_else(|| "unknown".into())
        );
    }
    Ok(())
}

/// A bot's name and icon, from `bots.info`.
///
/// It is needed because many integrations' messages carry only `bot_id`: no
/// `user`, no `bot_profile`, and the author would stay "?".
pub async fn bot_info(&self, bot_id: &str) -> Result<(String, String)> {
    #[derive(Deserialize)]
    struct Bot {
        #[serde(default)]
        name: String,
        #[serde(default)]
        icons: std::collections::HashMap<String, String>,
    }
    #[derive(Deserialize)]
    struct Resp {
        bot: Bot,
    }
    let resp: Resp = self.call("bots.info", vec![("bot", bot_id)]).await?;
    let icon = ["image_72", "image_48", "image_36"]
        .iter()
        .find_map(|k| resp.bot.icons.get(*k).cloned())
        .unwrap_or_default();
    Ok((resp.bot.name, icon))
}

/// The app id of a bot, from `bots.info`, for the action dispatch.
pub async fn bot_app_id(&self, bot_id: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Bot {
        #[serde(default)]
        app_id: String,
    }
    #[derive(Deserialize)]
    struct Resp {
        bot: Bot,
    }
    let resp: Resp = self.call("bots.info", vec![("bot", bot_id)]).await?;
    Ok(resp.bot.app_id)
}

/// Dispatches an action-button click, as the web client does.
///
/// `actions`, `container` and `state` travel as JSON strings in form fields;
/// the `container` names the message the buttons live on. `app_id` is omitted
/// when it could not be resolved, and the server's error is surfaced.
pub async fn blocks_actions(
    &self,
    service_id: &str,
    app_id: Option<&str>,
    team_id: &str,
    channel_id: &str,
    message_ts: &str,
    actions: &[serde_json::Value],
) -> Result<()> {
    let actions_json = serde_json::to_string(actions)?;
    let container = serde_json::json!({
        "type": "message",
        "message_ts": message_ts,
        "channel_id": channel_id,
        "is_ephemeral": false,
    })
    .to_string();
    let state = serde_json::json!({ "values": {} }).to_string();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let client_token = format!("web-{ms}");

    let mut params: Vec<(&str, &str)> = vec![
        ("service_id", service_id),
        ("service_team_id", team_id),
        ("actions", actions_json.as_str()),
        ("container", container.as_str()),
        ("state", state.as_str()),
        ("client_token", client_token.as_str()),
    ];
    if let Some(app) = app_id {
        params.push(("app_id", app));
    }
    let _: serde_json::Value = self.call("blocks.actions", params).await?;
    Ok(())
}

/// The "do not disturb" state of several users in a single call.
///
/// `dnd.teamInfo` takes a list, unlike `users.getPresence`, which ignores the
/// plural and answers only for itself: it is the only way not to make one
/// request per person.
pub async fn dnd_team_info(
    &self,
    users: &[String],
) -> Result<std::collections::HashMap<String, DndInfo>> {
        if users.is_empty() {
            return Ok(Default::default());
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            users: std::collections::HashMap<String, DndInfo>,
        }
        let joined = users.join(",");
        let resp: Resp = self
            .call("dnd.teamInfo", vec![("users", joined.as_str())])
            .await?;
        Ok(resp.users)
    }

/// List all users in the workspace.
pub async fn users_list(&self) -> Result<Vec<UserInfo>> {
        let mut all_users = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            #[derive(Deserialize)]
            struct UsersListResponse {
                ok: bool,
                #[serde(default)]
                members: Vec<UserInfo>,
                #[serde(default)]
                response_metadata: ResponseMetadata,
                #[serde(default)]
                error: Option<String>,
            }
            let mut params = vec![("limit", "200")];
            if let Some(ref c) = cursor {
                params.push(("cursor", c));
            }
            let resp: UsersListResponse = self.call("users.list", params).await?;
            all_users.extend(resp.members);
            if resp.response_metadata.next_cursor.is_empty() {
                break;
            }
            cursor = Some(resp.response_metadata.next_cursor);
        }
        Ok(all_users)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user with the given names; the username is fixed so the fallback is
    /// predictable.
    fn user(real: &str, display: &str, profile: &UserProfile) -> UserInfo {
        UserInfo {
            id: "U1".to_string(),
            name: "giulia.conti".to_string(),
            real_name: real.to_string(),
            display_name: display.to_string(),
            profile: profile.clone(),
            is_bot: false,
            is_app_user: false,
        }
    }

    #[test]
    fn full_name_prefers_the_real_name_over_the_nickname() {
        // `users.info` puts the full name on `real_name` and the alias on the
        // profile's `display_name`: the alias must not win.
        let prof = UserProfile {
            display_name: "giuly".to_string(),
            real_name: "Giulia Conti".to_string(),
            ..Default::default()
        };
        assert_eq!(user("Giulia Conti", "", &prof).full_name(), "Giulia Conti");
    }

    #[test]
    fn full_name_works_from_the_profile_alone() {
        // `users.list` items carry the name under `profile.real_name`.
        let prof = UserProfile {
            real_name: "Marco Bianchi".to_string(),
            ..Default::default()
        };
        assert_eq!(user("", "", &prof).full_name(), "Marco Bianchi");
    }

    #[test]
    fn full_name_falls_back_to_the_alias_then_the_username() {
        let alias = UserProfile {
            display_name: "giuly".to_string(),
            ..Default::default()
        };
        assert_eq!(user("", "", &alias).full_name(), "giuly");
        // Nothing but the handle: still better than an empty label.
        assert_eq!(
            user("", "", &UserProfile::default()).full_name(),
            "giulia.conti"
        );
    }

    /// `download_bytes` (the `file-get` transport) reads the body and **refuses**
    /// anything over the cap — whether the size is known (`Content-Length`) or only
    /// discovered while streaming — and never sends the session cookie to a
    /// non-Slack host.
    #[tokio::test]
    async fn download_bytes_reads_and_caps() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            // The body is 5 bytes; the third response omits `Content-Length`, so
            // the cap is enforced by the streaming loop, not the header.
            let responses: [&[u8]; 3] = [
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello",
            ];
            for resp in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = s.write_all(resp);
            }
        });
        let client = Client::new(crate::auth::Credentials {
            xoxc: "x".into(),
            xoxd: "d".into(),
            workspace_url: "example.slack.com".into(),
            user_id: "U1".into(),
            team_id: "T1".into(),
        });
        let url = format!("http://127.0.0.1:{}/f", addr.port());
        // Under the cap: the body is returned.
        assert_eq!(client.download_bytes(&url, 16).await.unwrap(), b"hello");
        // Over the cap, caught by `Content-Length`.
        let err = client.download_bytes(&url, 3).await.unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
        // Over the cap, caught by the streaming loop.
        let err = client.download_bytes(&url, 3).await.unwrap_err();
        assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
        server.join().unwrap();
        // A non-Slack host never receives the session cookie.
        while let Ok(req) = rx.try_recv() {
            assert!(
                !req.to_ascii_lowercase().contains("cookie"),
                "the cookie leaked to a non-Slack host: {req}"
            );
        }
    }
}
