use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

#[cfg(feature = "vp8")]
use anyhow::Context;
#[cfg(feature = "vp8")]
use std::time::SystemTime;
#[cfg(feature = "vp8")]
use tracing::warn;
#[cfg(feature = "vp8")]
use webrtc::media::Sample;

#[cfg(feature = "vp8")]
use crate::calls::screen::capture::{self, PixelOrder};

/// The screen is captured at this rate, and the send descriptor declares it
/// to the SFU. 30 fps is what the SHM capture pipeline can sustain at 1080p;
/// with the old socket-based capture 15 was already optimistic.
#[cfg(feature = "vp8")]
pub(crate) const SCREEN_SEND_FPS: u32 = 30;
/// The bitrate the send descriptor promises (`maxBitrateKbps`), in bits.
#[cfg(feature = "vp8")]
const SCREEN_SEND_BITRATE_BPS: u32 = 3_000_000;

pub struct VideoManager {
    _video_track: Option<Arc<TrackLocalStaticSample>>,
    _capturing: AtomicBool,
}

impl VideoManager {
    pub fn new() -> Self {
        VideoManager {
            _video_track: None,
            _capturing: AtomicBool::new(false),
        }
    }

    pub async fn init(&mut self) -> Result<Arc<TrackLocalStaticSample>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/VP8".to_string(),
                ..Default::default()
            },
            "slack-video".to_string(),
            "slack-video-id".to_string(),
        ));
        self._video_track = Some(track.clone());
        self._capturing.store(true, Ordering::SeqCst);
        info!("Video manager initialized (camera capture not yet implemented)");
        Ok(track)
    }

    pub fn track(&self) -> Option<Arc<TrackLocalStaticSample>> {
        self._video_track.clone()
    }

    pub fn is_capturing(&self) -> bool {
        self._capturing.load(Ordering::SeqCst)
    }
}

/// Captures the primary monitor, encodes each frame to VP8 and writes the
/// encoded samples to a WebRTC track.
///
/// The encoder is created on the first frame, when the real capture size is
/// known, and recreated if that size changes. Encoding is mandatory: a VP8
/// track expects whole VP8 frames, not raw pixels, and writing I420 to it
/// produces packets the far side cannot decode.
#[cfg(feature = "vp8")]
pub struct ScreenShareManager {
    screen_track: Option<Arc<TrackLocalStaticSample>>,
    sharing: Arc<AtomicBool>,
    stop_signal: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(feature = "vp8")]
impl ScreenShareManager {
    pub fn new() -> Self {
        ScreenShareManager {
            screen_track: None,
            sharing: Arc::new(AtomicBool::new(false)),
            stop_signal: None,
        }
    }

    pub async fn init(&mut self) -> Result<Arc<TrackLocalStaticSample>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "video/VP8".to_string(),
                ..Default::default()
            },
            "slack-screen".to_string(),
            "slack-screen-id".to_string(),
        ));
        self.screen_track = Some(track.clone());
        self.sharing.store(false, Ordering::SeqCst);

        Ok(track)
    }

    pub fn start_capture(&mut self) -> Result<()> {
        let track = self
            .screen_track
            .clone()
            .context("init() must be called before start_capture()")?;
        let sharing = self.sharing.clone();
        sharing.store(true, Ordering::SeqCst);

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        self.stop_signal = Some(stop_tx);
        // This track lives on the main session, whose transport is already
        // connected when the share starts: there is nothing to wait for.
        spawn_capture(track, sharing, Arc::new(AtomicBool::new(true)), None, stop_rx);

        Ok(())
    }

    pub fn stop_capture(&mut self) {
        self.sharing.store(false, Ordering::SeqCst);
        if let Some(tx) = self.stop_signal.take() {
            let _ = tx.send(());
        }
    }

    pub fn track(&self) -> Option<Arc<TrackLocalStaticSample>> {
        self.screen_track.clone()
    }

    pub fn is_sharing(&self) -> bool {
        self.sharing.load(Ordering::SeqCst)
    }
}

/// The capture-and-encode loop, shared by screen sharing and content publish.
///
/// It runs on a thread of its own, not on the call's runtime: a call owns a
/// dedicated OS thread and a current-thread runtime that also drives the
/// audio, so capturing and encoding here as ordinary tasks starved the
/// microphone and the far side stopped hearing us as soon as a share started.
/// Only `write_sample` is asynchronous, and it is driven through a handle to
/// the call's runtime.
#[cfg(feature = "vp8")]
pub(crate) fn spawn_capture(
    track: Arc<TrackLocalStaticSample>,
    sharing: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    display_id: Option<u32>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::sync::oneshot::error::TryRecvError;

    let handle = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut source = match capture::open(display_id) {
            Ok(source) => source,
            Err(e) => {
                warn!("No screen found for capture: {e:#}");
                sharing.store(false, Ordering::SeqCst);
                return;
            }
        };
        let (src_w, src_h) = source.dimensions();

        info!("Screen sharing started: {src_w}x{src_h}");

        let frame_duration = std::time::Duration::from_millis(1000 / SCREEN_SEND_FPS as u64);
        let mut next_tick = std::time::Instant::now();
        let mut encoder: Option<shiguredo_libvpx::Encoder> = None;
        let mut encoder_size = (0u32, 0u32);
        let mut frame_count = 0u64;
        let mut ts: u64 = 0;
        // The first frame has to be a keyframe, or the decoder has nothing to
        // start from.
        let mut force_keyframe = true;
        let mut last_frame_at = std::time::Instant::now();
        // The host may not keep up with 1080p30 — a busy machine, a slow CPU.
        // Rather than send frames late, step the resolution down until the work
        // per frame fits the budget, and back up when there is headroom.
        const QUALITY: [(u32, u32); 4] = [(1920, 1080), (1600, 900), (1280, 720), (960, 540)];
        let frame_budget_ms = 1000.0 / SCREEN_SEND_FPS as f64;
        let mut quality = 0usize;
        let mut work_ema_ms = 0.0f64;
        let mut checked_at = 0u64;

        loop {
            if !matches!(stop_rx.try_recv(), Err(TryRecvError::Empty)) {
                info!("Screen capture stopped");
                break;
            }

            if !sharing.load(Ordering::SeqCst) {
                break;
            }

            next_tick += frame_duration;
            let now = std::time::Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            } else {
                next_tick = now;
            }

            // Writing before the transport is up throws the first keyframe
            // away: `write_sample` returns `Ok` and sends nothing. The far
            // side then has no keyframe to decode from and waits forever, so
            // the frames are not even produced until the connection is ready.
            if !connected.load(Ordering::SeqCst) {
                force_keyframe = true;
                continue;
            }

            // The RTP timestamp advances by the sample's duration, and that must
            // be the real time between frames, not the nominal 1/30 s: when the
            // host cannot keep up and frames come slower, a fixed duration makes
            // the RTP clock fall behind wall-clock time and the receiver's
            // latency grows without bound.
            let frame_at = std::time::Instant::now();
            let frame_dt = frame_at
                .saturating_duration_since(last_frame_at)
                .max(std::time::Duration::from_millis(1));
            last_frame_at = frame_at;

            if let Err(e) = source.capture() {
                warn!("Screen capture failed: {e}");
                continue;
            }

            let (full_w, full_h) = source.dimensions();
            // The capture is the physical resolution — 2x on a HiDPI screen —
            // and software VP8 cannot keep up with 4K: the frame is scaled to
            // fit inside 1080p before encoding. libvpx also wants even
            // dimensions: with an odd one the plane strides and the packed
            // I420 layout disagree and the encoded frame is wrong from the
            // first row on.
            let (w, h) = fit_within(full_w, full_h, QUALITY[quality].0, QUALITY[quality].1);
            if w == 0 || h == 0 {
                continue;
            }

            let i420 = match convert_to_i420(
                source.bytes(),
                full_w as usize,
                full_h as usize,
                w as usize,
                h as usize,
                source.pixel_order(),
            ) {
                Some(yuv) => yuv,
                None => continue,
            };

            if encoder_size != (w, h) {
                match build_encoder(w, h) {
                    Ok(e) => {
                        info!("VP8 encoder ready: {w}x{h}");
                        encoder = Some(e);
                        encoder_size = (w, h);
                        force_keyframe = true;
                    }
                    Err(e) => {
                        warn!("VP8 encoder not created: {e:#}");
                        sharing.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
            let Some(enc) = encoder.as_mut() else { continue };

            let (y, u, v) = split_planes(&i420, w as usize, h as usize);
            let options = shiguredo_libvpx::EncodeOptions { force_keyframe };
            force_keyframe = false;
            if let Err(e) = enc.encode(&shiguredo_libvpx::ImageData::I420 { y, u, v }, &options) {
                warn!("VP8 frame not encoded: {e}");
                continue;
            }

            // One input frame can yield more than one packet; drain them all,
            // copying each out before the next borrow of the encoder.
            loop {
                let Some(data) = enc.next_frame().map(|f| f.data().to_vec()) else {
                    break;
                };
                let sample = Sample {
                    data: data.into(),
                    duration: frame_dt,
                    timestamp: SystemTime::now(),
                    packet_timestamp: ts as u32,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                if let Err(e) = handle.block_on(track.write_sample(&sample)) {
                    warn!("Failed to write screen sample: {e}");
                    sharing.store(false, Ordering::SeqCst);
                    return;
                }
            }

            if frame_count == 0 {
                info!("first screen frame sent");
            }
            frame_count += 1;
            ts += 1000 / SCREEN_SEND_FPS as u64;

            if frame_count % (SCREEN_SEND_FPS as u64 * 5) == 0 {
                info!("sharing: {frame_count} frames sent");
            }

            // If producing a frame consistently takes well over 1/fps, the
            // resolution is too high for this host; if it is comfortably under,
            // there is room for more detail. Evaluated once a second so the
            // level does not flap.
            let work_ms = frame_at.elapsed().as_secs_f64() * 1000.0;
            work_ema_ms = if work_ema_ms == 0.0 {
                work_ms
            } else {
                work_ema_ms * 0.8 + work_ms * 0.2
            };
            if frame_count - checked_at >= SCREEN_SEND_FPS as u64 {
                checked_at = frame_count;
                if work_ema_ms > frame_budget_ms * 1.2 && quality + 1 < QUALITY.len() {
                    quality += 1;
                    info!(
                        "sharing: resolution reduced to {}x{} (frame {work_ema_ms:.0}ms)",
                        QUALITY[quality].0, QUALITY[quality].1
                    );
                } else if work_ema_ms < frame_budget_ms * 0.55 && quality > 0 {
                    quality -= 1;
                    info!(
                        "sharing: resolution restored to {}x{}",
                        QUALITY[quality].0, QUALITY[quality].1
                    );
                }
            }
        }
    });
}

/// Builds the VP8 encoder for a screen of the given size.
#[cfg(feature = "vp8")]
fn build_encoder(width: u32, height: u32) -> Result<shiguredo_libvpx::Encoder> {
    use shiguredo_libvpx as vpx;
    let mut config = vpx::EncoderConfig::new(
        width as usize,
        height as usize,
        vpx::ImageFormat::I420,
        vpx::CodecConfig::Vp8(vpx::Vp8Config::default()),
    );
    config.fps_numerator = SCREEN_SEND_FPS as usize;
    config.fps_denominator = 1;
    config.target_bitrate = SCREEN_SEND_BITRATE_BPS as usize;
    // Realtime is the only sensible deadline for a live share: a slower one
    // adds latency the far side sees as a frozen screen.
    config.deadline = vpx::EncodingDeadline::Realtime;
    config.cpu_used = Some(8);
    config.rate_control = vpx::RateControlMode::Vbr;
    config.error_resilient = true;
    // A keyframe every two seconds: often enough to recover from loss,
    // rarely enough not to blow the bitrate.
    config.keyframe_interval = std::num::NonZeroUsize::new(SCREEN_SEND_FPS as usize * 2);
    config.lag_in_frames = None;
    // No lookahead in realtime, but frame threads still spread the macroblock
    // work: without them a 1080p30 encode sits on a single core.
    config.threads = std::num::NonZeroUsize::new(
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8),
    );
    vpx::Encoder::new(config).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Splits a packed I420 buffer into its three planes.
#[cfg(feature = "vp8")]
fn split_planes(i420: &[u8], width: usize, height: usize) -> (&[u8], &[u8], &[u8]) {
    let y_size = width * height;
    let uv_size = ((width + 1) / 2) * ((height + 1) / 2);
    let (y, rest) = i420.split_at(y_size);
    let (u, v) = rest.split_at(uv_size);
    (y, u, v)
}

/// Largest even size that fits inside `max_w` x `max_h` preserving the source
/// aspect ratio. libvpx wants even dimensions, so both sides are rounded down
/// to even. Returns `(0, 0)` for a degenerate source.
#[cfg(feature = "vp8")]
pub(crate) fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    let scale = (max_w as f32 / src_w as f32)
        .min(max_h as f32 / src_h as f32)
        .min(1.0);
    let w = ((src_w as f32 * scale) as u32) & !1;
    let h = ((src_h as f32 * scale) as u32) & !1;
    (w, h)
}

/// Downscales a packed 4-byte-per-pixel frame and converts it to packed I420
/// in a single pass.
///
/// Downscaling is nearest-neighbour, so it works for any ratio (the source is
/// 3840x2400 on a 2x display, the target 1728x1080). The coefficients are
/// BT.601 limited range in fixed point. `order` says whether the source bytes
/// are RGBA (the `screenshots` crate) or BGRA (the X server's ZPixmap on a
/// little-endian host). `width` and `height` must be even, because libvpx's
/// plane strides depend on it.
#[cfg(feature = "vp8")]
pub(crate) fn convert_to_i420(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    width: usize,
    height: usize,
    order: PixelOrder,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return None;
    }
    if src_width == 0 || src_height == 0 {
        return None;
    }
    if src.len() < src_width.checked_mul(src_height)?.checked_mul(4)? {
        return None;
    }

    let y_plane_size = width * height;
    let uv_width = width / 2;
    let uv_plane_size = uv_width * (height / 2);
    let mut yuv = vec![0u8; y_plane_size + uv_plane_size * 2];

    let (y_plane, rest) = yuv.split_at_mut(y_plane_size);
    let (u_plane, v_plane) = rest.split_at_mut(uv_plane_size);

    let (ri, bi) = match order {
        PixelOrder::Rgba => (0usize, 2usize),
        PixelOrder::Bgra => (2usize, 0usize),
    };
    let src_stride = src_width * 4;

    // Nearest-neighbour mapping precomputed once per frame. Doing the
    // `n * src / dst` division per output block instead is ~300 times more
    // divisions and dominated the whole conversion.
    let mut col_off = Vec::with_capacity(width);
    for col in 0..width {
        col_off.push((col * src_width / width) * 4);
    }
    let mut row_off = Vec::with_capacity(height);
    for row in 0..height {
        row_off.push((row * src_height / height) * src_stride);
    }

    let mut row = 0;
    while row < height {
        let src_top = row_off[row];
        let src_bottom = row_off[row + 1];
        let y_top = row * width;
        let y_bottom = y_top + width;
        let uv_row = (row / 2) * uv_width;

        let mut col = 0;
        while col < width {
            let x0 = col_off[col];
            let x1 = col_off[col + 1];
            let c0 = src_top + x0;
            let c1 = src_top + x1;
            let c2 = src_bottom + x0;
            let c3 = src_bottom + x1;

            let (r0, g0, b0) = (src[c0 + ri] as u32, src[c0 + 1] as u32, src[c0 + bi] as u32);
            let (r1, g1, b1) = (src[c1 + ri] as u32, src[c1 + 1] as u32, src[c1 + bi] as u32);
            let (r2, g2, b2) = (src[c2 + ri] as u32, src[c2 + 1] as u32, src[c2 + bi] as u32);
            let (r3, g3, b3) = (src[c3 + ri] as u32, src[c3 + 1] as u32, src[c3 + bi] as u32);

            y_plane[y_top + col] = ((66 * r0 + 129 * g0 + 25 * b0 + 128) >> 8) as u8 + 16;
            y_plane[y_top + col + 1] = ((66 * r1 + 129 * g1 + 25 * b1 + 128) >> 8) as u8 + 16;
            y_plane[y_bottom + col] = ((66 * r2 + 129 * g2 + 25 * b2 + 128) >> 8) as u8 + 16;
            y_plane[y_bottom + col + 1] = ((66 * r3 + 129 * g3 + 25 * b3 + 128) >> 8) as u8 + 16;

            let r = (r0 + r1 + r2 + r3) >> 2;
            let g = (g0 + g1 + g2 + g3) >> 2;
            let b = (b0 + b1 + b2 + b3) >> 2;

            let uv = uv_row + col / 2;
            u_plane[uv] =
                (((-38 * r as i32 - 74 * g as i32 + 112 * b as i32 + 128) >> 8) + 128) as u8;
            v_plane[uv] =
                (((112 * r as i32 - 94 * g as i32 - 18 * b as i32 + 128) >> 8) + 128) as u8;

            col += 2;
        }
        row += 2;
    }

    Some(yuv)
}

#[cfg(all(test, feature = "vp8"))]
mod encoder_roundtrip {
    use super::*;

    #[test]
    fn encoded_vp8_decodes() {
        let (w, h) = (64u32, 64u32);
        let mut enc = build_encoder(w, h).expect("encoder");
        let y = vec![16u8; (w * h) as usize];
        let uv = vec![128u8; ((w / 2) * (h / 2)) as usize];
        enc.encode(
            &shiguredo_libvpx::ImageData::I420 { y: &y, u: &uv, v: &uv },
            &shiguredo_libvpx::EncodeOptions { force_keyframe: true },
        )
        .expect("encode");
        let frame = enc.next_frame().expect("frame").data().to_vec();
        assert!(!frame.is_empty(), "empty frame");
        let mut dec = shiguredo_libvpx::Decoder::new(
            shiguredo_libvpx::DecoderConfig::new(shiguredo_libvpx::DecoderCodec::Vp8),
        )
        .expect("decoder");
        dec.decode(&frame).expect("decode");
    }
}

#[cfg(all(test, feature = "vp8"))]
mod i420_conversion {
    use super::*;
    use crate::calls::screen::capture::PixelOrder;

    const W: usize = 1920;
    const H: usize = 1200;

    /// Thread CPU time, to see the compute cost independently of how long the
    /// thread was descheduled on a loaded machine.
    fn thread_cpu_ns() -> u128 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        }
        ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
    }

    fn solid(r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut rgba = vec![0u8; W * H * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = 255;
        }
        rgba
    }

    fn planes(yuv: &[u8]) -> (&[u8], &[u8], &[u8]) {
        split_planes(yuv, W, H)
    }

    #[test]
    fn solid_colours_map_to_bt601_limited_range() {
        for (name, (r, g, b), (ey, eu, ev)) in [
            ("black", (0, 0, 0), (16u8, 128u8, 128u8)),
            ("white", (255, 255, 255), (235, 128, 128)),
            ("red", (255, 0, 0), (82, 90, 240)),
            ("green", (0, 255, 0), (144, 54, 34)),
            ("blue", (0, 0, 255), (41, 240, 110)),
        ] {
            let yuv = convert_to_i420(&solid(r, g, b), W, H, W, H, PixelOrder::Rgba).expect("i420");
            let (y, u, v) = planes(&yuv);
            assert_eq!(y[0], ey, "{name}: Y");
            assert_eq!(u[0], eu, "{name}: U");
            assert_eq!(v[0], ev, "{name}: V");
        }
    }

    #[test]
    fn bgra_source_matches_rgba() {
        let rgba = solid(200, 100, 50);
        let mut bgra = rgba.clone();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let a = convert_to_i420(&rgba, W, H, W, H, PixelOrder::Rgba).expect("rgba");
        let b = convert_to_i420(&bgra, W, H, W, H, PixelOrder::Bgra).expect("bgra");
        assert_eq!(a, b, "BGRA must convert to the same I420 as RGBA");
    }

    #[test]
    fn downscale_samples_the_nearest_source_pixels() {
        // A 4x2 source downscaled to 2x2 must sample the four corners
        // (0,0), (0,2), (1,0), (1,2), not the zero-filled columns between.
        let values = [[10u8, 99, 20, 99], [30, 99, 40, 99]];
        let mut src = vec![0u8; 4 * 2 * 4];
        for (row, cols) in values.iter().enumerate() {
            for (col, r) in cols.iter().enumerate() {
                let i = (row * 4 + col) * 4;
                src[i] = *r;
                src[i + 3] = 255;
            }
        }
        let yuv = convert_to_i420(&src, 4, 2, 2, 2, PixelOrder::Rgba).expect("i420");
        let (y, _, _) = split_planes(&yuv, 2, 2);
        let luma = |r: u32| (((66 * r + 128) >> 8) + 16) as u8;
        assert_eq!(y, &[luma(10), luma(20), luma(30), luma(40)]);
    }

    #[test]
    fn fit_within_keeps_the_aspect_ratio_and_stays_even() {
        assert_eq!(fit_within(3840, 2400, 1920, 1080), (1728, 1080));
        assert_eq!(fit_within(1920, 1200, 1920, 1080), (1728, 1080));
        assert_eq!(fit_within(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_within(0, 720, 1920, 1080), (0, 0));
    }

    #[test]
    #[ignore]
    fn measure_capture() {
        let screens = screenshots::Screen::all().expect("screens");
        let screen = screens.first().expect("at least one screen");
        let _ = screen.capture();
        let n = 20u32;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let img = screen.capture().expect("capture");
            std::hint::black_box(img.rgba());
        }
        println!(
            "capture {}x{}: {:?}/frame",
            screen.display_info.width,
            screen.display_info.height,
            start.elapsed() / n
        );

        let small = std::time::Instant::now();
        for _ in 0..n {
            let img = screen.capture_area(0, 0, 320, 200).expect("area");
            std::hint::black_box(img.rgba());
        }
        println!("capture 320x200: {:?}/frame", small.elapsed() / n);
    }

    #[test]
    #[ignore]
    fn measure_pipeline() {
        let mut source = crate::calls::screen::capture::open(None).expect("capture");
        let (sw, sh) = source.dimensions();
        let (dw, dh) = fit_within(sw, sh, 1920, 1080);

        source.capture().expect("warmup");
        let n = 60usize;

        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            source.capture().expect("capture");
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "capture-only {sw}x{sh}: min {:.2} / p50 {:.2} / avg {:.2} ms/frame",
            samples[0],
            samples[n / 2],
            samples.iter().sum::<f64>() / n as f64,
        );

        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            source.capture().expect("capture");
            let yuv = convert_to_i420(
                source.bytes(),
                sw as usize,
                sh as usize,
                dw as usize,
                dh as usize,
                source.pixel_order(),
            )
            .expect("convert");
            std::hint::black_box(&yuv);
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "pipeline capture+convert {sw}x{sh} -> {dw}x{dh}: min {:.2} / p50 {:.2} / avg {:.2} ms/frame",
            samples[0],
            samples[n / 2],
            samples.iter().sum::<f64>() / n as f64,
        );

        let frozen = source.bytes().to_vec();
        let mut samples = Vec::with_capacity(n);
        let mut cpu = Vec::with_capacity(n);
        for _ in 0..n {
            let t = std::time::Instant::now();
            let c = thread_cpu_ns();
            let yuv = convert_to_i420(
                &frozen,
                sw as usize,
                sh as usize,
                dw as usize,
                dh as usize,
                source.pixel_order(),
            )
            .expect("convert");
            cpu.push((thread_cpu_ns() - c) as f64 / 1_000_000.0);
            std::hint::black_box(&yuv);
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        cpu.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "convert-only {sw}x{sh} -> {dw}x{dh}: min {:.2} / p50 {:.2} / avg {:.2} ms/frame (cpu min {:.2} / p50 {:.2})",
            samples[0],
            samples[n / 2],
            samples.iter().sum::<f64>() / n as f64,
            cpu[0],
            cpu[n / 2],
        );

        let screens = screenshots::Screen::all().expect("screens");
        let screen = screens
            .iter()
            .find(|s| s.display_info.is_primary)
            .or_else(|| screens.first())
            .expect("at least one screen");
        let first = screen.capture().expect("capture");
        let (osw, osh) = (first.width(), first.height());
        let (odw, odh) = fit_within(osw, osh, 1920, 1080);
        let m = 3usize;
        let mut samples = Vec::with_capacity(m);
        for _ in 0..m {
            let t = std::time::Instant::now();
            let img = screen.capture().expect("capture");
            let yuv = convert_to_i420(
                img.rgba(),
                osw as usize,
                osh as usize,
                odw as usize,
                odh as usize,
                PixelOrder::Rgba,
            )
            .expect("convert");
            std::hint::black_box(&yuv);
            samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "pipeline [screenshots] capture+convert {osw}x{osh} -> {odw}x{odh}: min {:.2} / p50 {:.2} / avg {:.2} ms/frame",
            samples[0],
            samples[m / 2],
            samples.iter().sum::<f64>() / m as f64,
        );
    }

    #[test]
    #[ignore]
    fn measure_encode() {
        let (w, h) = fit_within(3840, 2400, 1920, 1080);
        let mut enc = build_encoder(w, h).expect("encoder");
        let mut y = vec![0u8; (w * h) as usize];
        for (i, b) in y.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2_654_435_761) >> 13) as u8;
        }
        let uv: Vec<u8> = (0..((w / 2) * (h / 2)) as usize)
            .map(|i| (i.wrapping_mul(40_503) >> 7) as u8)
            .collect();
        let frame = shiguredo_libvpx::ImageData::I420 { y: &y, u: &uv, v: &uv };
        enc.encode(&frame, &shiguredo_libvpx::EncodeOptions { force_keyframe: true })
            .expect("encode");
        while enc.next_frame().is_some() {}
        let n = 60u32;
        let start = std::time::Instant::now();
        for i in 1..=n {
            enc.encode(
                &frame,
                &shiguredo_libvpx::EncodeOptions { force_keyframe: i % 30 == 0 },
            )
            .expect("encode");
            while enc.next_frame().is_some() {}
        }
        println!("encode {w}x{h}: {:?}/frame", start.elapsed() / n);
    }

    #[test]
    fn converts_1080p_in_a_reasonable_time() {
        let rgba = solid(120, 130, 140);
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let out = convert_to_i420(&rgba, W, H, W, H, PixelOrder::Rgba).expect("i420");
            assert_eq!(out.len(), W * H + (W / 2) * (H / 2) * 2);
            std::hint::black_box(&out);
        }
        println!("convert_to_i420 {W}x{H}: {:?}/frame", start.elapsed() / 10);
    }
}
