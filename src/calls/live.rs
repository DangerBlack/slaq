//! A call in progress, drivable from an interface.
//!
//! The diagnostic probe in [`super`] prints and blocks; here the call lives in
//! a task and is driven from outside: you join, you mute, you hang up. The
//! state is read without blocking, because the graphics thread reads it on
//! every frame.

use crate::auth::Credentials;
use crate::calls::session::{Devices, MediaSession};
use crate::calls::signaling::{frame_type, slack_user_from_external_id, SignalingClient};
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// What you can ask of a call in progress.
enum Cmd {
    ToggleMute,
    ToggleScreenShare(Option<u32>),
    HangUp,
}

/// How the call is doing, for whoever draws it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallStatus {
    /// The channel it is open on.
    pub channel: String,
    /// Connected and working.
    pub connected: bool,
    pub muted: bool,
    /// How many seconds it has been running.
    pub seconds: u64,
    /// If it ended badly, why.
    pub error: Option<String>,
    /// The task has closed everything: the call can be forgotten.
    pub finished: bool,
    /// Somebody is sharing their screen.
    pub screen_on: bool,
    /// Who is sharing, when the id can be read: the SFU does not always put
    /// a user in the field it should be in.
    pub sharing: Option<String>,
    /// We are publishing our own screen.
    pub sharing_own: bool,
    /// Who is talking right now, as a Slack user id. It comes from the SFU's
    /// audio metadata (the loudest unmuted stream) and drives the highlight
    /// around the speaker's avatar.
    pub speaker: Option<String>,
    /// Our own microphone level (0..100): the SFU does not report our track,
    /// so it is measured locally to highlight ourselves while we talk.
    pub self_level: u32,
}

/// A live call.
///
/// The media runs on a thread of its own: cpal's audio streams are bound to the
/// thread that opened them and cannot be moved. Hence commands over a channel
/// instead of direct calls.
pub struct Call {
    status: Arc<RwLock<CallStatus>>,
    /// The last picture of the shared screen.
    ///
    /// A watch channel and not a queue: a frame that arrives while the window
    /// is drawing the previous one is of no interest, only the newest is. On
    /// a slow link it is the difference between falling behind and skipping.
    screen: tokio::sync::watch::Receiver<Option<Arc<crate::calls::screen::Picture>>>,
    cmds: tokio::sync::mpsc::UnboundedSender<Cmd>,
    /// Hang up: it is raised and the loop sees it on the next turn.
    ///
    /// It sits beside the channel on purpose. Whoever hangs up often lets go of the
    /// call straight afterwards, and with it the end of the channel: a queued
    /// command might never be read. A shared flag is not.
    hangup: Arc<std::sync::atomic::AtomicBool>,
}

impl Call {
    /// Joins the channel's call and stays in it until you hang up.
    ///
    /// It returns as soon as the room is created: the rest — signaling, media,
    /// audio — goes on in a task, and is followed through [`Call::status`].
    pub async fn join(
        creds: Credentials,
        channel_id: String,
        devices: Devices,
    ) -> Result<Self> {
        let api = crate::api::Client::new(creds.clone());
        let resp = api
            .rooms_join(&channel_id, "", false)
            .await
            .context("room creation failed")?;

        let call = resp.get("call").cloned().unwrap_or(serde_json::Value::Null);
        let fw = call
            .get("free_willy")
            .cloned()
            .context("this call does not use the Chime backend")?;

        let pick = |obj: Option<&serde_json::Value>, keys: &[&str]| -> Option<String> {
            let o = obj?;
            keys.iter()
                .find_map(|k| o.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let placement = fw.get("meeting").and_then(|m| m.get("MediaPlacement"));
        let signaling_url = pick(placement, &["SignalingUrl", "signalingUrl"])
            .context("SignalingUrl missing")?;
        let audio_host = pick(placement, &["AudioHostUrl", "audioHostUrl"])
            .context("AudioHostUrl missing")?;
        let attendee_id = pick(fw.get("attendee"), &["AttendeeId", "attendeeId"])
            .context("AttendeeId missing")?;
        let join_token = pick(fw.get("attendee"), &["JoinToken", "joinToken"])
            .context("JoinToken missing")?;

        let status = Arc::new(RwLock::new(CallStatus {
            channel: channel_id.clone(),
            ..Default::default()
        }));
        let (cmds, rx) = tokio::sync::mpsc::unbounded_channel();
        let (screen_tx, screen) = tokio::sync::watch::channel(None);
        let hangup = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let task_hangup = hangup.clone();
        let task_status = status.clone();
        std::thread::spawn(move || {
            // A runtime of its own, and multi-threaded: the media session lives
            // on this thread (the cpal streams are bound to it), but the tasks
            // it spawns — encode, receive, video decode — must run on separate
            // workers. On a single-threaded runtime a busy video share starved
            // the audio receive, which then stalled and broke.
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("call runtime not created: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let outcome = run(
                    &signaling_url,
                    &join_token,
                    &attendee_id,
                    &audio_host,
                    &devices,
                    &task_status,
                    rx,
                    &task_hangup,
                    screen_tx,
                )
                .await;
                let mut s = task_status.write().await;
                s.connected = false;
                s.finished = true;
                if let Err(e) = outcome {
                    warn!("call ended: {e:#}");
                    s.error = Some(format!("{e:#}"));
                }
            });
        });

        Ok(Call { status, cmds, hangup, screen })
    }

    /// The current state, without blocking whoever is drawing.
    pub fn status(&self) -> Option<CallStatus> {
        self.status.try_read().ok().map(|s| s.clone())
    }

    /// The last picture of the shared screen.
    ///
    /// The same `Arc` comes back until a new frame arrives, so whoever draws
    /// can tell by the pointer whether it is worth uploading a texture again:
    /// a screen at 15 fps against a window that redraws far more often would
    /// otherwise mean uploading the same picture to the GPU all day.
    pub fn screen(&self) -> Option<Arc<crate::calls::screen::Picture>> {
        self.screen.borrow().clone()
    }

    /// Toggles the microphone mute.
    pub fn toggle_mute(&self) {
        let _ = self.cmds.send(Cmd::ToggleMute);
    }

    /// Starts or stops publishing our own screen.
    pub fn toggle_screen_share(&self, display_id: Option<u32>) {
        let _ = self.cmds.send(Cmd::ToggleScreenShare(display_id));
    }

    /// Hangs up.
    pub fn hang_up(&self) {
        tracing::info!("hang-up requested");
        self.hangup
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.cmds.send(Cmd::HangUp);
    }
}

const SPEAKER_VOLUME_THRESHOLD: u32 = 10;

/// The call's life cycle: signaling, media, and the traffic that keeps
/// the session alive.
#[allow(clippy::too_many_arguments)]
async fn run(
    signaling_url: &str,
    join_token: &str,
    attendee_id: &str,
    audio_host: &str,
    devices: &Devices,
    status: &Arc<RwLock<CallStatus>>,
    mut cmds: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    hangup: &Arc<std::sync::atomic::AtomicBool>,
    screen_tx: tokio::sync::watch::Sender<Option<Arc<crate::calls::screen::Picture>>>,
) -> Result<()> {
    let mut sig = SignalingClient::connect(signaling_url, join_token)
        .await
        .context("signaling unreachable")?;
    let (ack, _early) = sig.join().await.context("JOIN rejected")?;

    let screen_reset = screen_tx.clone();
    let session = MediaSession::negotiate(
        &mut sig, &ack, attendee_id, audio_host, false, None, devices, Some(screen_tx),
    )
    .await
    .context("media negotiation failed")?;
    session
        .wait_connected(std::time::Duration::from_secs(20))
        .await
        .context("the media did not connect")?;

    info!("call connected");
    status.write().await.connected = true;

    let started = tokio::time::Instant::now();
    let mut ping_timer = tokio::time::interval(std::time::Duration::from_millis(
        crate::calls::signaling::PING_INTERVAL_MS,
    ));
    ping_timer.tick().await;
    // The server closes whoever stays silent: the official client sends
    // metrics every second, and without them the session dies in a few seconds.
    let mut metrics = tokio::time::interval(std::time::Duration::from_secs(1));
    metrics.tick().await;
    // Our own mic level, read often enough for the speaker highlight to follow.
    let mut level_timer = tokio::time::interval(std::time::Duration::from_millis(150));
    level_timer.tick().await;
    let mut ping_id: u32 = 0;
    // Who we currently believe is sharing: it keeps us from renegotiating on
    // every index, and those arrive several times a second.
    let mut watching: Option<u32> = None;
    let mut last_kbps: Option<u32> = None;
    // audio stream id -> Slack user id, from the SFU's `audioStreamIdInfo`:
    // the audio metadata only names streams, and this is what turns a stream
    // back into a person.
    let mut stream_users: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    // Our own publishing session, when one is running. It lives here, on the
    // call's dedicated thread, together with its encoder and second runtime
    // resources.
    #[cfg(feature = "vp8")]
    let mut content: Option<crate::calls::screen_send::ContentShare> = None;

    loop {
        if hangup.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            Some(cmd) = cmds.recv() => match cmd {
                Cmd::ToggleMute => {
                    let muted = session.toggle_mute().await;
                    status.write().await.muted = muted;
                }
                Cmd::ToggleScreenShare(display_id) => {
                    #[cfg(feature = "vp8")]
                    if let Some(c) = content.take() {
                        c.stop().await;
                        status.write().await.sharing_own = false;
                    } else {
                        match crate::calls::screen_send::ContentShare::start(
                            signaling_url,
                            join_token,
                            attendee_id,
                            audio_host,
                            display_id,
                        )
                        .await
                        {
                            Ok(c) => {
                                content = Some(c);
                                status.write().await.sharing_own = true;
                            }
                            Err(e) => {
                                warn!("screen sharing not started: {e:#}");
                                status.write().await.sharing_own = false;
                            }
                        }
                    }
                    #[cfg(not(feature = "vp8"))]
                    {
                        let _ = display_id;
                        warn!(
                            "screen sharing not available: build with --features vp8"
                        );
                    }
                }
                Cmd::HangUp => {
                    info!("hang-up: command received");
                    break;
                }
            },
            _ = metrics.tick() => {
                if hangup.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                status.write().await.seconds = started.elapsed().as_secs();
                let sample = crate::calls::signaling::MetricSample {
                    rtt_ms: 25.0,
                    mic_pps: 50.0,
                    mic_bitrate: 16_000.0,
                    spk_pps: 50.0,
                    spk_bitrate: 16_000.0,
                };
                sig.send_client_metrics(
                    crate::calls::signaling::client_metrics(sample),
                )
                .await
                .context("metrics not sent")?;
                if watching.is_some() {
                    if let Some((mid, attendee, group)) = session.screen_info() {
                        let kbps = session.screen_kbps();
                        if last_kbps != Some(kbps) {
                            last_kbps = Some(kbps);
                            if let Err(e) =
                                sig.watch_video(&mid, &attendee, group, kbps).await
                            {
                                warn!("video bitrate not updated: {e:#}");
                            }
                        }
                    }
                }
            }
            _ = level_timer.tick() => {
                let level = session.self_level();
                let mut st = status.write().await;
                if st.self_level != level {
                    st.self_level = level;
                }
            }
            _ = ping_timer.tick() => {
                ping_id += 1;
                sig.send_ping(ping_id).await.context("ping failed")?;
            }
            frame = sig.recv_frame() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        info!("signaling interrupted: {e:#}");
                        break;
                    }
                };
                match frame {
                    Some(f) if f.r#type == frame_type::LEAVE => break,
                    Some(f) if f.r#type == frame_type::INDEX => {
                        let found = f.index.as_ref().and_then(crate::calls::signaling::shared_screen);
                        // The index carries every source in the room, so it
                        // says both that a screen has appeared and that it is
                        // gone — the absence is the signal. The stream is what
                        // we compare on, not the person: the same screen can
                        // be announced without a readable user.
                        let stream = found.as_ref().map(|s| s.stream_id);
                        if stream != watching {
                            watching = stream;
                            last_kbps = None;
                            {
                                let mut st = status.write().await;
                                st.screen_on = found.is_some();
                                st.sharing =
                                    found.as_ref().and_then(|s| s.user_id()).map(str::to_string);
                            }
                            if found.is_none() {
                                info!("screen sharing ended");
                                session.stop_screen();
                                let _ = screen_reset.send(None);
                            }
                            if let Some(screen) = found {
                                info!(
                                    "screen sharing: attendee={} external={} stream={}",
                                    screen.attendee, screen.external_user_id, screen.stream_id
                                );
                                if let Err(e) = session
                                    .watch_screen(&mut sig, &screen, audio_host, attendee_id)
                                    .await
                                {
                                    warn!("shared screen not attached: {e:#}");
                                }
                            }
                        }
                    }
                    Some(f)
                        if f.r#type == frame_type::PAUSE
                            || f.r#type == frame_type::RESUME =>
                    {
                        let p = f.pause.as_ref();
                        warn!(
                            "signaling: {} from the server — groups={:?} streams={:?}",
                            if f.r#type == frame_type::PAUSE {
                                "PAUSE"
                            } else {
                                "RESUME"
                            },
                            p.map(|p| p.group_ids.clone()).unwrap_or_default(),
                            p.map(|p| p.stream_ids.clone()).unwrap_or_default(),
                        );
                    }
                    Some(f) if f.audio_stream_id_info.is_some() => {
                        if let Some(info) = &f.audio_stream_id_info {
                            for st in &info.streams {
                                let Some(id) = st.audio_stream_id else { continue };
                                if st.dropped.unwrap_or(false) {
                                    stream_users.remove(&id);
                                } else if let Some(u) = st
                                    .external_user_id
                                    .as_deref()
                                    .and_then(slack_user_from_external_id)
                                {
                                    // `external_user_id` is `<team>-<call>-<user>`:
                                    // stored whole, the speaker never matched a
                                    // participant and the green ring never lit.
                                    stream_users.insert(id, u.to_string());
                                }
                            }
                        }
                    }
                    Some(f) if f.audio_metadata.is_some() => {
                        // The loudest unmuted stream is whoever is talking.
                        let mut best: Option<(u32, String)> = None;
                        if let Some(md) = &f.audio_metadata {
                            for a in &md.attendee_states {
                                if a.muted.unwrap_or(false) {
                                    continue;
                                }
                                let vol = a.volume.unwrap_or(0);
                                if vol < SPEAKER_VOLUME_THRESHOLD {
                                    continue;
                                }
                                let Some(id) = a.audio_stream_id else { continue };
                                if let Some(u) = stream_users.get(&id) {
                                    if best.as_ref().map(|(v, _)| vol > *v).unwrap_or(true) {
                                        best = Some((vol, u.clone()));
                                    }
                                }
                            }
                        }
                        let speaker = best.map(|(_, u)| u);
                        let mut st = status.write().await;
                        if st.speaker != speaker {
                            st.speaker = speaker;
                        }
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }

    // Hanging up is two things: the `LEAVE` frame on the signaling and
    // closing the media. The room closes when the Chime attendee leaves, not with
    // a REST call.
    #[cfg(feature = "vp8")]
    if let Some(c) = content.take() {
        c.stop().await;
    }
    session.stop_screen();
    let _ = screen_reset.send(None);
    sig.leave().await.ok();
    session.close().await.ok();
    info!("call closed");
    Ok(())
}
