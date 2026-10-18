//! Amazon Chime signaling client.
//!
//! It replaces the module of the same name before it, which implemented an
//! invented protocol. This one speaks the real protocol, worked out from
//! `amazon-chime-sdk-js` (Apache-2.0) and documented in
//! `docs/huddle-protocol.md`.
//!
//! The wire works like this:
//!
//! - WebSocket to `MediaPlacement.SignalingUrl` with the query parameters
//!   `X-Chime-Control-Protocol-Version=3` and `X-Amzn-Chime-Send-Close-On-Error=1`;
//! - authentication through WebSocket **subprotocols**
//!   `["_aws_wt_session", <JoinToken>]` — not a header, not a query string;
//! - every message is one [`FRAME_TYPE_RTC`] byte followed by an
//!   `SdkSignalFrame` protobuf.

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

/// Types generated from `proto/SignalingProtocol.proto`.
pub mod chime {
    include!(concat!(env!("OUT_DIR"), "/_.rs"));
}

use chime::{SdkClientDetails, SdkIndexFrame, SdkJoinFrame, SdkSignalFrame};

/// First byte of every message on the wire (`DefaultSignalingClient`).
const FRAME_TYPE_RTC: u8 = 5;

/// The server sends a ping every 10 seconds and expects the pong.
pub const PING_INTERVAL_MS: u64 = 10_000;

/// The values of `SdkSignalFrame.type` that we need.
pub mod frame_type {
    pub const JOIN: i32 = 1;
    pub const JOIN_ACK: i32 = 2;
    pub const SUBSCRIBE: i32 = 3;
    pub const SUBSCRIBE_ACK: i32 = 4;
    pub const INDEX: i32 = 5;
    pub const PAUSE: i32 = 7;
    pub const RESUME: i32 = 8;
    pub const LEAVE: i32 = 9;
    pub const LEAVE_ACK: i32 = 10;
    pub const PING_PONG: i32 = 19;
    pub const CLIENT_METRIC: i32 = 21;
    pub const REMOTE_VIDEO_UPDATE: i32 = 24;
}

/// `SdkJoinFlags.HAS_STREAM_UPDATE`, the only flag the web client sends.
const JOIN_FLAG_HAS_STREAM_UPDATE: u32 = 2;

/// `SdkPingPongType.PING` / `.PONG`
const PING_PONG_PING: i32 = 1;
const PING_PONG_PONG: i32 = 2;

/// `SdkStreamMediaType.AUDIO`
const MEDIA_TYPE_AUDIO: i32 = 1;
/// `SdkStreamMediaType.VIDEO`. A shared screen arrives as video too: what
/// tells it from a webcam is the attendee id, which ends in `#content`.
pub const MEDIA_TYPE_VIDEO: i32 = 2;
/// `SdkStreamServiceType.RX`
const STREAM_SERVICE_RX: i32 = 1;
/// `SdkStreamServiceType.DUPLEX`. A publishing session declares this instead
/// of `RX`: it is what tells the SFU that we are sending, not only receiving.
const STREAM_SERVICE_DUPLEX: i32 = 3;

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

pub struct SignalingClient {
    socket: Socket,
    /// Where to write a copy of every frame, if asked.
    dump: Option<std::sync::Mutex<std::fs::File>>,
}

/// Writes every frame, in and out, in the same shape the capture of the
/// official client uses.
///
/// It exists so the two can be compared field by field instead of guessed at:
/// that is what settled the SRTP profile, and it is cheaper than another round
/// of changing one thing and looking. On with `SLAQ_DUMP_FRAMES=<file>`.
fn open_dump() -> Option<std::sync::Mutex<std::fs::File>> {
    let path = std::env::var_os("SLAQ_DUMP_FRAMES")?;
    // Appended, not recreated: a call that is opened twice would otherwise
    // leave only the last session in the file, and the one that mattered was
    // the first.
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => {
            info!("signaling frames to {}", path.to_string_lossy());
            Some(std::sync::Mutex::new(f))
        }
        Err(e) => {
            warn!("frame dump not opened: {e}");
            None
        }
    }
}

/// The metric types the official client sends, from the `.proto`.
///
/// They are the only ones that appear in the capture of 2026-09-30: five
/// global and two per-stream groups, the receiving one and the microphone's.
pub mod metric {
    pub const VIDEO_AVAILABLE_SEND_BANDWIDTH: i32 = 2;
    pub const STUN_RTT_MS: i32 = 7;
    pub const SOCKET_DISCARDED_PPS: i32 = 8;
    pub const RTC_MIC_PPS: i32 = 10;
    pub const RTC_MIC_BITRATE: i32 = 13;
    pub const RTC_SPK_PPS: i32 = 14;
    pub const RTC_SPK_FRACTION_PACKET_LOST_PERCENT: i32 = 15;
    pub const RTC_SPK_JITTER_MS: i32 = 16;
    pub const RTC_SPK_FRACTION_DECODER_LOSS_PERCENT: i32 = 17;
    pub const RTC_SPK_BITRATE: i32 = 18;
    pub const RTC_SPK_JITTER_BUFFER_MS: i32 = 20;
    pub const RTC_SPK_AUDIO_LEVEL: i32 = 96;
    pub const RTC_MIC_AUDIO_LEVEL: i32 = 97;
    pub const RTC_SPK_TOTAL_LOST: i32 = 98;
    pub const RTC_SPK_TOTAL_EXPECTED: i32 = 99;
    pub const CPU_PRESSURE_STATE: i32 = 113;
}

/// What we know about the session at the moment of reporting.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricSample {
    /// Round trip to the server, in milliseconds.
    pub rtt_ms: f64,
    /// Packets and bits per second going out from the microphone.
    pub mic_pps: f64,
    pub mic_bitrate: f64,
    /// The same for the receiving stream.
    pub spk_pps: f64,
    pub spk_bitrate: f64,
}

fn m(t: i32, v: f64) -> chime::SdkMetric {
    chime::SdkMetric {
        r#type: Some(t),
        value: Some(v),
        ..Default::default()
    }
}

/// Builds a `CLIENT_METRIC` in the shape the official client sends.
///
/// An **empty** frame will not do: sending empty ones every second makes the
/// server close the connection even sooner than silence does (~1.5 s instead
/// of ~3), so it really does read the content.
pub fn client_metrics(s: MetricSample) -> chime::SdkClientMetricFrame {
    const MEDIA_AUDIO: i32 = 1;
    chime::SdkClientMetricFrame {
        global_metrics: vec![
            m(metric::STUN_RTT_MS, s.rtt_ms),
            m(metric::SOCKET_DISCARDED_PPS, 0.0),
            m(metric::VIDEO_AVAILABLE_SEND_BANDWIDTH, 1_010_016.0),
            m(metric::RTC_MIC_AUDIO_LEVEL, 0.0),
            m(metric::CPU_PRESSURE_STATE, 0.0),
        ],
        stream_metric_frames: vec![
            // The receiving stream: the official client sends eleven of them.
            chime::SdkStreamMetricFrame {
                media_type: Some(MEDIA_AUDIO),
                metrics: vec![
                    m(metric::RTC_SPK_FRACTION_DECODER_LOSS_PERCENT, 0.0),
                    m(metric::RTC_SPK_PPS, s.spk_pps),
                    m(metric::RTC_SPK_FRACTION_PACKET_LOST_PERCENT, 0.0),
                    m(metric::RTC_SPK_JITTER_MS, 0.0),
                    m(metric::RTC_SPK_JITTER_BUFFER_MS, 0.0),
                    m(metric::RTC_SPK_BITRATE, s.spk_bitrate),
                    m(metric::RTC_SPK_AUDIO_LEVEL, 0.0),
                    m(metric::RTC_SPK_TOTAL_LOST, 0.0),
                    m(metric::RTC_SPK_TOTAL_EXPECTED, 1.0),
                ],
                ..Default::default()
            },
            // The microphone: packets and bitrate only.
            chime::SdkStreamMetricFrame {
                media_type: Some(MEDIA_AUDIO),
                metrics: vec![
                    m(metric::RTC_MIC_PPS, s.mic_pps),
                    m(metric::RTC_MIC_BITRATE, s.mic_bitrate),
                ],
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// Which video stream we want, on which section of our SDP.
///
/// The same description goes in two places, and both are needed: inside the
/// `SUBSCRIBE`, which is what makes the SFU allocate the stream to us, and in
/// the `REMOTE_VIDEO_UPDATE` that follows. Sending only the second one gets a
/// `SUBSCRIBE_ACK` with no track mapping, and then nothing ever arrives —
/// which is exactly how it failed the first two times.
pub fn default_video_kbps() -> u32 {
    std::env::var("SLAQ_VIDEO_KBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(800)
}

pub fn video_subscription(
    mid: &str,
    attendee_id: &str,
    group_id: u32,
    kbps: u32,
) -> chime::SdkVideoSubscriptionConfiguration {
    chime::SdkVideoSubscriptionConfiguration {
        mid: mid.to_string(),
        attendee_id: Some(attendee_id.to_string()),
        // No `streamId`: the official client leaves it out and names the
        // group instead, letting the SFU pick which of the simulcast layers
        // to send. Naming one is the only difference left between its
        // SUBSCRIBE and ours, put side by side field by field.
        stream_id: None,
        group_id: Some(group_id),
        // The official client's priority, and the low end of the bitrate it
        // alternates: it probes up to 10000, but asking for that flat out
        // pushes the SFU past what the path carries and the packets are
        // dropped — we measured ~50% loss with 10000 and none of the
        // recovery RTX would give.
        priority: Some(4_294_967_285),
        target_bitrate_kbps: Some(kbps),
        quality_adaptation_preference: Some(1),
    }
}

/// A screen somebody is sharing, as the `INDEX` announces it.
///
/// Chime does not have a screen protocol of its own: whoever shares joins the
/// meeting a second time, as an attendee whose id is their own with `#content`
/// on the end, and publishes a video stream. The old `ScreenSharingUrl` and
/// `ScreenViewingUrl` that still arrive in every invite belong to the previous
/// generation and the official client never opens them (checked over 66880
/// captured frames: not one connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedScreen {
    /// The content attendee, `<uuid>#content`.
    pub attendee: String,
    /// `<team>-<room>-<user>`: the last field is the Slack user sharing.
    pub external_user_id: String,
    /// The stream to ask for. Several arrive in the same group — simulcast,
    /// one per quality — and the first is the one the official client takes.
    pub stream_id: u32,
    pub group_id: u32,
}

/// The Slack user id inside a Chime `externalUserId`.
///
/// `externalUserId` is `<team>-<room>-<user>`, but not always: on a share of
/// one's own it came back with a number on the end instead of a user. So the
/// field is scanned for something shaped like a Slack id rather than trusting
/// the position, and if there is none the caller shows nothing instead of a
/// number nobody can read.
///
/// Shared by a screen share *and* the audio "who is speaking" map: the latter
/// stored the raw id and so never matched a participant.
pub fn slack_user_from_external_id(external: &str) -> Option<&str> {
    external
        .split('-')
        .rev()
        .find(|f| {
            f.len() >= 9
                && matches!(f.as_bytes().first(), Some(b'U' | b'W' | b'B'))
                && f.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
}

impl SharedScreen {
    /// The Slack user id of whoever is sharing, if it can be read.
    pub fn user_id(&self) -> Option<&str> {
        slack_user_from_external_id(&self.external_user_id)
    }
}

/// Reads a screen share out of an `INDEX` frame, if there is one.
///
/// The index carries every source in the room, so it is also what says the
/// share has stopped: a frame with no `#content` source means nobody is
/// sharing any more.
pub fn shared_screen(index: &SdkIndexFrame) -> Option<SharedScreen> {
    let mut found: Option<SharedScreen> = None;
    for s in &index.sources {
        if s.media_type() != MEDIA_TYPE_VIDEO {
            continue;
        }
        let attendee = s.attendee_id();
        if !attendee.ends_with("#content") {
            // A webcam, not a screen.
            continue;
        }
        let candidate = SharedScreen {
            attendee: attendee.to_string(),
            external_user_id: s.external_user_id().to_string(),
            stream_id: s.stream_id(),
            group_id: s.group_id(),
        };
        // Simulcast: the lowest id in the group is the first the server
        // announced, and it is the one the official client subscribes to.
        match &found {
            Some(prev) if prev.stream_id <= candidate.stream_id => {}
            _ => found = Some(candidate),
        }
    }
    found
}

impl SignalingClient {
    /// Opens the signaling WebSocket.
    ///
    /// `signaling_url` is `call.free_willy.meeting.MediaPlacement.SignalingUrl`
    /// and `join_token` is `call.free_willy.attendee.JoinToken`, both from the
    /// `rooms.join` response.
    pub async fn connect(signaling_url: &str, join_token: &str) -> Result<Self> {
        Self::connect_with(signaling_url, join_token, true).await
    }

    /// `close_on_error = false` drops `X-Amzn-Chime-Send-Close-On-Error`: the
    /// server sends an error frame instead of resetting the connection. Useful in
    /// diagnostics, where the reason matters more than the tidiness.
    pub async fn connect_with(
        signaling_url: &str,
        join_token: &str,
        close_on_error: bool,
    ) -> Result<Self> {
        let url = if close_on_error {
            format!("{signaling_url}?X-Chime-Control-Protocol-Version=3&X-Amzn-Chime-Send-Close-On-Error=1")
        } else {
            format!("{signaling_url}?X-Chime-Control-Protocol-Version=3")
        };

        let mut request = url
            .as_str()
            .into_client_request()
            .context("invalid signaling URL")?;

        // The join token travels as the second subprotocol. It is the detail
        // you would not guess: it is neither a header nor a query parameter.
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!("_aws_wt_session,{join_token}")
                .parse()
                .context("invalid join token as header")?,
        );

        info!("Connecting to the Chime signaling...");
        let (socket, response) = connect_async(request)
            .await
            .context("connection to the signaling failed")?;
        debug!("signaling connected: HTTP {}", response.status());

        Ok(SignalingClient { socket, dump: open_dump() })
    }

    /// Writes a frame to the dump, if one is open.
    ///
    /// Base64 and one JSON object per line, the same shape the CDP capture of
    /// the official client produces, so a single decoder reads both and the
    /// two can be put side by side.
    fn record(&self, dir: &str, raw: &[u8]) {
        let Some(file) = &self.dump else { return };
        use base64::Engine as _;
        use std::io::Write as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let line = format!("{{\"dir\":\"{dir}\",\"url\":\"slaq\",\"b64\":\"{b64}\"}}\n");
        if let Ok(mut f) = file.lock() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Serialises and sends a frame, with the one-byte prefix.
    pub async fn send_frame(&mut self, mut frame: SdkSignalFrame) -> Result<()> {
        frame.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut buf = Vec::with_capacity(frame.encoded_len() + 1);
        buf.push(FRAME_TYPE_RTC);
        frame.encode(&mut buf).context("frame encoding failed")?;

        self.record("out", &buf);

        self.socket
            .send(Message::Binary(buf))
            .await
            .context("sending the frame failed")?;
        Ok(())
    }

    /// Receives the next frame, answering the pings itself.
    ///
    /// Returns `None` if the server closes the connection.
    pub async fn recv_frame(&mut self) -> Result<Option<SdkSignalFrame>> {
        while let Some(msg) = self.socket.next().await {
            match msg.context("read error on the signaling")? {
                Message::Binary(data) => {
                    self.record("in", &data);
                    let Some((&tag, payload)) = data.split_first() else {
                        warn!("empty frame ignored");
                        continue;
                    };
                    // The official client tolerates 5 and 2, and only warns
                    // about the others: we do the same instead of aborting.
                    if tag != FRAME_TYPE_RTC && tag != 2 {
                        warn!("expected FrameTypeRTC, received {tag}");
                    }
                    let frame = SdkSignalFrame::decode(payload)
                        .context("frame decoding failed")?;
                    return Ok(Some(frame));
                }
                Message::Ping(data) => {
                    self.socket.send(Message::Pong(data)).await.ok();
                }
                Message::Close(frame) => {
                    info!("signaling closed by the server: {frame:?}");
                    return Ok(None);
                }
                other => debug!("non-binary message ignored: {other:?}"),
            }
        }
        Ok(None)
    }

    /// Sends the JOIN and waits for the JOIN_ACK.
    ///
    /// Frames that arrive before the ack (typically an INDEX) are returned
    /// together with the ack: throwing them away would lose the list of streams
    /// already in the room.
    pub async fn join(&mut self) -> Result<(chime::SdkJoinAckFrame, Vec<SdkSignalFrame>)> {
        let join = SdkJoinFrame {
            protocol_version: Some(2),
            flags: Some(JOIN_FLAG_HAS_STREAM_UPDATE),
            client_details: Some(SdkClientDetails {
                client_source: Some("slaq".into()),
                // The real Chime SDK version, not an invented one: the server
                // handles us according to it, and `15.1.0` does not exist.
                chime_sdk_version: Some("3.32.0".into()),
                platform_name: Some("linux".into()),
                os_name: Some("Linux".into()),
                ..Default::default()
            }),
            audio_session_id: Some(rand_session_id()),
            // SDP compression is optional and would cost us zlib: we refuse
            // it explicitly instead of having to handle it.
            wants_compressed_sdp: Some(false),
            // No server-side network adaptation.
            //
            // The official client declares 4 (`BandwidthProbingAndRemoteVideo
            // QualityAdaption`) because it runs a downlink policy that probes
            // the link and reports a bandwidth estimate. We do not, and a
            // client that claims 4 without probing gets its video groups
            // paused: the server sent `PAUSE {groupIds:[3]}` for the shared
            // screen and never resumed, which is why the answer allocated the
            // SSRC and not one packet ever arrived. `None` leaves the server
            // forwarding the layer we subscribed to without adapting it.
            server_side_network_adaption: Some(2),
            supported_server_side_network_adaptions: vec![2],
            ..Default::default()
        };

        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::JOIN,
            join: Some(join),
            ..Default::default()
        })
        .await?;
        info!("JOIN sent, waiting for JOIN_ACK");

        let mut others = Vec::new();
        loop {
            let Some(frame) = self.recv_frame().await? else {
                bail!("connection closed before the JOIN_ACK");
            };

            match frame.r#type {
                frame_type::JOIN_ACK => {
                    let ack = frame.joinack.clone().context("JOIN_ACK without a body")?;
                    return Ok((ack, others));
                }
                frame_type::PING_PONG => {
                    if let Some(pp) = frame.ping_pong.clone() {
                        self.send_pong(pp).await?;
                    }
                }
                _ => others.push(frame),
            }
        }
    }

    /// Sends a PING. The keepalive is the **client's** job: the server
    /// closes the connection if it stops receiving them.
    ///
    /// It must be called every [`PING_INTERVAL_MS`].
    pub async fn send_ping(&mut self, ping_id: u32) -> Result<()> {
        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::PING_PONG,
            ping_pong: Some(chime::SdkPingPongFrame {
                r#type: PING_PONG_PING,
                ping_id,
            }),
            ..Default::default()
        })
        .await
    }

    async fn send_pong(&mut self, ping: chime::SdkPingPongFrame) -> Result<()> {
        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::PING_PONG,
            ping_pong: Some(chime::SdkPingPongFrame {
                r#type: PING_PONG_PONG,
                ping_id: ping.ping_id,
            }),
            ..Default::default()
        })
        .await
    }

    /// Sends the SUBSCRIBE with the SDP offer and waits for the
    /// SUBSCRIBE_ACK.
    ///
    /// The audio `SdkStreamDescriptor` values (label, bitrate, framerate) are the
    /// official client's exact ones: `framerate` on an audio stream makes no
    /// sense, but the server expects it and reproducing it costs nothing.
    pub async fn subscribe(
        &mut self,
        sdp_offer: &str,
        audio_host: &str,
        attendee_id: &str,
        muted: bool,
        receive: &[u32],
        video: Vec<chime::SdkVideoSubscriptionConfiguration>,
    ) -> Result<chime::SdkSubscribeAckFrame> {
        let audio_stream = chime::SdkStreamDescriptor {
            media_type: Some(MEDIA_TYPE_AUDIO),
            track_label: Some("AmazonChimeExpressAudio".into()),
            attendee_id: Some(attendee_id.to_string()),
            stream_id: Some(1),
            group_id: Some(1),
            framerate: Some(15),
            max_bitrate_kbps: Some(600),
            avg_bitrate_bps: Some(400_000),
            ..Default::default()
        };

        let sub = chime::SdkSubscribeFrame {
            // RX even when we send audio: `duplex` describes the video.
            duplex: Some(STREAM_SERVICE_RX),
            send_streams: vec![audio_stream],
            // 0 is the audio, which is always wanted; a shared screen is
            // added here by its stream id, and that alone is what starts it
            // arriving — the official client does nothing else (capture of
            // 2026-10-01).
            receive_stream_ids: {
                let mut ids = vec![0];
                ids.extend(receive.iter().copied().filter(|id| *id != 0));
                ids
            },
            sdp_offer: Some(sdp_offer.to_string()),
            audio_host: Some(audio_host.to_string()),
            audio_checkin: Some(false),
            audio_muted: Some(muted),
            video_subscription_configuration: video,
            ..Default::default()
        };

        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::SUBSCRIBE,
            sub: Some(sub),
            ..Default::default()
        })
        .await?;
        info!("SUBSCRIBE sent, waiting for SUBSCRIBE_ACK");

        loop {
            let Some(frame) = self.recv_frame().await? else {
                bail!("connection closed before the SUBSCRIBE_ACK");
            };
            match frame.r#type {
                frame_type::SUBSCRIBE_ACK => {
                    return frame.suback.context("SUBSCRIBE_ACK without a body");
                }
                frame_type::PING_PONG => {
                    if let Some(pp) = frame.ping_pong.clone() {
                        self.send_pong(pp).await?;
                    }
                }
                other => {
                    // The `error` field can arrive on a frame of any type: it
                    // has to be looked at, not inferred from the type.
                    if let Some(err) = &frame.error {
                        bail!("error from the server: {err:?}");
                    }
                    debug!("frame {other} before the SUBSCRIBE_ACK");
                }
            }
        }
    }

    /// Sends the SUBSCRIBE of a publishing session and waits for the
    /// SUBSCRIBE_ACK.
    ///
    /// This is the content branch: the same meeting, joined a second time with
    /// `#content` on the attendee id and the join token. It differs from
    /// [`subscribe`] in three fields only — `duplex` is `DUPLEX`, the send
    /// stream is the video one, and nothing is asked for on receive, because
    /// this session only publishes.
    pub async fn subscribe_content(
        &mut self,
        sdp_offer: &str,
        audio_host: &str,
        attendee_id: &str,
        video: chime::SdkStreamDescriptor,
    ) -> Result<chime::SdkSubscribeAckFrame> {
        let audio_stream = chime::SdkStreamDescriptor {
            media_type: Some(MEDIA_TYPE_AUDIO),
            track_label: Some("AmazonChimeExpressAudio".to_string()),
            attendee_id: Some(attendee_id.to_string()),
            stream_id: Some(1),
            group_id: Some(1),
            framerate: Some(15),
            max_bitrate_kbps: Some(600),
            avg_bitrate_bps: Some(400_000),
            ..Default::default()
        };
        let sub = chime::SdkSubscribeFrame {
            duplex: Some(STREAM_SERVICE_DUPLEX),
            send_streams: vec![audio_stream, video],
            // The SDK documents that the backend wants the video subscriptions
            // to line up with the video m-sections, one `0` per send section
            // (`fixUpSubscriptionOrder`). This session has a single send video
            // section, so the only entry is that `0`.
            receive_stream_ids: vec![0],
            sdp_offer: Some(sdp_offer.to_string()),
            audio_host: Some(audio_host.to_string()),
            audio_checkin: Some(false),
            // No microphone on the content session: nothing is captured for it.
            audio_muted: Some(true),
            video_subscription_configuration: Vec::new(),
            ..Default::default()
        };

        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::SUBSCRIBE,
            sub: Some(sub),
            ..Default::default()
        })
        .await?;
        info!("sharing SUBSCRIBE sent, waiting for SUBSCRIBE_ACK");

        loop {
            let Some(frame) = self.recv_frame().await? else {
                bail!("connection closed before the SUBSCRIBE_ACK");
            };
            match frame.r#type {
                frame_type::SUBSCRIBE_ACK => {
                    let ack = frame.suback.context("SUBSCRIBE_ACK without a body")?;
                    // What proves the `#content` derivation was accepted is
                    // that no `error` field accompanied this ack (checked in
                    // the `other` branch below); the duplex in the answer is a
                    // second confirmation.
                    debug!(
                        "sharing {attendee_id}: SUBSCRIBE_ACK duplex={:?}",
                        ack.duplex
                    );
                    return Ok(ack);
                }
                frame_type::PING_PONG => {
                    if let Some(pp) = frame.ping_pong.clone() {
                        self.send_pong(pp).await?;
                    }
                }
                other => {
                    if let Some(err) = &frame.error {
                        bail!("error from the server: {err:?}");
                    }
                    debug!("frame {other} before the sharing SUBSCRIBE_ACK");
                }
            }
        }
    }

    /// Sends a CLIENT_METRIC frame.
    ///
    /// The official client sends it at regular intervals as soon as the media is
    /// up (`MonitorTask` → `sendClientMetrics`). Both fields are `repeated`, so an
    /// empty frame is valid protobuf: it serves to find out whether the server is
    /// happy with their presence or reads their content.
    pub async fn send_client_metrics(
        &mut self,
        frame: chime::SdkClientMetricFrame,
    ) -> Result<()> {
        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::CLIENT_METRIC,
            client_metric: Some(frame),
            ..Default::default()
        })
        .await
    }

    /// Says which video stream to put on which m-line.
    ///
    /// The `SUBSCRIBE` is what starts the stream arriving; this binds it to a
    /// section of the SDP and says how much bandwidth to give it. The official
    /// client sends it right after subscribing to a screen, with the values
    /// copied here.
    pub async fn watch_video(
        &mut self,
        mid: &str,
        attendee_id: &str,
        group_id: u32,
        kbps: u32,
    ) -> Result<()> {
        let sub = video_subscription(mid, attendee_id, group_id, kbps);
        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::REMOTE_VIDEO_UPDATE,
            remote_video_update: Some(chime::SdkRemoteVideoUpdateFrame {
                added_or_updated_video_subscriptions: vec![sub],
                removed_video_subscription_mids: vec![],
            }),
            ..Default::default()
        })
        .await
    }

    /// Sends LEAVE. The server answers with LEAVE_ACK and closes.
    pub async fn leave(&mut self) -> Result<()> {
        self.send_frame(SdkSignalFrame {
            timestamp_ms: 0,
            r#type: frame_type::LEAVE,
            leave: Some(chime::SdkLeaveFrame {}),
            ..Default::default()
        })
        .await
    }
}

/// A random audio session id, as the SDK does.
fn rand_session_id() -> u64 {
    // It only has to not collide within the room; it is not a secret.
    uuid::Uuid::new_v4().as_u128() as u64
}

#[cfg(test)]
mod screen_tests {
    use super::*;

    fn source(attendee: &str, media: i32, stream: u32, group: u32) -> chime::SdkStreamDescriptor {
        chime::SdkStreamDescriptor {
            stream_id: Some(stream),
            group_id: Some(group),
            attendee_id: Some(attendee.to_string()),
            media_type: Some(media),
            external_user_id: Some("T0TEAM0000-R0ROOM0000-U0CHIAMANTE".to_string()),
            ..Default::default()
        }
    }

    fn index(sources: Vec<chime::SdkStreamDescriptor>) -> SdkIndexFrame {
        SdkIndexFrame { sources, ..Default::default() }
    }

    #[test]
    fn a_content_attendee_is_a_shared_screen() {
        // The shape captured from the official client: the sharer appears as
        // an attendee of their own, with `#content` on the end.
        let s = shared_screen(&index(vec![source(
            "4f80df48-5306-2ef5-73ff-9e2b5c8c0e00#content",
            MEDIA_TYPE_VIDEO,
            81,
            9,
        )]))
        .expect("screen");
        assert_eq!(s.stream_id, 81);
        assert_eq!(s.group_id, 9);
        assert_eq!(s.user_id(), Some("U0CHIAMANTE"));
    }

    #[test]
    fn an_external_id_without_a_user_gives_no_name() {
        // Seen on a share of one's own: the last field was a number, and
        // showing «12200865345558 sta condividendo» helps nobody.
        let mut src = source("a#content", MEDIA_TYPE_VIDEO, 81, 9);
        src.external_user_id = Some("T0TEAM0000-R0ROOM0000-12200865345558".into());
        let s = shared_screen(&index(vec![src])).expect("screen");
        assert_eq!(s.user_id(), None);
    }

    #[test]
    fn the_user_is_found_wherever_it_sits() {
        let mut src = source("a#content", MEDIA_TYPE_VIDEO, 81, 9);
        src.external_user_id = Some("T0TEAM0000-R0ROOM0000-U0CHIAMANTE-12200865".into());
        let s = shared_screen(&index(vec![src])).expect("screen");
        assert_eq!(s.user_id(), Some("U0CHIAMANTE"));
    }

    /// The audio "who is speaking" map gets the same `<team>-<call>-<user>` id.
    /// Stored whole it never matched a participant, so the green ring never lit
    /// for a remote speaker — this is the helper the audio path now uses too.
    #[test]
    fn the_audio_speaker_id_is_read_the_same_way() {
        assert_eq!(
            slack_user_from_external_id("T0TEAM0000-R0ROOM0000-U0CHIAMANTE"),
            Some("U0CHIAMANTE")
        );
        // A share of one's own carries a number instead of a user: no name.
        assert_eq!(
            slack_user_from_external_id("T0TEAM0000-R0ROOM0000-12200865345558"),
            None
        );
    }

    #[test]
    fn a_webcam_is_not_a_screen() {
        // Same media type, no `#content`: it is someone's face.
        assert_eq!(
            shared_screen(&index(vec![source("4f80df48-aaaa", MEDIA_TYPE_VIDEO, 7, 2)])),
            None
        );
    }

    #[test]
    fn audio_is_not_a_screen() {
        assert_eq!(
            shared_screen(&index(vec![source("4f80df48-aaaa#content", MEDIA_TYPE_AUDIO, 1, 1)])),
            None
        );
    }

    #[test]
    fn of_the_simulcast_streams_the_first_is_taken() {
        // Three streams in the same group, one per quality: the official
        // client subscribes to the one the server announced first.
        let s = shared_screen(&index(vec![
            source("a#content", MEDIA_TYPE_VIDEO, 83, 9),
            source("a#content", MEDIA_TYPE_VIDEO, 81, 9),
            source("a#content", MEDIA_TYPE_VIDEO, 82, 9),
        ]))
        .expect("screen");
        assert_eq!(s.stream_id, 81);
    }

    #[test]
    fn an_index_without_content_means_nobody_is_sharing() {
        // This is also how a share that has stopped is recognised: the index
        // carries every source in the room, so its absence is the signal.
        assert_eq!(shared_screen(&index(vec![])), None);
    }
}
