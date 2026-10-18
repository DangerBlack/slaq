use serde::Deserialize;

/// A generic Slack WebSocket event envelope.
///
/// All events have at minimum a `type` field. Many have a `subtype`.
#[derive(Debug, Clone, Deserialize)]
pub struct WsEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub subtype: Option<String>,

    // Message fields
    #[serde(default)]
    pub channel: Option<String>,
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

    // Channel/team fields
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub purpose: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,

    // Presence
    #[serde(default)]
    pub presence: Option<String>,

    // Call fields
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,

    // Error
    #[serde(default)]
    pub error: Option<WsError>,

    // Raw payload for untyped events
    #[serde(default)]
    pub payload: Option<serde_json::Value>,

    // Catch-all for any unhandled fields
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// WebSocket error structure.
#[derive(Debug, Clone, Deserialize)]
pub struct WsError {
    pub code: Option<i64>,
    pub msg: Option<String>,
}

impl WsEvent {
    pub fn is_message(&self) -> bool {
        self.event_type == "message"
    }

    pub fn is_call(&self) -> bool {
        self.event_type.starts_with("call_") || self.event_type == "channel_calls"
    }

    pub fn is_presence(&self) -> bool {
        self.event_type == "presence_change"
    }

    pub fn is_hello(&self) -> bool {
        self.event_type == "hello"
    }
}

/// An event alongside its original JSON.
///
/// [`WsEvent`] is convenient to query but it is a partial view: writing to the
/// cache needs the payload intact, because it is the same shape
/// `conversations.history` returns and the one the rest of the client already
/// knows how to read.
#[derive(Debug, Clone)]
pub struct WsEnvelope {
    pub event: WsEvent,
    pub raw: serde_json::Value,
}

/// What the WebSocket reader tells its consumer.
///
/// The connection state travels on the same channel as the events, so that the
/// UI can say whether it is really listening or showing stale data.
#[derive(Debug, Clone)]
pub enum WsUpdate {
    Connected,
    Disconnected(String),
    Event(WsEnvelope),
}

/// What to do in the cache with a `message` event.
#[derive(Debug, PartialEq)]
pub enum CacheOp<'a> {
    /// Write this message object.
    Upsert(&'a serde_json::Value),
    /// Remove the message with this `ts`.
    Delete(&'a str),
    /// Does not concern the history.
    Ignore,
}

/// Decides the cache operation for a raw WebSocket event.
///
/// It is needed because `message` events do not all have the same shape:
///
/// - normal: the message fields are on the event itself;
/// - `message_changed` / `message_replied`: the real message is nested under
///   `message`, and the event's `ts` is that of the *edit*, not of the
///   message. Writing the event as it comes would create a ghost row;
/// - `message_deleted`: carries only `deleted_ts`.
///
/// Recognising the nesting does not look at the subtype but at the presence of
/// a `message.ts`, so any other subtype with the same shape is handled without
/// having to list it.
pub fn cache_op(raw: &serde_json::Value) -> CacheOp<'_> {
    if raw.get("type").and_then(|v| v.as_str()) != Some("message") {
        return CacheOp::Ignore;
    }

    if raw.get("subtype").and_then(|v| v.as_str()) == Some("message_deleted") {
        return match raw.get("deleted_ts").and_then(|v| v.as_str()) {
            Some(ts) => CacheOp::Delete(ts),
            None => CacheOp::Ignore,
        };
    }

    if let Some(nested) = raw.get("message") {
        if nested.get("ts").and_then(|v| v.as_str()).is_some() {
            return CacheOp::Upsert(nested);
        }
    }

    if raw.get("ts").and_then(|v| v.as_str()).is_some() {
        CacheOp::Upsert(raw)
    } else {
        CacheOp::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_message_is_cached_as_is() {
        let e = json!({ "type": "message", "channel": "C1", "ts": "100.1", "text": "ciao" });
        assert_eq!(cache_op(&e), CacheOp::Upsert(&e));
    }

    #[test]
    fn edited_message_uses_the_nested_object_not_the_event() {
        let e = json!({
            "type": "message", "subtype": "message_changed", "channel": "C1",
            "ts": "200.9",
            "message": { "ts": "100.1", "text": "modificato" },
            "previous_message": { "ts": "100.1", "text": "originale" }
        });
        match cache_op(&e) {
            CacheOp::Upsert(m) => {
                assert_eq!(m["ts"], "100.1", "the message ts must win");
                assert_eq!(m["text"], "modificato");
            }
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn message_replied_is_handled_like_an_edit() {
        let e = json!({
            "type": "message", "subtype": "message_replied", "channel": "C1",
            "ts": "300.4",
            "message": { "ts": "100.1", "text": "radice", "reply_count": 2 }
        });
        match cache_op(&e) {
            CacheOp::Upsert(m) => assert_eq!(m["ts"], "100.1"),
            other => panic!("expected Upsert, got {other:?}"),
        }
    }

    #[test]
    fn deleted_message_reports_the_deleted_ts() {
        let e = json!({
            "type": "message", "subtype": "message_deleted",
            "channel": "C1", "ts": "400.2", "deleted_ts": "100.1"
        });
        assert_eq!(cache_op(&e), CacheOp::Delete("100.1"));
    }

    #[test]
    fn non_message_events_are_ignored() {
        for e in [
            json!({ "type": "user_typing", "channel": "C1" }),
            json!({ "type": "presence_change", "user": "U1" }),
            json!({ "type": "hello" }),
        ] {
            assert_eq!(cache_op(&e), CacheOp::Ignore, "{e}");
        }
    }

    #[test]
    fn message_without_any_ts_is_ignored_rather_than_guessed() {
        let e = json!({ "type": "message", "channel": "C1", "text": "senza ts" });
        assert_eq!(cache_op(&e), CacheOp::Ignore);
    }
}
