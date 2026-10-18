use serde::Deserialize;

/// A message from conversations.history or conversations.replies.
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub ts: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub parent_user_id: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub icons: Option<serde_json::Value>,
    #[serde(default)]
    pub files: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub reactions: Option<Vec<Reaction>>,
    #[serde(default)]
    pub blocks: Option<Vec<serde_json::Value>>,
}

/// A reaction on a message.
#[derive(Debug, Clone, Deserialize)]
pub struct Reaction {
    pub name: String,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub count: i64,
}

/// Response from conversations.replies.
#[derive(Debug, Deserialize)]
pub struct RepliesResponse {
    pub ok: bool,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub error: Option<String>,
}
