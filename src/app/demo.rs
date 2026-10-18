//! Fabricated application state for the GUI preview (`slaq gui --demo`).
//!
//! It exists so the interface can be built and reviewed against every screen —
//! channels, messages, markdown, reactions, attachments, action buttons,
//! threads, a running huddle, notifications — **without a workspace and without
//! a network**. Nothing here talks to Slack; the GUI renders these values
//! exactly as it renders real ones.
//!
//! Messages go through [`super::parse_message_as`], the same parser the real
//! client uses, so what the preview shows is what live data would produce.

use super::*;

/// The author id used for "my" messages, matched against `my_user_id`.
const ME: &str = "U_ME";

pub fn seed(s: &mut AppState) {
    s.authenticated = true;
    s.live = true;
    s.my_user_id = ME.to_string();
    s.team_id = "T_DEMO".to_string();
    s.status = "Anteprima · dati di esempio".to_string();

    seed_people(s);
    let engineering = seed_channels(s);
    seed_messages(s, engineering);
    seed_thread(s, engineering);
    seed_huddle(s, "C_DESIGN");
    seed_notifications(s);
    seed_pins(s);
    seed_saved(s);
    seed_threads(s);
    seed_mentions(s);
    seed_scheduled(s);
}

/// Two messages queued to be sent later (fixed times, so snapshots are stable).
fn seed_scheduled(s: &mut AppState) {
    s.scheduled = vec![
        crate::app::ScheduledMessage {
            id: "s1".to_string(),
            channel: "C_ENG".to_string(),
            channel_name: "#engineering".to_string(),
            text: "Ricordati di riguardare la PR #42.".to_string(),
            post_at: 1_763_000_000,
        },
        crate::app::ScheduledMessage {
            id: "s2".to_string(),
            channel: "C_DESIGN".to_string(),
            channel_name: "#design".to_string(),
            text: "Aggiornamento design: consegna slittata.".to_string(),
            post_at: 1_763_010_000,
        },
    ];
}

/// Two messages that mention the user.
fn seed_mentions(s: &mut AppState) {
    use crate::api::client::{SearchChannel, SearchHit};
    let hit = |ts: &str, text: &str, user: &str, thread: Option<&str>| SearchHit {
        ts: ts.into(),
        text: text.into(),
        username: None,
        user: Some(user.into()),
        permalink: String::new(),
        thread_ts: thread.map(str::to_string),
        channel: SearchChannel {
            id: "C_ENG".into(),
            name: "engineering".into(),
        },
    };
    s.mentions = std::sync::Arc::new(vec![
        hit(
            "1700000001.000100",
            "Ottimo, guardo adesso. <@U_ME> quando puoi dai un occhio al CSS `mrkdwn`?",
            "U_MARCO",
            None,
        ),
        hit(
            "1700000003.000300",
            "Ci sono! <@U_ME> ho già in mente due fix per il rendering.",
            "U_ANA",
            Some("1700000001.000100"),
        ),
    ]);
}

/// The "Threads" inbox: two threads the user is in.
fn seed_threads(s: &mut AppState) {
    let one = |v: serde_json::Value| parse_all(&serde_json::json!([v])).into_iter().next();
    s.threads = vec![
        crate::app::ThreadRow {
            channel: "C_ENG".to_string(),
            root: one(serde_json::json!({
                "user": "U_GIULIA",
                "ts": "1700000004.000400",
                "text": "Domanda veloce: il timeout lo teniamo a 30s?"
            }))
            .expect("root"),
            replies: parse_all(&serde_json::json!([
                { "user": "U_MARCO", "ts": "1700000004.000500", "text": "Direi che 30s va bene, è quello che usa anche il client ufficiale." },
                { "user": "U_ANA", "ts": "1700000004.000600", "text": "Ok anche per me, apro un ticket per allineare i valori." }
            ])),
            reply_count: 3,
        },
        crate::app::ThreadRow {
            channel: "C_DESIGN".to_string(),
            root: one(serde_json::json!({
                "user": "U_MARCO",
                "ts": "1700000007.000700",
                "text": "Chiuso e mergiato :white_check_mark:"
            }))
            .expect("root"),
            replies: parse_all(&serde_json::json!([
                { "user": "U_LUCA", "ts": "1700000007.000800", "text": "Grazie!" }
            ])),
            reply_count: 1,
        },
    ];
    s.threads_unread = 2;
}

/// A couple of pinned messages in #engineering.
fn seed_pins(s: &mut AppState) {
    let pins = serde_json::json!([
        {
            "user": "U_MARCO",
            "ts": "1700000002.000200",
            "text": "Ottimo, guardo adesso. <@U_ANA> quando puoi dai un occhio al CSS `mrkdwn`?"
        },
        {
            "user": "U_ANA",
            "ts": "1700000003.000300",
            "text": "Ci sono! Ho già in mente due fix per il rendering di <https://example.com/docs|questa pagina>."
        }
    ]);
    s.pins = parse_all(&pins);
}

/// Three messages saved for later.
fn seed_saved(s: &mut AppState) {
    let saved = serde_json::json!([
        {
            "user": "U_GIULIA",
            "ts": "1700000004.000400",
            "text": "Domanda veloce: il timeout lo teniamo a 30s?"
        },
        {
            "user": "U_BOT",
            "ts": "1700000005.000500",
            "text": "Deploy #128 riuscito su *production*."
        },
        {
            "user": "U_MARCO",
            "ts": "1700000011.000100",
            "text": "Blocco di codice:\n```\nfn main() {\n    println!(\"ciao\");\n}\n```"
        }
    ]);
    s.saved = parse_all(&saved)
        .into_iter()
        .map(|m| crate::app::SavedRow {
            channel: "C_ENG".to_string(),
            ts: m.ts.clone(),
            state: "in_progress".to_string(),
            message: m,
        })
        .collect();
    s.saved_counts = crate::api::client::SavedCounts {
        total_count: 3,
        uncompleted_count: 2,
        completed_count: 1,
        archived_count: 0,
    };
}

fn seed_people(s: &mut AppState) {
    let names: &[(&str, &str)] = &[
        (ME, "Daniele Baschieri"),
        ("U_ANA", "Ana Rossi"),
        ("U_MARCO", "Marco Bianchi"),
        ("U_GIULIA", "Giulia Conti"),
        ("U_LUCA", "Luca Ferrari"),
        ("U_BOT", "deploy-bot"),
    ];
    s.user_names = Arc::new(
        names
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect(),
    );
    // username -> display name, used to expand group-DM names.
    s.usernames = Arc::new(
        names
            .iter()
            .map(|(_id, name)| (name.to_lowercase().replace(' ', ""), name.to_string()))
            .chain(std::iter::once((
                "danielebaschieri".to_string(),
                "Daniele Baschieri".to_string(),
            )))
            .collect(),
    );

    // Online / away, for the sidebar dots.
    s.presence = Arc::new(
        [
            (ME, true),
            ("U_ANA", true),
            ("U_MARCO", false),
            ("U_GIULIA", true),
            ("U_LUCA", true),
            ("U_BOT", true),
        ]
        .iter()
        .map(|(id, on)| (id.to_string(), *on))
        .collect(),
    );

    // Chosen status emoji + text.
    s.user_status = Arc::new(
        [
            (
                "U_ANA",
                UserStatus {
                    emoji: ":palm_tree:".to_string(),
                    text: "In ferie fino a lunedì".to_string(),
                },
            ),
            (
                "U_GIULIA",
                UserStatus {
                    emoji: ":spiral_calendar_pad:".to_string(),
                    text: "In riunione".to_string(),
                },
            ),
        ]
        .iter()
        .map(|(id, st)| (id.to_string(), st.clone()))
        .collect(),
    );

    s.bot_users = Arc::new(["U_BOT".to_string()].into_iter().collect());
}

/// Adds the conversations and returns the index of `#engineering`.
fn seed_channels(s: &mut AppState) -> usize {
    // (id, display name, is_dm, peer)
    let rows: &[(&str, &str, bool, Option<&str>)] = &[
        ("C_ENG", "engineering", false, None),
        ("C_DESIGN", "design", false, None),
        ("C_GEN", "general", false, None),
        ("C_RANDOM", "random", false, None),
        ("D_ANA", "Ana Rossi", true, Some("U_ANA")),
        ("D_MARCO", "Marco Bianchi", true, Some("U_MARCO")),
        (
            "M_LEAD",
            "Ana Rossi, Marco Bianchi, Giulia Conti",
            true,
            Some("U_GIULIA"),
        ),
    ];
    for (id, name, is_dm, peer) in rows {
        ensure_channel(
            s,
            id.to_string(),
            name.to_string(),
            *is_dm,
            peer.map(str::to_string),
        );
    }

    // A couple of unread markers so the sidebar shows both states.
    {
        let ch = Arc::make_mut(&mut s.channels);
        if let Some(c) = ch.iter_mut().find(|c| c.id == "C_ENG") {
            c.unread = true;
            c.mentions = 2;
        }
        if let Some(c) = ch.iter_mut().find(|c| c.id == "C_DESIGN") {
            c.unread = true;
        }
        if let Some(c) = ch.iter_mut().find(|c| c.id == "D_ANA") {
            c.unread = true;
            c.mentions = 1;
        }
    }

    s.selected_channel = s.channels.iter().position(|c| c.id == "C_ENG").unwrap_or(0);
    s.selected_channel
}

fn seed_messages(s: &mut AppState, _engineering: usize) {
    let msgs = serde_json::json!([
        {
            "user": "U_MARCO",
            "ts": "1700000001.000100",
            "text": "Ciao! Ho aggiornato il *deploy* per usare il nuovo endpoint. Il diff è qui: <https://example.com/pr/42|PR #42>",
            "reactions": [{ "name": "thumbsup", "count": 3, "users": ["U_ANA", "U_GIULIA", "U_LUCA"] }]
        },
        {
            "user": ME,
            "ts": "1700000002.000200",
            "text": "Ottimo, guardo adesso. <@U_ANA> quando puoi dai un occhio al CSS `mrkdwn`?"
        },
        {
            "user": "U_ANA",
            "ts": "1700000003.000300",
            "text": "Ci sono! Ho già in mente due fix per il rendering di <https://example.com/docs|questa pagina>.",
            "reactions": [{ "name": "eyes", "count": 2, "users": ["U_MARCO", "U_LUCA"] }]
        },
        {
            "user": "U_GIULIA",
            "ts": "1700000004.000400",
            "text": "Domanda veloce: il timeout lo teniamo a 30s? ",
            "thread_ts": "1700000004.000400",
            "reply_count": 3
        },
        {
            "user": "U_BOT",
            "bot_id": "B_DEPLOY",
            "ts": "1700000005.000500",
            "text": "",
            "bot_profile": {
                "name": "deploy-bot",
                "app_id": "A_DEPLOY",
                "icons": { "image_48": "" }
            },
            "attachments": [{
                "color": "36a64f",
                "title": "Deploy #128 riuscito",
                "title_link": "https://example.com/deploys/128",
                "text": "Ambiente: *production* · Durata: 1m 12s · Commit: `a1b2c3d`"
            }],
            "blocks": [{
                "type": "actions",
                "block_id": "deploy_actions",
                "elements": [
                    { "type": "button", "action_id": "redeploy", "text": { "type": "plain_text", "text": "Rilancia" }, "value": "128", "style": "primary" },
                    { "type": "button", "action_id": "view_log", "text": { "type": "plain_text", "text": "Log" }, "url": "https://example.com/deploys/128/log" }
                ]
            }]
        },
        {
            "user": "U_LUCA",
            "ts": "1700000006.000600",
            "text": "Ho aggiunto lo screenshot dell'anteprima qui sotto.",
            "files": [{
                "name": "anteprima.png",
                "mimetype": "image/png",
                "thumb_360": "https://files.slack.com/thumb.png",
                "url_private": "https://files.slack.com/full.png",
                "url_private_download": "https://files.slack.com/full.png",
                "permalink": "https://example.slack.com/files/F1"
            }]
        },
        {
            "user": "U_MARCO",
            "ts": "1700000007.000700",
            "text": "Chiuso e mergiato :white_check_mark:",
            "reactions": [
                { "name": "white_check_mark", "count": 4, "users": ["U_ANA", "U_GIULIA", "U_LUCA", ME] },
                { "name": "tada", "count": 2, "users": ["U_ANA", "U_LUCA"] }
            ]
        },
        {
            "user": "U_GIULIA",
            "ts": "1700000010.000100",
            "text": "Identificatore lunghissimo senza spazi, per provare il wrapping: AAAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKKLLLLMMMMNNNNOOOOPPPPQQQQRRRRSSSSTTTTUUUUVVVVWWWWXXXXYYYYZZZZ0123456789AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH"
        },
        {
            "user": "U_MARCO",
            "ts": "1700000011.000100",
            "text": "Blocco di codice:\n```\nfn main() {\n    let messaggio = \"una riga di codice piuttosto lunga per vedere se il blocco scorre o va a capo\";\n    println!(\"{messaggio}\");\n}\n```\nE codice inline: `cargo build --release --locked`."
        }
    ]);

    s.messages = parse_all(&msgs);
    s.messages_channel = Some("C_ENG".to_string());
}

fn seed_thread(s: &mut AppState, _engineering: usize) {
    let parent = "1700000004.000400";
    let replies = serde_json::json!([
        {
            "user": "U_MARCO",
            "ts": "1700000004.000500",
            "text": "Direi che 30s va bene, è quello che usa anche il client ufficiale.",
            "thread_ts": parent
        },
        {
            "user": "U_ANA",
            "ts": "1700000004.000600",
            "text": "Ok anche per me, apro un ticket per allineare i valori.",
            "thread_ts": parent
        },
        {
            "user": ME,
            "ts": "1700000004.000700",
            "text": "Perfetto, allora procedo io.",
            "thread_ts": parent
        },
        {
            "user": "U_GIULIA",
            "ts": "1700000004.000800",
            "text": "Nel thread, per confronto:\n```\nSELECT id, body FROM messages\nWHERE channel = 'C_ENG' AND ts > '1700000000';\n```",
            "thread_ts": parent
        }
    ]);
    s.thread_parent_ts = Some(parent.to_string());
    s.thread_channel_id = Some("C_ENG".to_string());
    s.thread_messages = parse_all(&replies);
}

fn seed_huddle(s: &mut AppState, channel: &str) {
    s.huddles.insert(
        channel.to_string(),
        Huddle {
            room_id: "R_DEMO".to_string(),
            participants: vec!["U_ANA".to_string(), "U_MARCO".to_string()],
            caller: None,
            ringing: false,
        },
    );
}

fn seed_notifications(s: &mut AppState) {
    push_notification(
        s,
        NotifEntry {
            channel_id: "D_ANA".to_string(),
            channel_name: "Ana Rossi".to_string(),
            author: "Ana Rossi".to_string(),
            text: "Ci sei per una call veloce sul redesign?".to_string(),
            ts: "1700000009.000100".to_string(),
            is_dm: true,
            thread_ts: None,
            read: false,
        },
    );
    push_notification(
        s,
        NotifEntry {
            channel_id: "C_ENG".to_string(),
            channel_name: "#engineering".to_string(),
            author: "Marco Bianchi".to_string(),
            text: "Qualcuno può rivedere la PR #42?".to_string(),
            ts: "1700000008.000100".to_string(),
            is_dm: false,
            thread_ts: None,
            read: false,
        },
    );
}

/// An English showcase workspace for the README screenshot (the
/// `readme_screenshot` generator in `gui`): a fictional team in launch week,
/// with emoji, reactions, a thread, a running huddle and a Second Brain note.
/// [`seed`] stays the review fixture the snapshot tests compare against.
pub fn seed_showcase(s: &mut AppState) {
    // Replace what [`seed`] put in (it runs first, through `App::demo`).
    s.channels = Arc::new(Vec::new());
    s.huddles.clear();
    s.notifications.clear();
    s.pins.clear();
    s.saved.clear();
    s.saved_counts = Default::default();
    s.threads.clear();
    s.threads_unread = 0;
    s.mentions = Arc::new(Vec::new());
    s.scheduled.clear();
    s.authenticated = true;
    s.live = true;
    s.my_user_id = ME.to_string();
    s.team_id = "T_SHOW".to_string();
    s.status = "Northwind · launch week".to_string();

    let people: &[(&str, &str, bool)] = &[
        (ME, "Sam Rivera", true),
        ("U_MAYA", "Maya Chen", true),
        ("U_LEO", "Leo Park", true),
        ("U_PRIYA", "Priya Nair", false),
        ("U_TOM", "Tom Becker", true),
        ("U_CI", "ci-bot", true),
    ];
    s.user_names = Arc::new(people.iter().map(|(id, n, _)| (id.to_string(), n.to_string())).collect());
    s.usernames = Arc::new(
        people
            .iter()
            .map(|(_, n, _)| (n.to_lowercase().replace(' ', "."), n.to_string()))
            .collect(),
    );
    s.presence = Arc::new(people.iter().map(|(id, _, on)| (id.to_string(), *on)).collect());
    s.user_status = Arc::new(
        [
            ("U_MAYA", ":headphones:", "Deep focus"),
            ("U_PRIYA", ":palm_tree:", "Back on Monday"),
        ]
        .iter()
        .map(|(id, e, t)| {
            (
                id.to_string(),
                UserStatus {
                    emoji: e.to_string(),
                    text: t.to_string(),
                },
            )
        })
        .collect(),
    );
    s.bot_users = Arc::new(["U_CI".to_string()].into_iter().collect());

    let rows: &[(&str, &str, bool, Option<&str>)] = &[
        ("C_LAUNCH", "launch", false, None),
        ("C_ENG", "engineering", false, None),
        ("C_DESIGN", "design", false, None),
        ("C_GEN", "general", false, None),
        ("C_RANDOM", "random", false, None),
        ("D_MAYA", "Maya Chen", true, Some("U_MAYA")),
        ("D_LEO", "Leo Park", true, Some("U_LEO")),
        ("M_CREW", "Maya Chen, Leo Park, Tom Becker", true, Some("U_TOM")),
    ];
    for (id, name, is_dm, peer) in rows {
        ensure_channel(s, id.to_string(), name.to_string(), *is_dm, peer.map(str::to_string));
    }
    {
        let ch = Arc::make_mut(&mut s.channels);
        for c in ch.iter_mut() {
            match c.id.as_str() {
                "C_ENG" => {
                    c.unread = true;
                    c.mentions = 2;
                }
                "C_DESIGN" | "C_RANDOM" => c.unread = true,
                "D_LEO" => {
                    c.unread = true;
                    c.mentions = 1;
                }
                _ => {}
            }
        }
    }
    s.selected_channel = s.channels.iter().position(|c| c.id == "C_LAUNCH").unwrap_or(0);

    let question = "1792138800.000500";
    let msgs = serde_json::json!([
        {
            "user": "U_MAYA",
            "ts": "1792135200.000100",
            "text": "Morning team :sunny: Release candidate *2.0-rc.3* is live on staging. Here's <https://example.com/changelog|what's new>.",
            "reactions": [
                { "name": "rocket", "count": 5, "users": ["U_LEO", "U_TOM", "U_PRIYA", ME, "U_CI"] },
                { "name": "tada", "count": 3, "users": ["U_LEO", "U_TOM", ME] },
                { "name": "eyes", "count": 2, "users": ["U_PRIYA", "U_TOM"] }
            ]
        },
        {
            "user": "U_LEO",
            "ts": "1792135800.000200",
            "text": "Ran the full smoke suite on it: 142/142 green :white_check_mark: Cold start is down to *0.8 s* :zap:",
            "reactions": [
                { "name": "fire", "count": 4, "users": ["U_MAYA", "U_TOM", "U_PRIYA", ME] },
                { "name": "100", "count": 2, "users": ["U_MAYA", ME] }
            ]
        },
        {
            "user": "U_CI",
            "bot_id": "B_CI",
            "ts": "1792136400.000300",
            "text": "",
            "bot_profile": { "name": "ci-bot", "app_id": "A_CI", "icons": { "image_48": "" } },
            "attachments": [{
                "color": "36a64f",
                "title": "Build #2048 passed",
                "title_link": "https://example.com/builds/2048",
                "text": "Branch: `release/2.0` · Duration: 3m 41s · Coverage: *87%*"
            }],
            "blocks": [{
                "type": "actions",
                "block_id": "build_actions",
                "elements": [
                    { "type": "button", "action_id": "promote", "text": { "type": "plain_text", "text": "Promote to production" }, "value": "2048", "style": "primary" },
                    { "type": "button", "action_id": "logs", "text": { "type": "plain_text", "text": "View logs" }, "url": "https://example.com/builds/2048/logs" }
                ]
            }]
        },
        {
            "user": "U_PRIYA",
            "ts": "1792137600.000400",
            "text": "The new onboarding illustrations are in :art: Huge thanks to <@U_TOM> for the late-night polish :pray:",
            "reactions": [
                { "name": "heart", "count": 6, "users": ["U_MAYA", "U_LEO", "U_TOM", ME, "U_CI", "U_PRIYA"] },
                { "name": "sparkles", "count": 3, "users": ["U_MAYA", "U_LEO", ME] }
            ]
        },
        {
            "user": "U_TOM",
            "ts": question,
            "text": "Are we still on track to ship on Thursday? :thinking_face:",
            "thread_ts": question,
            "reply_count": 3
        },
        {
            "user": ME,
            "ts": "1792140000.000600",
            "text": "Code freeze tonight at 18:00 :lock: Ping me if anything still needs to land.",
            "reactions": [
                { "name": "+1", "count": 4, "users": ["U_MAYA", "U_LEO", "U_TOM", "U_PRIYA"] },
                { "name": "ok_hand", "count": 2, "users": ["U_LEO", "U_TOM"] }
            ]
        },
        {
            "user": "U_LEO",
            "ts": "1792140600.000700",
            "text": "Memory check before the freeze :mag:\n```\n$ ps -o rss=,comm= -C slaq\n113284 slaq\n```",
            "reactions": [
                { "name": "exploding_head", "count": 3, "users": ["U_MAYA", "U_TOM", ME] },
                { "name": "muscle", "count": 2, "users": ["U_PRIYA", "U_MAYA"] }
            ]
        }
    ]);
    s.messages = parse_all(&msgs);
    s.messages_channel = Some("C_LAUNCH".to_string());

    // The thread on Tom's question, open on the right.
    let replies = serde_json::json!([
        { "user": "U_MAYA", "ts": "1792139100.000510", "text": "Yes! Release notes are drafted :memo:", "thread_ts": question },
        { "user": "U_LEO", "ts": "1792139400.000520", "text": "QA signed off an hour ago :tada:", "thread_ts": question },
        { "user": ME, "ts": "1792139700.000530", "text": "Then Thursday it is :rocket:", "thread_ts": question }
    ]);
    s.thread_parent_ts = Some(question.to_string());
    s.thread_channel_id = Some("C_LAUNCH".to_string());
    s.thread_messages = parse_all(&replies);

    // A Second Brain note under the question, as the mod writes it.
    let note = "🧠 Yes: the release plan targets Thursday 10:00 [1], and the last blocker, \
                the upload retry, was fixed on Tuesday [2].\n\nSources:\n\
                [1] #launch · 12 Oct 2026 · Maya Chen\n\
                [2] code src/upload/retry.rs:88";
    s.decorations = Arc::new(
        [(
            "C_LAUNCH".to_string(),
            [(question.to_string(), vec![note.to_string()])].into_iter().collect(),
        )]
        .into_iter()
        .collect(),
    );

    s.huddles.insert(
        "C_DESIGN".to_string(),
        Huddle {
            room_id: "R_SHOW".to_string(),
            participants: vec!["U_PRIYA".to_string(), "U_TOM".to_string()],
            caller: None,
            ringing: false,
        },
    );
    push_notification(
        s,
        NotifEntry {
            channel_id: "D_LEO".to_string(),
            channel_name: "Leo Park".to_string(),
            author: "Leo Park".to_string(),
            text: "Got a minute to pair on the release checklist?".to_string(),
            ts: "1792140900.000100".to_string(),
            is_dm: true,
            thread_ts: None,
            read: false,
        },
    );
}

fn parse_all(msgs: &serde_json::Value) -> Vec<MessageEntry> {
    msgs.as_array()
        .map(|arr| arr.iter().filter_map(|m| parse_message_as(m, ME)).collect())
        .unwrap_or_default()
}
