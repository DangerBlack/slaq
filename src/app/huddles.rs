//! Incoming huddles: who is calling, and where a call is already running.
//!
//! Slack sends four events, all undocumented, observed by hooking the official
//! client's WebSocket (see `docs/huddle-protocol.md`):
//!
//! - `huddle_invite` — someone is calling us *directly*. It only arrives for
//!   two-party conversations, and it is the only one that should ring.
//! - `huddle_invite_cancel` — they hung up before we answered.
//! - `sh_room_update` / `sh_room_join` / `sh_room_leave` — the room changed.
//!   They all carry the same complete `room` object, so they are handled the
//!   same way: take the snapshot and replace the previous one.
//!
//! In a channel you are not invited: the huddle is simply there. That is the
//! difference between "it is ringing" and "they are talking in there".

use serde_json::Value;

/// What we need to know about a room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub id: String,
    /// Who is in it *now*, not who passed through.
    pub participants: Vec<String>,
    /// Whether the room is over and should be forgotten.
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A channel's room has changed.
    Room { channel: String, room: Room },
    /// Someone is calling us in this conversation.
    Invite { channel: String, caller: String },
    /// The invite was withdrawn.
    Cancel { channel: String },
}

/// Reads a WebSocket event, if it concerns huddles.
pub fn parse(raw: &Value) -> Option<Event> {
    let get = |k: &str| {
        raw.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    match raw.get("type").and_then(Value::as_str)? {
        "huddle_invite" => {
            let channel = get("channel_id");
            if channel.is_empty() {
                return None;
            }
            Some(Event::Invite {
                channel,
                caller: get("sender_user_id"),
            })
        }
        "huddle_invite_cancel" => {
            let channel = get("channel_id");
            if channel.is_empty() {
                return None;
            }
            Some(Event::Cancel { channel })
        }
        "sh_room_update" | "sh_room_join" | "sh_room_leave" => {
            let room = raw.get("room")?;
            // The channel is under `huddle`; for rooms that are not huddles
            // (the old Slack calls) it falls back to `room.channels`.
            let channel = raw
                .get("huddle")
                .and_then(|h| h.get("channel_id"))
                .and_then(Value::as_str)
                .or_else(|| {
                    room.get("channels")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                })?
                .to_string();

            let participants: Vec<String> = room
                .get("participants")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();

            // A room with nobody in it is not a call you can join: the first
            // `sh_room_update` of a freshly created huddle is still empty, and it is
            // the caller's `sh_room_join` that makes it real.
            let ended = room
                .get("has_ended")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || participants.is_empty();

            Some(Event::Room {
                channel,
                room: Room {
                    id: room
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    participants,
                    ended,
                },
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real shape, cut down to the fields that are read: captured from
    /// the official client's WebSocket during a real huddle.
    fn room_event(kind: &str, participants: Value) -> Value {
        json!({
            "type": kind,
            "event_ts": "1790760401.000200",
            "huddle": { "channel_id": "D0AAAAAAAAA" },
            "user": "U0CHIAMANTE",
            "room": {
                "id": "R0BBBBBBBBB",
                "call_family": "huddle",
                "channels": ["D0AAAAAAAAA"],
                "created_by": "U0CHIAMANTE",
                "has_ended": false,
                "is_dm_call": true,
                "participant_history": ["U0CHIAMANTE", "U0NOIALTRI"],
                "participants": participants,
            }
        })
    }

    #[test]
    fn an_invite_says_who_is_calling_and_where() {
        let e = parse(&json!({
            "type": "huddle_invite",
            "call_id": "R0BBBBBBBBB",
            "channel_id": "D0AAAAAAAAA",
            "sender_user_id": "U0CHIAMANTE",
            "team_id": "T0TEAM0000",
        }));
        assert_eq!(
            e,
            Some(Event::Invite {
                channel: "D0AAAAAAAAA".into(),
                caller: "U0CHIAMANTE".into()
            })
        );
    }

    #[test]
    fn a_cancelled_invite_is_recognised() {
        let e = parse(&json!({
            "type": "huddle_invite_cancel",
            "channel_id": "D0AAAAAAAAA",
            "team_id": "T0TEAM0000",
        }));
        assert_eq!(
            e,
            Some(Event::Cancel {
                channel: "D0AAAAAAAAA".into()
            })
        );
    }

    #[test]
    fn all_three_room_events_are_read_the_same_way() {
        for kind in ["sh_room_update", "sh_room_join", "sh_room_leave"] {
            let Some(Event::Room { channel, room }) =
                parse(&room_event(kind, json!(["U0CHIAMANTE"])))
            else {
                panic!("{kind} not recognised");
            };
            assert_eq!(channel, "D0AAAAAAAAA");
            assert_eq!(room.id, "R0BBBBBBBBB");
            assert_eq!(room.participants, ["U0CHIAMANTE"]);
            assert!(!room.ended, "{kind}");
        }
    }

    #[test]
    fn a_room_nobody_is_in_is_over() {
        // It happens twice: the room just created and still empty, and the one
        // that empties when the last person leaves. In neither case is there a call
        // to join.
        let Some(Event::Room { room, .. }) = parse(&room_event("sh_room_leave", json!([]))) else {
            panic!()
        };
        assert!(room.ended);
    }

    #[test]
    fn has_ended_wins_over_a_room_still_full() {
        let mut e = room_event("sh_room_update", json!(["U0CHIAMANTE"]));
        e["room"]["has_ended"] = json!(true);
        let Some(Event::Room { room, .. }) = parse(&e) else {
            panic!()
        };
        assert!(room.ended);
    }

    #[test]
    fn the_channel_falls_back_to_the_rooms_own_list() {
        // Calls that are not huddles have no `huddle` block.
        let mut e = room_event("sh_room_join", json!(["U0CHIAMANTE"]));
        e.as_object_mut().unwrap().remove("huddle");
        let Some(Event::Room { channel, .. }) = parse(&e) else {
            panic!()
        };
        assert_eq!(channel, "D0AAAAAAAAA");
    }

    #[test]
    fn everything_else_is_not_our_business() {
        for e in [
            json!({ "type": "message", "channel": "C1" }),
            json!({ "type": "user_typing" }),
            json!({ "type": "huddle_invite" }), // unusable without a channel
            json!({ "type": "sh_room_join" }),  // nor without a room
        ] {
            assert_eq!(parse(&e), None, "{e}");
        }
    }
}
