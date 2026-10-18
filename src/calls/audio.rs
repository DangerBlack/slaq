//! Audio capture and playback for WebRTC calls.
//!
//! Captures the microphone with cpal, encodes it as Opus and pushes it onto a
//! WebRTC track; in the other direction it decodes what arrives from the
//! others and plays it on the speakers.

use crate::calls::wav;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};
use webrtc::media::Sample;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 1;
/// 20 ms at 48 kHz mono
const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE as usize * 20) / 1000;
/// Ceiling for a 20 ms Opus frame: generous, the encoder uses far less.
const MAX_OPUS_FRAME_BYTES: usize = 4000;
/// WebRTC's audio module works on 10 ms blocks.
const APM_FRAME: usize = (SAMPLE_RATE as usize * 10) / 1000;
/// Target depth of the playback queue (~90 ms): it is the point at which
/// the queue realigns itself when it exceeds the ceiling.
const PLAYBACK_TARGET: usize = (SAMPLE_RATE as usize * 90) / 1000;
/// Ceiling of the playback queue (~300 ms). Beyond it, the oldest is dropped:
/// never a total flush, which would be a one-second jump.
const PLAYBACK_MAX: usize = (SAMPLE_RATE as usize * 300) / 1000;
/// Estimated constant latency between render and capture, in ms. AEC3 wants it:
/// without it, the adaptive filter does not converge and the echo remains.
/// `SLAQ_AEC_DELAY_MS` overrides it (device buffers + capture + a small margin).
///
/// Measured in the field: with the render given at playback time the residual
/// latency is that of the device buffers (~10-20 ms). Higher values (50-160)
/// worsen the ERLE, so a small value is better and the fine delay is left for
/// AEC3 to estimate.
const DEFAULT_AEC_DELAY_MS: i32 = 20;

/// Render→capture delay to pass to AEC3.
///
/// The value can be measured from the device buffers, but it changes from
/// machine to machine and from moment to moment: a reasonable default plus an
/// environment override is more predictable than a wrong estimate.
fn aec_delay_ms() -> i32 {
    std::env::var("SLAQ_AEC_DELAY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(DEFAULT_AEC_DELAY_MS)
        .clamp(0, 500)
}

/// Echo cancellation and noise suppression, shared between the two
/// directions.
///
/// Echo is removed by comparing what goes out of the speakers with what comes
/// in from the microphone: both signals are needed in the same object, from
/// different threads — capture runs on cpal's audio thread, playback on
/// another. Hence the mutex.
pub type Apm = Option<Arc<std::sync::Mutex<sonora::AudioProcessing>>>;

/// Builds the audio module with echo and noise on.
///
/// `None` turns it off entirely: useful for comparing, and as a way out if on
/// some machine the processing made things worse.
fn build_apm() -> Apm {
    let mut cfg = sonora::Config::default();
    // The two modules are turned on by assigning them: absent means off.
    cfg.echo_canceller = Some(sonora::config::EchoCanceller::default());
    cfg.noise_suppression = Some(sonora::config::NoiseSuppression::default());
    // The format has to be declared: by default the module assumes 16 kHz
    // mono, and handing it 48 kHz blocks makes it read them as three times
    // longer. The result is not an error, it is noise.
    let fmt = sonora::StreamConfig::new(SAMPLE_RATE, CHANNELS);
    let apm = sonora::AudioProcessing::builder()
        .config(cfg)
        .capture_config(fmt)
        .render_config(fmt)
        .build();
    Some(Arc::new(std::sync::Mutex::new(apm)))
}

/// Passes a block of capture samples through the module, in 10 ms pieces.
///
/// A remainder shorter than 10 ms is left as it is: accumulating it would give
/// a growing delay, and discarding it a gap in the audio. Render, instead,
/// goes through [`feed_render`] at playback time.
fn apm_capture(apm: &Apm, pcm: &mut [i16]) {
    let Some(apm) = apm else { return };
    let Ok(mut a) = apm.lock() else { return };
    let mut scratch = [0i16; APM_FRAME];
    for chunk in pcm.chunks_mut(APM_FRAME) {
        if chunk.len() < APM_FRAME {
            break;
        }
        if a.process_capture_i16(chunk, &mut scratch).is_ok() {
            chunk.copy_from_slice(&scratch);
        }
    }
}

/// Gives the module the render samples that are about to come out of the
/// speakers.
///
/// It must happen at playback time, not when the packet arrives: it is the
/// reference with which AEC3 recognises its own sound in the microphone. The
/// blocks are 10 ms each; the remainder stays in `stage` for the next call, so
/// the render timeline stays continuous instead of jumping back and forth.
fn feed_render(apm: &Apm, stage: &mut Vec<i16>, consumed: &[i16]) {
    let Some(apm) = apm else { return };
    stage.extend_from_slice(consumed);
    let Ok(mut a) = apm.lock() else {
        stage.clear();
        return;
    };
    let mut scratch = [0i16; APM_FRAME];
    let mut done = 0;
    while done + APM_FRAME <= stage.len() {
        if a.process_render_i16(&stage[done..done + APM_FRAME], &mut scratch)
            .is_err()
        {
            break;
        }
        done += APM_FRAME;
    }
    stage.drain(..done);
}

/// Diagnostics: opens a WAV dump, if a path is configured.
///
/// A failure must not stop the capture: it warns and continues without the
/// dump. `name` is the variable name, for the message.
fn open_dump(path: Option<&std::path::Path>, name: &str) -> Option<wav::WavWriter> {
    let path = path?;
    match wav::WavWriter::create(path) {
        Ok(w) => {
            info!("diagnostic dump {name} active: {}", path.display());
            Some(w)
        }
        Err(e) => {
            warn!("dump {name} cannot be opened ({}): {e}", path.display());
            None
        }
    }
}

/// RMS level of a block, scaled to 0..100 for the "speaking" indicator.
fn rms_level(pcm: &[i16]) -> u32 {
    if pcm.is_empty() {
        return 0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum / pcm.len() as f64).sqrt();
    ((rms / i16::MAX as f64 * 400.0) as u32).min(100)
}

pub struct AudioManager {
    /// Shared between capture and playback: it is the comparison between the
    /// two directions that lets the echo be removed.
    apm: Apm,
    _stream: Option<cpal::Stream>,
    /// The stream towards the speakers. Keeping it alive is what makes it
    /// play: drop it and cpal closes the device.
    _output: Option<cpal::Stream>,
    audio_track: Option<Arc<TrackLocalStaticSample>>,
    _playback_tx: Option<mpsc::Sender<Vec<i16>>>,
    muted: Arc<Mutex<bool>>,
    /// Our microphone level (0..100), to highlight ourselves when we speak:
    /// the SFU only reports the level of the remote tracks.
    self_level: Arc<std::sync::atomic::AtomicU32>,
    _sequence: u64,
    /// Diagnostics: where to write the pre-AEC capture. If `None`,
    /// `SLAQ_DUMP_MIC` is looked at once only at task startup.
    dump_mic: Option<PathBuf>,
    /// Diagnostics: where to write the post-AEC capture. If `None`,
    /// `SLAQ_DUMP_TX` is looked at once only at task startup.
    dump_tx: Option<PathBuf>,
}

impl AudioManager {
    pub fn new() -> Self {
        Self::with_processing(true)
    }

    /// `processing` false turns off echo and noise suppression.
    pub fn with_processing(processing: bool) -> Self {
        AudioManager {
            apm: if processing { build_apm() } else { None },
            _stream: None,
            _output: None,
            audio_track: None,
            _playback_tx: None,
            muted: Arc::new(Mutex::new(false)),
            self_level: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            _sequence: 0,
            dump_mic: None,
            dump_tx: None,
        }
    }

    /// Diagnostics: sets the pre/post AEC dump paths.
    ///
    /// It takes precedence over the `SLAQ_DUMP_MIC`/`SLAQ_DUMP_TX` environment
    /// variables. It serves the `aec-probe` probe so it does not have to resort
    /// to `set_var`, which is `unsafe` in the 2024 edition.
    pub fn with_dumps(mut self, mic: Option<PathBuf>, tx: Option<PathBuf>) -> Self {
        self.dump_mic = mic;
        self.dump_tx = tx;
        self
    }

    /// Our microphone level (0..100), to highlight ourselves while we speak.
    pub fn self_level(&self) -> u32 {
        self.self_level.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Picks a device whose name contains `want`, or the default one if
    /// `want` is empty.
    ///
    /// The comparison is by substring and case-insensitive: the real names are
    /// things like
    /// `alsa_input.usb-Kingston_HyperX_Quadcast_4110-00.analog-stereo`, and
    /// nobody wants to type them in full.
    fn pick_device(
        host: &cpal::Host,
        want: Option<&str>,
        input: bool,
    ) -> Result<cpal::Device> {
        let want = want.map(str::trim).filter(|w| !w.is_empty());
        if let Some(w) = want {
            let needle = w.to_lowercase();
            let mut devices = if input {
                host.input_devices().context("input devices")?
            } else {
                host.output_devices().context("output devices")?
            };
            if let Some(d) = devices.find(|d| {
                d.name()
                    .map(|n| n.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            }) {
                return Ok(d);
            }
            anyhow::bail!("no device matches «{w}»");
        }
        if input {
            host.default_input_device().context("no microphone")
        } else {
            host.default_output_device().context("no speaker")
        }
    }

    /// Lists the available devices, so they can be named.
    pub fn list_devices() {
        let host = cpal::default_host();
        println!("{}", t!("cli.audio_mics"));
        if let Ok(ds) = host.input_devices() {
            for d in ds {
                println!("  {}", d.name().unwrap_or_else(|_| "?".into()));
            }
        }
        println!("{}", t!("cli.audio_speakers"));
        if let Ok(ds) = host.output_devices() {
            for d in ds {
                println!("  {}", d.name().unwrap_or_else(|_| "?".into()));
            }
        }
    }

    pub fn init(&mut self, mic: Option<&str>) -> Result<Arc<TrackLocalStaticSample>> {
        let host = cpal::default_host();
        let device = Self::pick_device(&host, mic, true)?;
        let config = device
            .default_input_config()
            .context("Failed to get default input config")?;

        info!(
            "Mic: {} @ {} Hz, {} ch, {:?}",
            device.name().unwrap_or_else(|_| "?".into()),
            config.sample_rate().0,
            config.channels(),
            config.sample_format()
        );

        let hw_rate = config.sample_rate().0;
        let hw_channels = config.channels();
        let downmix = hw_channels > 1;
        let resample_ratio = hw_rate as f64 / SAMPLE_RATE as f64;

        // Create the WebRTC audio track
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".to_string(),
                ..Default::default()
            },
            "slack-audio".to_string(),
            "slack-audio-id".to_string(),
        ));
        self.audio_track = Some(track.clone());

        // Channel to push PCM from the capture callback to the encoding task.
        // Deep enough that a slow consumer does not eat the microphone: the
        // capture has to stay contiguous for the echo canceller.
        let (pcm_tx, mut pcm_rx) = mpsc::channel::<Vec<i16>>(256);
        let capture_drops = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Task: takes the PCM from the callback, encodes it as Opus and
        // writes it on the track.
        //
        // The track is declared `audio/opus`, so the samples **must** be Opus:
        // writing raw PCM to it produces RTP the peer cannot decode, and an SFU
        // like Chime closes the session after a few seconds instead of
        // signalling an error.
        let track_for_task = track.clone();
        let muted = self.muted.clone();
        let apm_cap = self.apm.clone();
        let self_level = self.self_level.clone();
        // Diagnostics: explicitly set paths, otherwise the environment
        // variables. Read once only here, not in the loop.
        let dump_mic = self
            .dump_mic
            .clone()
            .or_else(|| std::env::var_os("SLAQ_DUMP_MIC").map(PathBuf::from));
        let dump_tx = self
            .dump_tx
            .clone()
            .or_else(|| std::env::var_os("SLAQ_DUMP_TX").map(PathBuf::from));
        tokio::spawn(async move {
            let mut encoder = match opus::Encoder::new(
                SAMPLE_RATE,
                opus::Channels::Mono,
                opus::Application::Voip,
            ) {
                Ok(e) => e,
                Err(e) => {
                    warn!("cannot create the Opus encoder: {e}");
                    return;
                }
            };

            // AEC3 requires the constant latency between render and capture:
            // without it, the adaptation does not converge and the echo
            // remains. It is set once here, on the thread that processes the
            // capture, and what the module thinks of it is logged.
            if let Some(apm) = &apm_cap {
                if let Ok(mut a) = apm.lock() {
                    let delay = aec_delay_ms();
                    match a.set_stream_delay_ms(delay) {
                        Ok(()) => {}
                        Err(e) => warn!("AEC delay {delay} ms clamped: {e}"),
                    }
                    info!(
                        "AEC: delay set to {} ms (module estimate: {:?} ms)",
                        a.stream_delay_ms(),
                        a.statistics().delay_ms
                    );
                }
            }

            let mut buffer: Vec<i16> = Vec::with_capacity(SAMPLES_PER_FRAME);
            let mut encoded = vec![0u8; MAX_OPUS_FRAME_BYTES];
            let silence = vec![0i16; SAMPLES_PER_FRAME];
            let mut ts: u64 = 0;

            // Diagnostics: dump writers, one pre-AEC and one post-AEC. Absent
            // if the paths are not configured: no overhead when the feature is
            // not used. The files are closed with the task's `Drop`.
            let mut dump_mic_w = open_dump(dump_mic.as_deref(), "SLAQ_DUMP_MIC");
            let mut dump_tx_w = open_dump(dump_tx.as_deref(), "SLAQ_DUMP_TX");

            while let Some(chunk) = pcm_rx.recv().await {
                buffer.extend_from_slice(&chunk);

                while buffer.len() >= SAMPLES_PER_FRAME {
                    let mut frame: Vec<i16> = buffer.drain(..SAMPLES_PER_FRAME).collect();

                    // Diagnostics: raw capture, before the AEC.
                    if let Some(w) = &mut dump_mic_w {
                        let _ = w.write(&frame);
                    }

                    // This is where the echo goes: the module compares this
                    // block with the one just sent to the speakers.
                    apm_capture(&apm_cap, &mut frame);

                    // Diagnostics: capture after the AEC, before the
                    // replacement for mute (same block alignment as the
                    // pre-AEC dump).
                    if let Some(w) = &mut dump_tx_w {
                        let _ = w.write(&frame);
                    }

                    // While muted we still send encoded silence: the RTP
                    // stream must not stop, or the SFU considers the stream
                    // dead. Real mute is signalled separately.
                    let is_muted = *muted.lock().await;
                    let pcm: &[i16] = if is_muted { &silence } else { &frame };
                    self_level.store(
                        if is_muted { 0 } else { rms_level(&frame) },
                        std::sync::atomic::Ordering::Relaxed,
                    );

                    let n = match encoder.encode(pcm, &mut encoded) {
                        Ok(n) => n,
                        Err(e) => {
                            warn!("Opus encoding failed: {e}");
                            continue;
                        }
                    };

                    let sample = Sample {
                        data: bytes::Bytes::copy_from_slice(&encoded[..n]),
                        duration: std::time::Duration::from_millis(20),
                        timestamp: SystemTime::now(),
                        packet_timestamp: ts as u32,
                        prev_dropped_packets: 0,
                        prev_padding_packets: 0,
                    };

                    if let Err(e) = track_for_task.write_sample(&sample).await {
                        warn!("writing the audio sample failed: {e}");
                        break;
                    }
                    ts += SAMPLES_PER_FRAME as u64;
                }
            }
        });

        // Build the cpal input stream
        let err_fn = move |err| warn!("cpal input error: {err}");
        let drops_for_cb = capture_drops.clone();

        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Convert f32 samples to i16, downmix if needed
                    let chunk: Vec<i16> = if downmix {
                        data.chunks_exact(hw_channels as usize)
                            .map(|c| {
                                let sum: f32 = c.iter().sum();
                                let avg = sum / hw_channels as f32;
                                (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
                            })
                            .collect()
                    } else {
                        data.iter()
                            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                            .collect()
                    };

                    // Resample if hardware rate ≠ 48 kHz
                    let resampled: Vec<i16> = if (resample_ratio - 1.0).abs() < 1e-6 {
                        chunk
                    } else {
                        let out_len = (chunk.len() as f64 / resample_ratio) as usize;
                        let mut out = Vec::with_capacity(out_len);
                        for i in 0..out_len {
                            let src_idx = (i as f64 * resample_ratio) as usize;
                            if let Some(&s) = chunk.get(src_idx) {
                                out.push(s);
                            }
                        }
                        out
                    };

                    if pcm_tx.try_send(resampled).is_err() {
                        // It is not dropped silently: if the consumer lags
                        // behind, the capture loses pieces, and for the echo
                        // canceller that is a hole. It is counted and reported,
                        // rate-limited so as not to flood the logs from the
                        // audio thread.
                        let n = drops_for_cb
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        if n == 1 || n % 100 == 0 {
                            warn!("capture lagging behind: {n} blocks dropped (channel full)");
                        }
                    }
                },
                err_fn,
                None,
            )
            .context("Failed to build cpal input stream")?;

        stream
            .play()
            .context("Failed to start cpal input stream")?;

        self._stream = Some(stream);
        info!("Mic capture started");

        Ok(track)
    }

    /// Opens the speakers and returns the end to write the samples to.
    ///
    /// Opus decoding happens upstream, in whoever reads the remote track:
    /// what arrives here are 48 kHz mono samples, which are adapted to the
    /// device's rate and mixed across its channels.
    pub fn open_playback(&mut self, speaker: Option<&str>) -> Result<mpsc::Sender<Vec<i16>>> {
        let host = cpal::default_host();
        let device = Self::pick_device(&host, speaker, false)?;
        let config = device
            .default_output_config()
            .context("output configuration not readable")?;
        let dev_rate = config.sample_rate().0;
        let dev_channels = config.channels() as usize;
        let fmt = config.sample_format();
        info!(
            "speakers: {} Hz, {} channels, format {:?}",
            dev_rate, dev_channels, fmt
        );

        // A wide queue: better a few tens of milliseconds of delay than gaps
        // in the audio when the network thread arrives in bursts.
        let (tx, mut rx) = mpsc::channel::<Vec<i16>>(64);
        let buffer: Arc<std::sync::Mutex<std::collections::VecDeque<i16>>> =
            Arc::new(std::sync::Mutex::new(Default::default()));

        // Only the decoded, raw samples enter here: the reference for AEC3 is
        // given at playback time, in the output callback, not when the packet
        // arrives.
        let fill = buffer.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let Ok(mut b) = fill.lock() {
                    // Bounded queue: below the ceiling it enqueues, above it
                    // drops the oldest down to the target. A total flush would
                    // be a whole one-second jump.
                    if b.len() + frame.len() > PLAYBACK_MAX {
                        let excess = b.len() + frame.len() - PLAYBACK_TARGET;
                        let drop = excess.min(b.len());
                        b.drain(..drop);
                    }
                    b.extend(frame);
                }
            }
        });

        let ratio = SAMPLE_RATE as f64 / dev_rate as f64;
        let cfg: cpal::StreamConfig = config.into();

        // The device's format has to be respected: building an `f32` stream
        // on a 16-bit card gives no error, it gives silence. It was the reason
        // nothing could be heard while the packets were arriving.
        let apm_out = self.apm.clone();

        fn build<T>(
            device: &cpal::Device,
            cfg: &cpal::StreamConfig,
            buffer: Arc<std::sync::Mutex<std::collections::VecDeque<i16>>>,
            ratio: f64,
            channels: usize,
            apm: Apm,
        ) -> Result<cpal::Stream>
        where
            T: cpal::SizedSample + cpal::FromSample<f32>,
        {
            let err_fn = move |e| warn!("audio output error: {e}");
            // Remainder not yet given to AEC3: it is carried from one callback
            // to the next to stay on the 10 ms blocks.
            let mut render_stage: Vec<i16> = Vec::with_capacity(APM_FRAME * 2);
            let mut consumed: Vec<i16> = Vec::with_capacity(APM_FRAME * 4);
            let stream = device
                .build_output_stream(
                    cfg,
                    move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                        let silence = T::from_sample(0.0f32);
                        // The consumed samples are copied out holding the lock
                        // as little as possible: the AEC3 feed happens after,
                        // without the queue lock (and never taking it in reverse).
                        {
                            let Ok(mut b) = buffer.lock() else {
                                out.fill(silence);
                                return;
                            };
                            let mut pos = 0.0f64;
                            for frame in out.chunks_mut(channels) {
                                let idx = pos as usize;
                                let v = if idx < b.len() {
                                    b[idx] as f32 / i16::MAX as f32
                                } else {
                                    0.0
                                };
                                let s = T::from_sample(v);
                                // Mono to N channels: the same sample everywhere.
                                for c in frame.iter_mut() {
                                    *c = s;
                                }
                                pos += ratio;
                            }
                            // The callback consumes samples *at 48 kHz*: these
                            // are the ones to give to the reference, not those
                            // at the device rate. `pos` advances by `ratio` for
                            // every written frame, so the integer part is how
                            // many source samples have been read.
                            let used = (pos as usize).min(b.len());
                            consumed.clear();
                            consumed.extend(b.drain(..used));
                        }
                        feed_render(&apm, &mut render_stage, &consumed);
                    },
                    err_fn,
                    None,
                )
                .context("opening the speakers failed")?;
            Ok(stream)
        }

        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                build::<f32>(&device, &cfg, buffer, ratio, dev_channels, apm_out)
            }
            cpal::SampleFormat::I16 => {
                build::<i16>(&device, &cfg, buffer, ratio, dev_channels, apm_out)
            }
            cpal::SampleFormat::U16 => {
                build::<u16>(&device, &cfg, buffer, ratio, dev_channels, apm_out)
            }
            other => anyhow::bail!("unsupported output format: {other:?}"),
        }?;

        stream.play().context("starting the speakers failed")?;
        self._output = Some(stream);
        Ok(tx)
    }

    pub fn track(&self) -> Option<Arc<TrackLocalStaticSample>> {
        self.audio_track.clone()
    }

    pub async fn toggle_mute(&self) -> bool {
        let mut muted = self.muted.lock().await;
        *muted = !*muted;
        *muted
    }

    pub async fn is_muted(&self) -> bool {
        *self.muted.lock().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic PRNG (xorshift64): no new dependencies and no random
    /// noise between one run and another.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x >> 32) as u32
        }

        fn sample(&mut self) -> f32 {
            self.next_u32() as f32 / u32::MAX as f32 * 2.0 - 1.0
        }
    }

    fn to_i16(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
    }

    /// Builds the far-end `x` and the capture `y = gain * x[t-d] + noise`.
    fn make_signals(seconds: usize, delay_samples: usize, gain: f32) -> (Vec<i16>, Vec<i16>) {
        let n = SAMPLE_RATE as usize * seconds;
        let mut far_rng = Rng::new(0x1234_5678_9abc_def0);
        let far: Vec<i16> = (0..n).map(|_| to_i16(far_rng.sample() * 0.3)).collect();
        let mut noise = Rng::new(0xdead_beef_0bad_f00d);
        let mut near = vec![0i16; n];
        for t in 0..n {
            let echo = if t >= delay_samples {
                gain * far[t - delay_samples] as f32 / i16::MAX as f32
            } else {
                0.0
            };
            near[t] = to_i16(echo + noise.sample() * 0.001);
        }
        (far, near)
    }

    /// ERLE in dB on the tail of the signal, after the filter has converged.
    ///
    /// `render` and `capture` are aligned block by block: if the reference
    /// arrives before or after the real sound, the echo is not cancelled. The
    /// same [`build_apm`] as the real path is used, so the test exercises
    /// exactly the configuration that runs in a call.
    fn erle_db(render: &[i16], capture: &[i16], delay_ms: i32, measure_from: usize) -> f64 {
        erle_db_order(render, capture, delay_ms, measure_from, true)
    }

    fn erle_db_order(
        render: &[i16],
        capture: &[i16],
        delay_ms: i32,
        measure_from: usize,
        render_first: bool,
    ) -> f64 {
        let apm = build_apm();
        let apm = apm.as_ref().expect("APM missing");
        let mut a = apm.lock().unwrap();
        a.set_stream_delay_ms(delay_ms).ok();
        let mut scratch = [0i16; APM_FRAME];
        let mut render_scratch = [0i16; APM_FRAME];
        let blocks = render.len().min(capture.len()) / APM_FRAME;
        let from = measure_from / APM_FRAME;
        let (mut p_y, mut p_out) = (0.0f64, 0.0f64);
        for b in 0..blocks {
            let lo = b * APM_FRAME;
            let hi = lo + APM_FRAME;
            if render_first {
                a.process_render_i16(&render[lo..hi], &mut render_scratch).unwrap();
                a.process_capture_i16(&capture[lo..hi], &mut scratch).unwrap();
            } else {
                a.process_capture_i16(&capture[lo..hi], &mut scratch).unwrap();
                a.process_render_i16(&render[lo..hi], &mut render_scratch).unwrap();
            }
            if b >= from {
                for i in 0..APM_FRAME {
                    let y = capture[lo + i] as f64;
                    let o = scratch[i] as f64;
                    p_y += y * y;
                    p_out += o * o;
                }
            }
        }
        10.0 * (p_y / p_out.max(1.0)).log10()
    }

    #[test]
    fn aec_cancels_echo_with_render_timed_to_playback() {
        // Echo at 50 ms, reference given in the documented order (first
        // process_render, then process_capture) and with the declared delay:
        // it is exactly the new wiring, reference at playback time.
        let (far, near) = make_signals(6, (SAMPLE_RATE as usize * 50) / 1000, 0.5);
        let erle = erle_db(&far, &near, 50, SAMPLE_RATE as usize * 3);
        assert!(erle > 20.0, "ERLE too low: {erle:.1} dB (expected > 20)");
    }

    #[test]
    fn aec_worsens_with_reference_after_capture() {
        // The other way to get the wiring wrong: giving the reference after
        // having already processed the capture. With a short echo (a 10 ms
        // block) the right reference has not arrived yet and the cancellation
        // collapses compared to the correct order.
        let delay = (SAMPLE_RATE as usize * 10) / 1000;
        let (far, near) = make_signals(6, delay, 0.5);
        let from = SAMPLE_RATE as usize * 3;
        let good = erle_db_order(&far, &near, 10, from, true);
        let bad = erle_db_order(&far, &near, 10, from, false);
        assert!(
            bad + 15.0 < good,
            "the order does not matter: correct {good:.1} dB, wrong {bad:.1} dB"
        );
    }

    #[test]
    fn aec_does_not_cancel_with_reference_not_following_playback() {
        // Model of the old bug: the reference is not the sound that actually
        // comes out of the speakers (it was given on packet arrival, while the
        // queue could jump by a second). An incoherent reference does not let
        // the filter align: the echo remains.
        let (far, near) = make_signals(6, (SAMPLE_RATE as usize * 50) / 1000, 0.5);
        let incoerente = vec![0i16; far.len()];
        let erle = erle_db(&incoerente, &near, 50, SAMPLE_RATE as usize * 3);
        assert!(erle < 20.0, "the echo turns out cancelled anyway: {erle:.1} dB");
    }
}
