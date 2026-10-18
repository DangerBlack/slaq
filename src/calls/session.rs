//! Media session towards the Chime SFU.
//!
//! Takes the `JOIN_ACK` (which carries the TURN credentials) and negotiates the
//! connection: SDP offer → `SUBSCRIBE` → `SUBSCRIBE_ACK` with the answer.
//!
//! Audio only. In a terminal video cannot be shown, and screen share is a
//! second transceiver that can be added later on the same session.

use crate::calls::audio::AudioManager;
use crate::calls::signaling::{chime, SignalingClient};
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{debug, info, warn};
use webrtc::api::media_engine::{
    MediaEngine, MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::interceptor::registry::Registry;

/// Which devices to use, by partial name. Empty = the defaults.
#[derive(Debug, Clone, Default)]
pub struct Devices {
    pub mic: Option<String>,
    pub speaker: Option<String>,
    /// No echo cancellation and no noise suppression.
    pub raw_audio: bool,
}

/// The speakers always receive 48 kHz mono: that is what Chime sends.
const SPEAKER_RATE: u32 = 48_000;
use webrtc::api::APIBuilder;
use webrtc::dtls::extension::extension_use_srtp::SrtpProtectionProfile;
use webrtc::rtp::packetizer::Depacketizer as _;
use webrtc::rtcp::payload_feedbacks::full_intra_request::{FirEntry, FullIntraRequest};
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;

pub struct MediaSession {
    pub pc: Arc<RTCPeerConnection>,
    audio: AudioManager,
    screen_stop: Arc<std::sync::atomic::AtomicBool>,
    screen_kbps: Arc<std::sync::atomic::AtomicU32>,
    screen_info: Arc<std::sync::Mutex<Option<(String, String, u32)>>>,
}

/// The video section of an SDP, reduced to what matters when it does not
/// work: the direction and the payload types.
pub(crate) fn video_section(sdp: &str) -> String {
    let mut out = Vec::new();
    for block in sdp.split("\r\nm=").filter(|b| b.starts_with("video")) {
        let dir = ["sendrecv", "recvonly", "sendonly", "inactive"]
            .into_iter()
            .find(|d| block.contains(&format!("a={d}")))
            .unwrap_or("?");
        let mid = block
            .lines()
            .find_map(|l| l.strip_prefix("a=mid:"))
            .unwrap_or("?");
        let ssrc = block
            .lines()
            .find_map(|l| l.strip_prefix("a=ssrc:"))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or("-");
        out.push(format!("[mid={mid} {dir} ssrc={ssrc}]"));
    }
    if out.is_empty() {
        return "no video section".to_string();
    }
    out.join(" ")
}

/// Plays a remote audio track on the speakers.
async fn play_audio(
    track: std::sync::Arc<webrtc::track::track_remote::TrackRemote>,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
) {
    let mut dec = match opus::Decoder::new(SPEAKER_RATE, opus::Channels::Mono) {
        Ok(d) => d,
        Err(e) => {
            warn!("Opus decoder not created: {e}");
            return;
        }
    };
    // 120 ms at 48 kHz: the most an Opus packet can contain.
    let mut pcm = vec![0i16; 5760];
    let mut drops: u64 = 0;
    while let Ok((pkt, _)) = track.read_rtp().await {
        // An empty payload is a hole: it is not skipped, it is concealed with PLC.
        let payload: &[u8] = &pkt.payload;
        let n = match dec.decode(payload, &mut pcm, false) {
            Ok(n) => n,
            Err(e) => {
                // With an empty packet Opus activates packet loss
                // concealment: the render stays continuous and AEC3 does not
                // see a jump in the reference timeline.
                warn!("Opus frame dropped, concealment: {e}");
                match dec.decode(&[], &mut pcm, false) {
                    Ok(n) => n,
                    Err(e2) => {
                        warn!("Opus concealment failed: {e2}");
                        continue;
                    }
                }
            }
        };
        // Never block the read: if the playback buffer is full, dropping one
        // frame is better than stopping `read_rtp`, which would let webrtc's
        // internal receive buffer grow until the audio breaks.
        match tx.try_send(pcm[..n].to_vec()) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                drops += 1;
                if drops == 1 || drops % 100 == 0 {
                    warn!("playback lagging behind: {drops} audio frames dropped");
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
    info!("remote audio track ended");
}

/// Puts the RTP packets of a video stream back into whole frames.
///
/// VP8 and AV1 do not share the framing, so the reassembler is chosen from the
/// codec: VP8 goes through its payload descriptor first, AV1 does not (it
/// carries an aggregation header and OBUs, which may be split across packets).
enum Reassembler {
    Vp8(crate::calls::screen::Frames),
    Av1(crate::calls::screen::av1_rtp::Av1Frames),
}

impl Reassembler {
    fn push(&mut self, seq: u16, marker: bool, payload: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Vp8(f) => f.push(seq, marker, payload),
            Self::Av1(f) => f.push(seq, marker, payload),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Vp8(f) => f.reset(),
            Self::Av1(f) => f.reset(),
        }
    }
}

/// Asks the sender for a keyframe.
///
/// A corrupted frame poisons the decoder until a keyframe arrives, and the
/// SFU never sends one on its own for a content share: a lost packet would
/// freeze the picture for good.
static FIR_SEQ: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

async fn request_keyframe(pc: &std::sync::Arc<RTCPeerConnection>, sender_ssrc: u32, media_ssrc: u32) {
    let pli = PictureLossIndication { sender_ssrc, media_ssrc };
    let fir = FullIntraRequest {
        sender_ssrc,
        media_ssrc,
        fir: vec![FirEntry {
            ssrc: media_ssrc,
            sequence_number: FIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }],
    };
    let pkts: Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>> =
        vec![Box::new(pli), Box::new(fir)];
    match pc.write_rtcp(&pkts).await {
        Ok(_) => info!("keyframe requested (PLI+FIR) for ssrc {media_ssrc}"),
        Err(e) => warn!("keyframe request not sent: {e}"),
    }
}

/// Decodes a remote video track — a shared screen — into pictures.
async fn show_screen(
    track: std::sync::Arc<webrtc::track::track_remote::TrackRemote>,
    tx: tokio::sync::watch::Sender<Option<std::sync::Arc<crate::calls::screen::Picture>>>,
    pc: std::sync::Arc<RTCPeerConnection>,
    local_ssrc: u32,
    screen_stop: Arc<std::sync::atomic::AtomicBool>,
    screen_kbps: Arc<std::sync::atomic::AtomicU32>,
) {
    // Which codec actually arrives decides which decoder and which
    // reassembler to use. Slack shares the screen in AV1.
    let codec = track.codec().capability.mime_type;
    info!("screen incoming, codec {codec}");

    let is_vp8 = codec == "video/VP8";
    let mut frames = if is_vp8 {
        Reassembler::Vp8(Default::default())
    } else {
        Reassembler::Av1(Default::default())
    };
    let decoder = std::sync::Arc::new(std::sync::Mutex::new(
        match crate::calls::screen::Decoder::new(&codec) {
            Ok(d) => d,
            Err(e) => {
                warn!("video decoder not available: {e:#}");
                return;
            }
        },
    ));
    let mut vp8 = webrtc::rtp::codecs::vp8::Vp8Packet::default();
    let mut av1_dump = if is_vp8 {
        None
    } else {
        std::env::var_os("SLAQ_DUMP_AV1").and_then(|p| std::fs::File::create(p).ok())
    };
    let mut rtp_dump = std::env::var_os("SLAQ_DUMP_RTP").and_then(|p| std::fs::File::create(p).ok());
    let media_ssrc = track.ssrc();
    let mut last_keyframe = std::time::Instant::now();
    let (mut packets, mut units, mut decoded) = (0u64, 0u64, 0u64);
    let mut refclock: Option<(std::time::Instant, u32)> = None;
    let mut stopped = false;
    let mut last_drift = std::time::Duration::ZERO;
    let mut last_seq: Option<u16> = None;
    let mut lost: u64 = 0;
    let mut window_packets: u64 = 0;
    let mut window = std::time::Instant::now();
    while let Ok((pkt, _)) = track.read_rtp().await {
        if pkt.payload.is_empty() {
            continue;
        }
        packets += 1;
        window_packets += 1;
        if let Some(prev) = last_seq {
            let d = pkt.header.sequence_number.wrapping_sub(prev) as u32;
            if d > 1 && d < 5000 {
                lost += (d - 1) as u64;
            }
        }
        last_seq = Some(pkt.header.sequence_number);
        if let Some(f) = rtp_dump.as_mut() {
            use std::io::Write as _;
            let _ = f.write_all(&pkt.header.sequence_number.to_be_bytes());
            let _ = f.write_all(&[pkt.header.marker as u8]);
            let _ = f.write_all(&(pkt.payload.len() as u16).to_be_bytes());
            let _ = f.write_all(&pkt.payload);
        }
        if window.elapsed() >= std::time::Duration::from_secs(2) {
            if packets > 200 {
                let total = window_packets + lost;
                let ratio = if total == 0 { 0.0 } else { lost as f64 / total as f64 };
                let cur = screen_kbps.load(std::sync::atomic::Ordering::Relaxed);
                if ratio > 0.10 {
                    let next = ((cur as f64 * 0.8) as u32).max(400);
                    if next != cur {
                        screen_kbps.store(next, std::sync::atomic::Ordering::Relaxed);
                        info!(
                            "adaptive video: {cur} -> {next} kbps (loss {:.1}%)",
                            ratio * 100.0
                        );
                    }
                }
            }
            lost = 0;
            window_packets = 0;
            window = std::time::Instant::now();
        }
        let paused = screen_stop.load(std::sync::atomic::Ordering::Relaxed);
        if paused != stopped {
            stopped = paused;
            frames.reset();
            if !paused {
                last_keyframe = std::time::Instant::now();
                request_keyframe(&pc, local_ssrc, media_ssrc).await;
            }
        }
        if paused {
            continue;
        }
        // How far the stream is behind wall clock, measured against the first
        // packet. It is only reported: with retransmission in place the delay
        // is the path's, and dropping packets to catch up would break the
        // decoder instead of helping.
        let now = std::time::Instant::now();
        let (base_now, base_ts) = *refclock.get_or_insert((now, pkt.header.timestamp));
        let media_later = std::time::Duration::from_micros(
            (pkt.header.timestamp.wrapping_sub(base_ts)) as u64 * 1_000_000 / 90_000,
        );
        last_drift = now.saturating_duration_since(base_now + media_later);
        // On VP8 the payload descriptor is not part of the frame: handing it
        // to the decoder would invalidate every frame. On AV1 the payload is
        // the OBU stream itself, and the aggregation header is handled inside
        // the reassembler.
        let frame = if is_vp8 {
            match vp8.depacketize(&pkt.payload) {
                Ok(p) => frames.push(pkt.header.sequence_number, pkt.header.marker, &p),
                Err(e) => {
                    warn!("VP8 packet dropped: {e}");
                    continue;
                }
            }
        } else {
            frames.push(pkt.header.sequence_number, pkt.header.marker, &pkt.payload)
        };
        let Some(frame) = frame else {
            continue;
        };
        units += 1;
        if units % 50 == 0 {
            info!(
                "screen: {packets} packets, {units} units, {decoded} frames, delay {:?}",
                last_drift
            );
        }
        if let Some(f) = av1_dump.as_mut() {
            use std::io::Write as _;
            let _ = f.write_all(&(frame.len() as u32).to_le_bytes());
            let _ = f.write_all(&frame);
        }
        let decoder = std::sync::Arc::clone(&decoder);
        let outcome = tokio::task::spawn_blocking(move || {
            decoder.lock().unwrap().decode(&frame)
        })
        .await;
        match outcome {
            Ok(Ok(Some(pic))) => {
                if screen_stop.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                decoded += 1;
                if decoded == 1 {
                    info!("first screen frame: {}x{}", pic.width, pic.height);
                }
                if tx.send(Some(std::sync::Arc::new(pic))).is_err() {
                    break;
                }
            }
            Ok(Ok(None)) => debug!("frame without an image"),
            Ok(Err(e)) => {
                warn!("video frame dropped: {e:#}");
                if last_keyframe.elapsed() >= std::time::Duration::from_secs(1) {
                    last_keyframe = std::time::Instant::now();
                    request_keyframe(&pc, local_ssrc, media_ssrc).await;
                }
            }
            Err(e) => warn!("decoding task failed: {e}"),
        }
    }
    info!("video track ended: {packets} packets, {decoded} frames");
    let _ = tx.send(None);
}

impl MediaSession {
    /// Toggles the microphone mute and returns the new state.
    ///
    /// Muting means sending silence, not stopping transmitting: the SFU has to
    /// keep receiving packets on the stream it allocated us, or it closes the
    /// session.
    pub async fn toggle_mute(&self) -> bool {
        self.audio.toggle_mute().await
    }

    pub async fn is_muted(&self) -> bool {
        self.audio.is_muted().await
    }

    pub fn self_level(&self) -> u32 {
        self.audio.self_level()
    }

    pub fn stop_screen(&self) {
        self.screen_stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn screen_kbps(&self) -> u32 {
        self.screen_kbps.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn screen_info(&self) -> Option<(String, String, u32)> {
        self.screen_info.lock().unwrap().clone()
    }

    /// Negotiates two-way audio with the SFU.
    ///
    /// `attendee_id` and `audio_host` come from the `rooms.join` response; `ack` is
    /// the `JOIN_ACK` just received on the signaling.
    pub async fn negotiate(
        sig: &mut SignalingClient,
        ack: &chime::SdkJoinAckFrame,
        attendee_id: &str,
        audio_host: &str,
        muted: bool,
        dump_sdp: Option<&std::path::Path>,
        devices: &Devices,
        screen_tx: Option<tokio::sync::watch::Sender<Option<std::sync::Arc<crate::calls::screen::Picture>>>>,
    ) -> Result<Self> {
        let pc = build_peer_connection(ack.turn_credentials.as_ref()).await?;

        let mut audio = AudioManager::with_processing(!devices.raw_audio);
        let track = audio.init(devices.mic.as_deref()).context("opening the microphone failed")?;
        let sender = pc.add_track(track).await.context("add_track failed")?;
        info!("audio track attached");

        // What webrtc-rs actually puts on the wire. The SDP declares an SSRC
        // and a payload type, but what produces the packets is the sender: if the
        // two do not match the SFU drops them and we stay mute without noticing.
        let local_ssrc = {
            let p = sender.get_parameters().await;
            for enc in &p.encodings {
                info!("sender: ssrc={}", enc.ssrc);
            }
            for c in &p.rtp_parameters.codecs {
                info!(
                    "sender: payload_type={} mime={} clock={}",
                    c.payload_type, c.capability.mime_type, c.capability.clock_rate
                );
            }
            p.encodings.first().map(|e| e.ssrc).unwrap_or(0)
        };

        // The others' audio: the output device is opened and a reader is
        // attached to every track the SFU sends us. Without this the session is
        // mute on receive — technically a call, but a deaf one.
        let speakers = match audio.open_playback(devices.speaker.as_deref()) {
            Ok(tx) => Some(tx),
            Err(e) => {
                // Without speakers you can still talk: that is no reason to fail the
                // call.
                warn!("playback not available: {e:#}");
                None
            }
        };
        // One handler for every track the SFU sends: webrtc-rs keeps a
        // single `on_track`, so registering a second one silently replaces
        // the first. Having two meant the remote audio stopped being played
        // the moment the screen was wired up, and nothing said so.
        let screen_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let screen_stop_for_track = Arc::clone(&screen_stop);
        let screen_kbps = Arc::new(std::sync::atomic::AtomicU32::new(
            crate::calls::signaling::default_video_kbps(),
        ));
        let screen_kbps_for_track = Arc::clone(&screen_kbps);
        let screen_info: Arc<std::sync::Mutex<Option<(String, String, u32)>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pc_for_track = Arc::clone(&pc);
        pc.on_track(Box::new(move |track, _recv, _tr| {
            let speakers = speakers.clone();
            let screen = screen_tx.clone();
            let pc = Arc::clone(&pc_for_track);
            let local_ssrc = local_ssrc;
            let screen_stop = Arc::clone(&screen_stop_for_track);
            let screen_kbps = Arc::clone(&screen_kbps_for_track);
            Box::pin(async move {
                match track.kind() {
                    RTPCodecType::Audio => {
                        let Some(tx) = speakers else { return };
                        info!("remote audio track: ssrc={}", track.ssrc());
                        tokio::spawn(play_audio(track, tx));
                    }
                    RTPCodecType::Video => {
                        let Some(tx) = screen else { return };
                        info!("remote video track: ssrc={}", track.ssrc());
                        tokio::spawn(show_screen(track, tx, pc, local_ssrc, screen_stop, screen_kbps));
                    }
                    _ => {}
                }
            })
        }));

        // No data channel: the SFU's answer contains only
        // `a=group:BUNDLE 0` and omits the `m=application` section entirely.
        // Offering it leaves the session with an unanswered m-line.

        // The official client always offers an `m=video` even with the webcam
        // off, and marks it `a=inactive`. Ours was `recvonly`, and that is not
        // cosmetic: the SFU binds a subscribed stream to the video sections in
        // order and uses the direction to tell a receive slot from the others
        // — the browser keeps the camera section in `{inactive, sendrecv}`,
        // never `recvonly`. With two receiving sections the screen landed on
        // this first one (mid=1), not on the content section we had asked for
        // (mid=2), and not one packet ever arrived.
        //
        // webrtc-rs refuses `Inactive` in `add_transceiver_from_kind`, so the
        // section is built as `recvonly` and switched right after.
        let camera = pc
            .add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: vec![],
                }),
            )
            .await
            .context("adding the inactive video transceiver failed")?;
        camera
            .set_direction(RTCRtpTransceiverDirection::Inactive)
            .await;

        let offer = pc.create_offer(None).await.context("create_offer failed")?;
        pc.set_local_description(offer)
            .await
            .context("set_local_description failed")?;

        // Chime wants an offer complete with candidates: no trickle ICE on
        // this path, so we wait for gathering to finish.
        let mut gathering = pc.gathering_complete_promise().await;
        let _ = gathering.recv().await;

        let local = pc
            .local_description()
            .await
            .context("no local description after ICE gathering")?;

        if let Some(path) = dump_sdp {
            std::fs::write(path, &local.sdp)
                .with_context(|| format!("writing {} failed", path.display()))?;
            info!("SDP offer saved to {}", path.display());
        }

        let ack = sig
            .subscribe(&local.sdp, audio_host, attendee_id, muted, &[], Vec::new())
            .await?;

        if let Some(path) = dump_sdp {
            let answer_path = path.with_extension("answer.sdp");
            if let Some(a) = &ack.sdp_answer {
                std::fs::write(&answer_path, a).ok();
                info!("SDP answer saved to {}", answer_path.display());
            }
        }

        info!(
            "SUBSCRIBE_ACK: duplex={:?} allocations={} tracks={}",
            ack.duplex,
            ack.allocations.len(),
            ack.tracks.len()
        );
        for a in &ack.allocations {
            info!("  allocation: {a:?}");
        }
        for t in &ack.tracks {
            info!("  track: {t:?}");
        }

        let answer_sdp = ack
            .sdp_answer
            .clone()
            .context("SUBSCRIBE_ACK without sdp_answer (compressed?)")?;

        let answer = RTCSessionDescription::answer(answer_sdp)
            .context("invalid sdp_answer")?;
        pc.set_remote_description(answer)
            .await
            .context("set_remote_description failed")?;

        info!("negotiation complete");
        Ok(MediaSession { pc, audio, screen_stop, screen_kbps, screen_info })
    }

    /// Asks the SFU for a screen somebody is sharing.
    ///
    /// Two things: a new `SUBSCRIBE` that adds the stream to those we want to
    /// receive — which is what makes it start arriving — and a
    /// `REMOTE_VIDEO_UPDATE` that binds it to the video section of our SDP.
    /// The offer is remade from scratch because the session has to be
    /// renegotiated: the video m-line goes from inactive to receiving.
    pub async fn watch_screen(
        &self,
        sig: &mut SignalingClient,
        screen: &crate::calls::signaling::SharedScreen,
        audio_host: &str,
        attendee_id: &str,
    ) -> Result<String> {
        info!(
            "screen shared by {}: stream {}",
            screen.user_id().unwrap_or("?"),
            screen.stream_id
        );
        self.screen_stop
            .store(false, std::sync::atomic::Ordering::Relaxed);

        // A section of its own for the screen.
        //
        // Everything else already matched the official client frame by frame
        // — same duplex, same receiveStreamIds, the video subscription in the
        // SUBSCRIBE, the answer coming back `sendonly` with the SSRC — and
        // still not one packet arrived. What was left different is the shape
        // of the offer: the official client keeps its first video section for
        // the camera, inactive while it is off, and opens a *second* one for
        // the content. We were asking for the screen on the first, and Chime
        // appears not to put it there.
        //
        // So the second section is added the first time a screen turns up,
        // which is also when the browser adds it.
        let video: Vec<_> = self
            .pc
            .get_transceivers()
            .await
            .into_iter()
            .filter(|t| t.kind() == RTPCodecType::Video)
            .collect();
        for t in &video {
            info!(
                "video section mid={} direction={:?}",
                t.mid().map(|m| m.to_string()).unwrap_or_default(),
                t.direction()
            );
        }
        let content = match video.len() {
            0 | 1 => {
                self.pc
                    .add_transceiver_from_kind(
                        RTPCodecType::Video,
                        Some(RTCRtpTransceiverInit {
                            direction: RTCRtpTransceiverDirection::Recvonly,
                            send_encodings: vec![],
                        }),
                    )
                    .await
                    .context("adding the video section for the screen failed")?
            }
            _ => video[1].clone(),
        };
        content.set_direction(RTCRtpTransceiverDirection::Recvonly).await;

        let offer = self.pc.create_offer(None).await.context("create_offer failed")?;
        self.pc
            .set_local_description(offer)
            .await
            .context("set_local_description failed")?;
        let local = self
            .pc
            .local_description()
            .await
            .context("no local description")?;

        // The mid only exists once the offer has been applied.
        let mid = content.mid().map(|m| m.to_string()).unwrap_or_else(|| "2".into());
        info!("screen expected on mid={mid}");

        // The section just opened for the screen (mid 2) is the one we name,
        // and the only receiving video section: the camera one is `inactive`.
        let kbps = crate::calls::signaling::default_video_kbps();
        self.screen_kbps
            .store(kbps, std::sync::atomic::Ordering::Relaxed);
        *self.screen_info.lock().unwrap() =
            Some((mid.clone(), screen.attendee.clone(), screen.group_id));
        let want = crate::calls::signaling::video_subscription(
            &mid,
            &screen.attendee,
            screen.group_id,
            kbps,
        );
        let ack = sig
            .subscribe(
                &local.sdp,
                audio_host,
                attendee_id,
                false,
                &[screen.stream_id],
                vec![want],
            )
            .await?;
        // The track mapping is the proof that the SFU is going to send it: no
        // mapping, no stream, and the silence that follows looks like a bug
        // further down.
        info!(
            "SUBSCRIBE_ACK screen: allocations={} tracks={:?}",
            ack.allocations.len(),
            ack.tracks
        );
        info!("video offer: {}", video_section(&local.sdp));
        if let Some(a) = &ack.sdp_answer {
            info!("video answer: {}", video_section(a));
        }
        let answer_sdp = ack
            .sdp_answer
            .clone()
            .context("SUBSCRIBE_ACK without sdp_answer")?;
        self.pc
            .set_remote_description(
                RTCSessionDescription::answer(answer_sdp).context("invalid sdp_answer")?,
            )
            .await
            .context("set_remote_description failed")?;

        sig.watch_video(&mid, &screen.attendee, screen.group_id, kbps)
            .await?;
        Ok(mid)
    }

    /// Waits for the connection to reach `Connected`, or to fail.
    pub async fn wait_connected(&self, timeout: std::time::Duration) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        self.pc.on_peer_connection_state_change(Box::new(move |s| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(s).await;
            })
        }));

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(state) = rx.recv() => {
                    info!("PeerConnection state: {state}");
                    match state {
                        RTCPeerConnectionState::Connected => return Ok(()),
                        RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed => {
                            anyhow::bail!("connection failed: {state}")
                        }
                        _ => {}
                    }
                }
                _ = &mut deadline => {
                    anyhow::bail!("timeout waiting for the connection")
                }
            }
        }
    }

    pub async fn close(&self) -> Result<()> {
        self.pc.close().await.context("closing PeerConnection")?;
        Ok(())
    }
}

/// Builds the PeerConnection with the ICE servers from the `JOIN_ACK`.
///
/// The TURNs come from the signaling, not from `TurnControlUrl` nor from public
/// STUN: without these credentials the connection to the SFU is never
/// established.
pub(crate) async fn build_peer_connection(
    turn: Option<&chime::SdkTurnCredentials>,
) -> Result<Arc<RTCPeerConnection>> {
    let mut media_engine = MediaEngine::default();

    // Opus registered by hand instead of `register_default_codecs`, so we
    // can put in the `transport-cc` feedback the official client declares.
    media_engine
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![RTCPFeedback {
                        typ: "transport-cc".to_owned(),
                        parameter: String::new(),
                    }],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .context("registering Opus failed")?;

    // Every video codec we can receive, in the order the official client
    // offers them, with its payload types.
    //
    // Declaring one was enough to produce an `m=video` of the right shape,
    // and for a while that was all the section was for. It is not enough to
    // *receive*: whoever shares encodes once for everybody and the SFU
    // forwards without transcoding, so a codec we have not declared is a
    // stream that never reaches us. That is exactly what happened on the
    // first real attempt — the renegotiation succeeded, the stream was
    // subscribed, and no track ever arrived, because Slack shares the screen
    // in AV1 (`[HUDDLE-VIDEO-CODECS] Setting preferred content share video
    // codec to AV1`) while we were asking for VP8 alone.
    //
    // Different clients and platforms may well settle on different codecs,
    // so all four are declared and the decoder is chosen from what actually
    // arrives.
    for (mime, payload, fmtp) in [
        (MIME_TYPE_AV1, 45u8, ""),
        (MIME_TYPE_H264, 108, "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"),
        (MIME_TYPE_VP8, 96, ""),
        (MIME_TYPE_VP9, 98, "profile-id=0"),
    ] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback: vec![
                            RTCPFeedback { typ: "goog-remb".to_owned(), parameter: String::new() },
                            RTCPFeedback { typ: "transport-cc".to_owned(), parameter: String::new() },
                            RTCPFeedback { typ: "ccm".to_owned(), parameter: "fir".to_owned() },
                            RTCPFeedback { typ: "nack".to_owned(), parameter: String::new() },
                            RTCPFeedback { typ: "nack".to_owned(), parameter: "pli".to_owned() },
                        ],
                    },
                    payload_type: payload,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .with_context(|| format!("registering {mime} failed"))?;
    }

    // Retransmission (RFC 4588), one stream per video codec, with the payload
    // types the official client uses. Without it the SFU has no way to resend
    // a lost packet and the only recovery is a whole keyframe: that is the lag
    // we kept seeing on a lossy link.
    for (primary, rtx_pt) in [(45u8, 46u8), (108, 109), (96, 97), (98, 99)] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/rtx".to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: format!("apt={primary}"),
                        rtcp_feedback: vec![],
                    },
                    payload_type: rtx_pt,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .with_context(|| format!("registering rtx for {primary} failed"))?;
    }

    // The RTP header extensions the official client sends.
    //
    // `sdes:mid` is the one that really counts, and it has to be on the video
    // section too: with BUNDLE everything shares one transport, and an SFU
    // starts sending a stream whose SSRC was never in the SDP. The only way
    // to know which m-line that RTP belongs to is the mid it carries. Having
    // registered the extensions for audio alone, the screen arrived and
    // webrtc-rs had nowhere to put it — «Could not determine PayloadType for
    // SSRC 2386880171» — so no track was ever surfaced and the window stayed
    // empty while the packets were coming in.
    //
    // `ssrc-audio-level` is audio only, by definition. `abs-send-time` and
    // `transport-wide-cc` serve the bandwidth estimation on both.
    for (uri, kind) in [
        ("urn:ietf:params:rtp-hdrext:sdes:mid", RTPCodecType::Audio),
        ("urn:ietf:params:rtp-hdrext:sdes:mid", RTPCodecType::Video),
        ("urn:ietf:params:rtp-hdrext:ssrc-audio-level", RTPCodecType::Audio),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            RTPCodecType::Audio,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            RTPCodecType::Video,
        ),
        (
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            RTPCodecType::Audio,
        ),
        (
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            RTPCodecType::Video,
        ),
        // The dependency descriptor, which is how AV1 is forwarded.
        //
        // An SFU does not re-encode: to decide which packets of a layered
        // stream to pass on it reads this extension. Without it declared
        // there is nothing it can safely forward, and the symptom is the one
        // we had — the stream allocated, the answer `sendonly`, the SSRC
        // mapped, and not one packet. It is the difference that was left
        // between the official client's content section and ours.
        (
            "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension",
            RTPCodecType::Video,
        ),
        // The rest of what it declares on that section: the simulcast layer
        // ids, how the layers are allocated, the transmission offset and the
        // colour space.
        (
            "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id",
            RTPCodecType::Video,
        ),
        (
            "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id",
            RTPCodecType::Video,
        ),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00",
            RTPCodecType::Video,
        ),
        ("urn:ietf:params:rtp-hdrext:toffset", RTPCodecType::Video),
        (
            "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            RTPCodecType::Video,
        ),
    ] {
        media_engine
            .register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: uri.to_owned() },
                kind,
                None,
            )
            .with_context(|| format!("registering extension {uri} failed"))?;
    }

    let ice_servers = match turn {
        Some(t) if !t.uris.is_empty() => {
            let username = t.username.clone().unwrap_or_default();
            let credential = t.password.clone().unwrap_or_default();
            if let Some(ttl) = t.ttl {
                info!("TURN credentials valid for {ttl}s");
            }

            // webrtc-rs cannot handle TURN over TLS/TCP: on a `turns:` URI all
            // it does is spend ~5 s timing out during ICE gathering before
            // discarding it. We strip it beforehand.
            let (usable, skipped): (Vec<_>, Vec<_>) = t
                .uris
                .iter()
                .cloned()
                .partition(|u| u.starts_with("turn:") && !u.contains("transport=tcp"));

            for u in &skipped {
                info!("TURN URI ignored (not supported by webrtc-rs): {u}");
            }
            if usable.is_empty() {
                warn!("no usable TURN URI among {:?}", t.uris);
            }

            vec![RTCIceServer {
                urls: usable,
                username,
                credential,
                ..Default::default()
            }]
        }
        _ => {
            warn!("no TURN credentials in the JOIN_ACK: trying without relay");
            Vec::new()
        }
    };

    // Chime happily negotiates SRTP_AEAD_AES_256_GCM, but webrtc-srtp
    // 0.14.0 has a bug on that profile: `ProtectionProfile::AeadAes256Gcm`
    // declares `key_len() == 32` while `CipherAeadAesGcm` instantiates a fixed
    // `Aes128Gcm`, so the key derived from 32 bytes ends up in a cipher that
    // wants 16 and the process panics inside `generic-array` during the DTLS
    // handshake.
    //
    // Until it is fixed upstream we leave it out of the offered profiles: the
    // server falls back to AES-128, which works.
    let mut setting_engine = SettingEngine::default();
    // Only `AES128_CM_SHA1_80`, which is what the official client puts at
    // the head of its list (captured on 2026-09-30). We were offering GCM
    // first: if the SFU picks that and our encryption is off by the smallest
    // amount, the packets leave but nobody manages to read them — which is
    // exactly the symptom observed (558 packets out, zero answers, no volume
    // reported for our stream).
    setting_engine.set_srtp_protection_profiles(vec![
        SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
    ]);

    let mut registry = Registry::new();
    if std::env::var_os("SLAQ_NO_NACK").is_some() {
        registry = webrtc::api::interceptor_registry::configure_rtcp_reports(registry);
        registry =
            webrtc::api::interceptor_registry::configure_twcc_receiver_only(
                registry, &mut media_engine,
            )
            .context("TWCC interceptor not registered")?;
    } else {
        registry = register_default_interceptors(registry, &mut media_engine)
            .context("default interceptors not registered")?;
    }

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting_engine)
        .build();
    let pc = api
        .new_peer_connection(RTCConfiguration {
            ice_servers,
            // Relay only, like the official client: its offer carries a
            // single `typ relay` candidate through Chime's TURN, ours carried
            // eighteen, all `typ host` — private network addresses the SFU
            // cannot reach. Comparison of the two SDPs on 2026-09-30.
            ice_transport_policy: RTCIceTransportPolicy::Relay,
            ..Default::default()
        })
        .await
        .context("creating the PeerConnection failed")?;

    Ok(Arc::new(pc))
}
