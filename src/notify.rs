//! Desktop notifications.
//!
//! We do not decide what to notify. Slack works it out server side —
//! mentions, keywords, muted channels, DND, per-channel preferences — and
//! pushes a ready-made `desktop_notification` event over the WebSocket.
//! Reimplementing those rules would mean getting them wrong: here we only
//! render what arrives.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};

/// Runtime switch for desktop notifications. `SLAQ_NO_NOTIFY` still turns them
/// off for good; the settings panel flips this one.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// The payload of a `desktop_notification` event.
///
/// Every field is optional on purpose: it is an undocumented internal event,
/// and a missing field must not stop the notification from being shown.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DesktopNotification {
    /// Usually the name of the workspace.
    #[serde(default)]
    pub title: Option<String>,
    /// Usually the channel, e.g. `#general`.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// The text, already formatted by Slack as `author: message`.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    /// The `ts` of the message that produced it.
    #[serde(default)]
    pub msg: Option<String>,
    /// The name of the sound Slack picked; we use the system one.
    #[serde(default)]
    pub ssb_filename: Option<String>,
}

impl DesktopNotification {
    /// Top line: the channel if there is one, otherwise the workspace.
    pub fn heading(&self) -> String {
        self.subtitle
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| self.title.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| t!("notify.app_name").to_string())
    }

    pub fn body(&self) -> String {
        self.content
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| t!("notify.new_message").to_string())
    }
}

/// What is needed to decide whether to notify.
pub struct Context<'a> {
    /// Our own user id: used to recognise mentions and to avoid
    /// notifying ourselves of our own messages.
    pub my_user_id: &'a str,
    /// The name of the conversation, for the heading.
    pub channel_name: &'a str,
    /// Whether it is a direct message: there we always notify.
    pub is_dm: bool,
    /// The author's name, already resolved.
    pub author: &'a str,
    /// The users' names, to write mentions in full: in the body of a
    /// notification `@U01ABCDEF` says nothing to anyone.
    pub names: &'a std::collections::HashMap<String, String>,
}

/// Decides whether a `message` event deserves a notification, and builds it.
pub fn from_message(raw: &serde_json::Value, ctx: &Context) -> Option<DesktopNotification> {
    if raw.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    // Slack says explicitly when it does not want a notification: that is
    // the case for service bot messages and for edits.
    if raw.get("suppress_notification").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    // Subtypes that are not real messages (joins, edits, deletions)
    // must not be notified.
    if let Some(sub) = raw.get("subtype").and_then(|v| v.as_str()) {
        if sub != "bot_message" && sub != "thread_broadcast" && sub != "file_share" {
            return None;
        }
    }
    // Notifying yourself of your own messages is the fastest way to get
    // anyone to turn notifications off.
    if raw.get("user").and_then(|v| v.as_str()) == Some(ctx.my_user_id) {
        return None;
    }

    let text = raw.get("text").and_then(|v| v.as_str()).unwrap_or_default();
    let mentioned = !ctx.my_user_id.is_empty() && text.contains(&format!("<@{}>", ctx.my_user_id));
    let broadcast = ["<!here", "<!channel", "<!everyone"]
        .iter()
        .any(|b| text.contains(b));

    if !(ctx.is_dm || mentioned || broadcast) {
        return None;
    }

    let body = resolve_mentions(&crate::app::mrkdwn::to_plain(text), ctx.names);
    let body = if body.is_empty() {
        t!("notify.new_message").to_string()
    } else {
        format!("{}: {}", ctx.author, truncate(&body, 180))
    };

    Some(DesktopNotification {
        title: Some(t!("notify.app_name").to_string()),
        subtitle: Some(ctx.channel_name.to_string()),
        content: Some(body),
        channel: raw.get("channel").and_then(|v| v.as_str()).map(str::to_string),
        msg: raw.get("ts").and_then(|v| v.as_str()).map(str::to_string),
        ssb_filename: None,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let short: String = s.chars().take(max - 1).collect();
    format!("{short}\u{2026}")
}

/// Shows the notification on the desktop.
pub fn show(n: &DesktopNotification) -> Result<()> {
    notify_rust::Notification::new()
        .summary(&n.heading())
        .body(&n.body())
        .appname("slaq")
        .icon("slack")
        // Without an explicit urgency GNOME treats it as low, and low
        // priority notifications do not open the banner: they only end up in the
        // drop-down list. You heard the sound and saw nothing.
        .urgency(notify_rust::Urgency::Normal)
        .show()
        .context("failed to send the desktop notification")?;
    Ok(())
}

/// Replaces `@U123` with the person's name.
///
/// `to_plain` leaves the id because it does not know the names: that is the
/// wrong level to know them at. Here we do have them, and reading
/// `@U01ABCDEF` in a notification wastes half the message.
fn resolve_mentions(text: &str, names: &std::collections::HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (id, name) in names {
        let from = format!("@{id}");
        if out.contains(&from) {
            out = out.replace(&from, &format!("@{name}"));
        }
    }
    out
}

/// Shows the notification from a thread of its own, without blocking or
/// bringing the rest down.
///
/// `notify_rust` opens a synchronous D-Bus connection, which internally
/// starts a runtime: doing that from a tokio thread panics the process with
/// "Cannot start a runtime from within a runtime", and the whole client dies
/// at the first notification. A system thread has no tokio context, so it is
/// the only place it can be called from.
///
/// `spawn_blocking` is not used: those threads stay inside the runtime's
/// context, so the problem would come back identical.
pub fn show_detached(n: DesktopNotification) {
    std::thread::spawn(move || {
        if let Err(e) = show(&n) {
            tracing::warn!("notification not shown: {e}");
        }
    });
}

/// Whether notifications are on. They are turned off with `SLAQ_NO_NOTIFY`
/// (permanently) or from the settings panel (at runtime).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) && std::env::var_os("SLAQ_NO_NOTIFY").is_none()
}

/// Turns desktop notifications on or off at runtime.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The names stay alive for the whole duration of the test process.
    fn names() -> &'static std::collections::HashMap<String, String> {
        use std::sync::OnceLock;
        static N: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();
        N.get_or_init(|| {
            [("U_ME".to_string(), "Daniele".to_string())].into_iter().collect()
        })
    }

    fn ctx() -> Context<'static> {
        Context {
            my_user_id: "U_ME",
            channel_name: "#general",
            is_dm: false,
            author: "giulio",
            names: names(),
        }
    }

    #[test]
    fn a_mention_is_written_with_the_name_not_the_id() {
        // In a notification `@U_ME` says nothing to whoever reads it.
        let n = from_message(&msg("ciao <@U_ME> guarda qui"), &ctx()).unwrap();
        assert!(n.body().contains("@Daniele"), "{}", n.body());
        assert!(!n.body().contains("U_ME"), "{}", n.body());
    }

    #[test]
    fn an_unknown_id_is_left_as_it_is() {
        let n = from_message(&msg("ciao <@U_ME> e <@U_IGNOTO>"), &ctx()).unwrap();
        assert!(n.body().contains("@U_IGNOTO"), "{}", n.body());
    }

    fn msg(text: &str) -> serde_json::Value {
        json!({ "type": "message", "channel": "C1", "user": "U_ALTRO",
                "ts": "100.1", "text": text })
    }

    #[test]
    fn a_mention_notifies() {
        let n = from_message(&msg("ciao <@U_ME> ci sei?"), &ctx()).expect("notification");
        assert_eq!(n.heading(), "#general");
        assert!(n.body().starts_with("giulio: "), "{}", n.body());
    }

    #[test]
    fn a_plain_channel_message_does_not() {
        assert!(from_message(&msg("buongiorno a tutti"), &ctx()).is_none());
    }

    #[test]
    fn a_dm_always_notifies() {
        let c = Context { is_dm: true, channel_name: "giulio", ..ctx() };
        assert!(from_message(&msg("ci sei?"), &c).is_some());
    }

    #[test]
    fn broadcasts_notify() {
        for b in ["<!here>", "<!channel>", "<!everyone>"] {
            assert!(from_message(&msg(&format!("{b} riunione")), &ctx()).is_some(), "{b}");
        }
    }

    #[test]
    fn our_own_messages_never_notify() {
        let mut m = msg("<@U_ME> nota per me");
        m["user"] = json!("U_ME");
        assert!(from_message(&m, &ctx()).is_none());
    }

    #[test]
    fn suppress_notification_is_respected() {
        let mut m = msg("<@U_ME> guarda");
        m["suppress_notification"] = json!(true);
        assert!(from_message(&m, &ctx()).is_none());
    }

    #[test]
    fn joins_and_edits_do_not_notify() {
        for sub in ["channel_join", "message_changed", "message_deleted"] {
            let mut m = msg("<@U_ME>");
            m["subtype"] = json!(sub);
            assert!(from_message(&m, &ctx()).is_none(), "{sub}");
        }
    }

    #[test]
    fn bot_messages_in_a_dm_still_notify() {
        let mut m = msg("build failed");
        m["subtype"] = json!("bot_message");
        let c = Context { is_dm: true, ..ctx() };
        assert!(from_message(&m, &c).is_some());
    }

    #[test]
    fn the_markup_is_stripped_from_the_body() {
        let n = from_message(&msg("<@U_ME> guarda *questo* <https://x.test|link>"), &ctx()).unwrap();
        assert!(n.body().contains("guarda questo link"), "{}", n.body());
    }

    #[test]
    fn a_very_long_body_is_truncated() {
        let long = "a".repeat(500);
        let n = from_message(&msg(&format!("<@U_ME> {long}")), &ctx()).unwrap();
        assert!(n.body().chars().count() < 220, "{}", n.body().len());
    }

    #[test]
    fn non_message_events_are_ignored() {
        assert!(from_message(&json!({ "type": "user_typing" }), &ctx()).is_none());
    }
}
