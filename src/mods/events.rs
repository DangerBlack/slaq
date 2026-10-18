//! Mod event subscriptions (`events`): the passive events a mod asks for.
//!
//! Slice 1 delivers `message.created` / `message.updated` through the host's
//! `on-message` dispatch, `channel.opened` / `channel.updated` through
//! `on-event`, and `reaction.added` / `reaction.removed` through
//! `on-reaction`. `background.tick` is not a Slack event: it is the host waking a
//! mod that holds `background` on its own worker thread, every `intervalSecs`,
//! through `on-event` (with an empty channel) — the mod's background lane. The
//! remaining vocabulary event (`message.deleted`) is known but
//! not implemented by this host yet, so a manifest listing it is refused at load
//! (fail closed), like an unimplemented capability.
//!
//! See `docs/mods/capabilities.md` ("Events").

use anyhow::{Result, bail};
use std::collections::BTreeSet;

use super::capability::Capability;

/// An event this host can deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    MessageCreated,
    MessageUpdated,
    ChannelOpened,
    ChannelUpdated,
    ReactionAdded,
    ReactionRemoved,
    BackgroundTick,
}

impl Event {
    fn from_name(name: &str) -> Option<Event> {
        Some(match name {
            "message.created" => Event::MessageCreated,
            "message.updated" => Event::MessageUpdated,
            "channel.opened" => Event::ChannelOpened,
            "channel.updated" => Event::ChannelUpdated,
            "reaction.added" => Event::ReactionAdded,
            "reaction.removed" => Event::ReactionRemoved,
            "background.tick" => Event::BackgroundTick,
            _ => return None,
        })
    }

    /// The event's normative name (passed to `on-event`).
    pub fn name(self) -> &'static str {
        match self {
            Event::MessageCreated => "message.created",
            Event::MessageUpdated => "message.updated",
            Event::ChannelOpened => "channel.opened",
            Event::ChannelUpdated => "channel.updated",
            Event::ReactionAdded => "reaction.added",
            Event::ReactionRemoved => "reaction.removed",
            Event::BackgroundTick => "background.tick",
        }
    }

    /// The capability the event requires (`docs/mods/capabilities.md`).
    pub fn capability(self) -> Capability {
        match self {
            Event::MessageCreated | Event::MessageUpdated => Capability::MessagesRead,
            Event::ChannelOpened | Event::ChannelUpdated => Capability::ChannelsList,
            Event::ReactionAdded | Event::ReactionRemoved => Capability::MessagesRead,
            Event::BackgroundTick => Capability::Background,
        }
    }
}

/// Vocabulary events that are known but not implemented by this host yet (a
/// manifest listing one is refused, fail closed).
const NOT_IMPLEMENTED: &[&str] = &["message.deleted"];

/// Reserved event names (a manifest listing one is refused).
const RESERVED: &[&str] = &["channel.member_joined"];

/// Cap on how many subscriptions a manifest may declare.
const MAX_EVENTS: usize = 16;

/// Parses and validates a manifest `events` value against the requested
/// capabilities: only implemented events survive, each with its capability
/// requested (and nothing else is silently accepted).
pub fn parse_events(v: &serde_json::Value, caps: &BTreeSet<Capability>) -> Result<Vec<Event>> {
    if v.is_null() {
        return Ok(Vec::new());
    }
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("`events` must be an array"))?;
    if arr.len() > MAX_EVENTS {
        bail!("events: too many (>{MAX_EVENTS})");
    }
    let mut out: Vec<Event> = Vec::new();
    for item in arr {
        let name = item
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("events must be strings"))?;
        if let Some(ev) = Event::from_name(name) {
            if !caps.contains(&ev.capability()) {
                bail!(
                    "event {name:?} requires the `{}` capability",
                    ev.capability().name()
                );
            }
            if out.contains(&ev) {
                bail!("duplicate event: {name}");
            }
            out.push(ev);
        } else if NOT_IMPLEMENTED.contains(&name) {
            bail!("event not implemented by this host: {name}");
        } else if RESERVED.contains(&name) {
            bail!("reserved event cannot be subscribed: {name}");
        } else {
            bail!("unknown event: {name}");
        }
    }
    Ok(out)
}
