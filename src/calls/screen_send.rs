//! Publishing our screen into a huddle.
//!
//! Amazon Chime content share is not a protocol of its own. The official SDK
//! joins the meeting a second time, with the literal string `#content`
//! appended to **both** the attendee id and the join token of the same
//! session, and opens a second signaling WebSocket and a second
//! PeerConnection. The SFU then treats that attendee as the publisher of a
//! video stream, which is what everybody else subscribes to.
//!
//! The `#content` derivation is the whole trick, and the one thing that has
//! to be right: the backend accepts it because `rooms.join` grants the
//! attendee `Capabilities.Content = SendReceive`.
//!
//! A silent Opus track is published too, exactly like the official client: the
//! audio m-line then comes first and the video lands on the camera slot (mid 1)
//! that the SFU routes content on. Nothing is asked for on receive.

use crate::calls::session::build_peer_connection;
use crate::calls::signaling::{chime, SignalingClient, MEDIA_TYPE_VIDEO};
use crate::calls::video::SCREEN_SEND_FPS;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use tracing::{debug, info, warn};
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

/// The track label the official client gives the content video.
const CONTENT_TRACK_LABEL: &str = "AmazonChimeExpressVideo";
/// The stream and group the official client allocates the content video on.
const CONTENT_STREAM_ID: u32 = 2;
const CONTENT_GROUP_ID: u32 = 2;
/// The bitrate the send descriptor promises, in kbps.
const CONTENT_MAX_BITRATE_KBPS: u32 = 3_000;

/// A screen share we are publishing.
///
/// It owns the second signaling client and the second PeerConnection, so
/// dropping it without [`ContentShare::stop`] would leave the content attendee
/// in the meeting; the toggle and the hang-up both go through `stop`.
pub struct ContentShare {
    sig: SignalingClient,
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    sharing: Arc<AtomicBool>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ContentShare {
    /// Starts publishing a monitor: `display_id` picks it, `None` the primary.
    ///
    /// `attendee_id` and `join_token` are the ones of the main session; the
    /// content session derives its own by appending `#content`, and gets its
    /// own TURN credentials from the `JOIN_ACK` it receives.
    pub async fn start(
        signaling_url: &str,
        join_token: &str,
        attendee_id: &str,
        audio_host: &str,
        display_id: Option<u32>,
    ) -> Result<Self> {
        let content_attendee = format!("{attendee_id}#content");
        let content_token = format!("{join_token}#content");

        let mut sig = SignalingClient::connect(signaling_url, &content_token)
            .await
            .context("share signaling unreachable")?;
        let (ack, _early) = sig
            .join()
            .await
            .context("share JOIN rejected")?;

        let pc = build_peer_connection(ack.turn_credentials.as_ref()).await?;

        // The audio m-line has to be first, so the video becomes the second
        // section: that is the camera slot the SFU sends content from, and the
        // shape the official client's content session has.
        let audio = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".to_string(),
                clock_rate: 48_000,
                channels: 2,
                ..Default::default()
            },
            "content-audio".to_string(),
            "content-audio-id".to_string(),
        ));
        pc.add_track(audio.clone())
            .await
            .context("share audio add_track failed")?;

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/VP8".to_string(),
                ..Default::default()
            },
            "slack-screen".to_string(),
            "slack-screen-id".to_string(),
        ));
        pc.add_track(track.clone())
            .await
            .context("share add_track failed")?;

        let offer = pc
            .create_offer(None)
            .await
            .context("share create_offer failed")?;
        pc.set_local_description(offer)
            .await
            .context("share set_local_description failed")?;

        // Chime wants an offer complete with candidates, exactly as on the
        // main session: no trickle ICE here either.
        let mut gathering = pc.gathering_complete_promise().await;
        let _ = gathering.recv().await;
        let local = pc
            .local_description()
            .await
            .context("no local description for the share")?;
        info!(
            "share offer: {}",
            crate::calls::session::video_section(&local.sdp)
        );
        debug!("share offer SDP:\n{}", local.sdp);

        let descriptor = chime::SdkStreamDescriptor {
            media_type: Some(MEDIA_TYPE_VIDEO),
            track_label: Some(CONTENT_TRACK_LABEL.to_string()),
            attendee_id: Some(content_attendee.clone()),
            stream_id: Some(CONTENT_STREAM_ID),
            group_id: Some(CONTENT_GROUP_ID),
            framerate: Some(SCREEN_SEND_FPS),
            max_bitrate_kbps: Some(CONTENT_MAX_BITRATE_KBPS),
            ..Default::default()
        };

        let ack = sig
            .subscribe_content(&local.sdp, audio_host, &content_attendee, descriptor)
            .await
            .context("share SUBSCRIBE rejected")?;
        info!(
            "share SUBSCRIBE_ACK: duplex={:?} allocations={} tracks={:?}",
            ack.duplex,
            ack.allocations.len(),
            ack.tracks
        );

        let answer_sdp = ack
            .sdp_answer
            .clone()
            .context("share SUBSCRIBE_ACK without sdp_answer")?;
        info!(
            "share answer: {}",
            crate::calls::session::video_section(&answer_sdp)
        );
        debug!("share answer SDP:\n{answer_sdp}");
        pc.set_remote_description(
            RTCSessionDescription::answer(answer_sdp)
                .context("invalid share sdp_answer")?,
        )
        .await
            .context("share set_remote_description failed")?;

        for sender in pc.get_senders().await {
            let params = sender.get_parameters().await;
            let ssrcs: Vec<u32> = params.encodings.iter().map(|e| e.ssrc).collect();
            info!("content sender: ssrc={ssrcs:?}");
        }

        let connected = Arc::new(AtomicBool::new(false));
        {
            let connected = connected.clone();
            pc.on_peer_connection_state_change(Box::new(move |state| {
                let connected = connected.clone();
                Box::pin(async move {
                    info!("sharing: PeerConnection state {state}");
                    if matches!(state, RTCPeerConnectionState::Connected) {
                        connected.store(true, Ordering::SeqCst);
                    }
                })
            }));
        }

        let sharing = Arc::new(AtomicBool::new(true));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        crate::calls::video::spawn_capture(track, sharing.clone(), connected, display_id, stop_rx);
        spawn_silent_audio(audio, sharing.clone());

        info!("screen sharing started");
        Ok(Self { sig, pc, sharing, stop: Some(stop_tx) })
    }

    /// Stops publishing and leaves the content session.
    ///
    /// The order matters: the capture loop is stopped first, then the
    /// signaling leaves and the PeerConnection closes. Closing the PC before
    /// the loop could make `write_sample` fail on every frame.
    pub async fn stop(mut self) {
        self.sharing.store(false, Ordering::SeqCst);
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        self.sig.leave().await.ok();
        self.pc.close().await.ok();
        info!("screen sharing stopped");
    }
}

/// Keeps the content session's audio stream alive with Opus silence.
///
/// The stream carries no microphone; it exists so the audio m-line is present
/// and sending, which is what the SFU expects of the content attendee.
fn spawn_silent_audio(track: Arc<TrackLocalStaticSample>, sharing: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut encoder = match opus::Encoder::new(
            48_000,
            opus::Channels::Stereo,
            opus::Application::Voip,
        ) {
            Ok(e) => e,
            Err(e) => {
                warn!("silent Opus encoder not created: {e}");
                return;
            }
        };
        // 20 ms of stereo at 48 kHz is the Opus frame size the SFU expects.
        let pcm = vec![0i16; 960 * 2];
        let mut out = vec![0u8; 4_000];
        let frame_duration = std::time::Duration::from_millis(20);
        let mut ticker = tokio::time::interval(frame_duration);
        let mut ts: u32 = 0;
        while sharing.load(Ordering::SeqCst) {
            ticker.tick().await;
            let n = match encoder.encode(&pcm, &mut out) {
                Ok(n) => n,
                Err(e) => {
                    warn!("Opus silence not encoded: {e}");
                    continue;
                }
            };
            let sample = Sample {
                data: out[..n].to_vec().into(),
                duration: frame_duration,
                timestamp: SystemTime::now(),
                packet_timestamp: ts,
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            if track.write_sample(&sample).await.is_err() {
                break;
            }
            ts = ts.wrapping_add(960);
        }
    });
}
