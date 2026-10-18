//! VP8 decoding, with libvpx.
//!
//! The only native dependency in the project, and it is here because it has
//! to be: whoever shares a screen chooses the codec, and whoever shares is
//! the official client — that is, Chrome, which sends VP8 or H.264. We can
//! only decode what arrives. There are pure-Rust VP8 and H.264 decoders on
//! crates.io, but they are recent, single-author and unproven; when one of
//! them becomes credible, replacing this file is the whole job.
//!
//! libvpx is built from source (the `source-build` feature) rather than
//! linked against the system one, so the binary stays self-contained: that
//! was the requirement from the start, and it is why the echo cancellation is
//! in Rust rather than being PipeWire's.

use anyhow::{Context as _, Result};
use shiguredo_libvpx as vpx;

use super::Picture;

pub struct VpxDecoder {
    inner: vpx::Decoder,
}

impl VpxDecoder {
    pub fn new() -> Result<Self> {
        let config = vpx::DecoderConfig::new(vpx::DecoderCodec::Vp8);
        let inner = vpx::Decoder::new(config).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self { inner })
    }

    /// Decodes one frame into a picture.
    ///
    /// A frame does not always produce a picture — libvpx can hold one back —
    /// so `None` is an ordinary outcome and not a failure.
    pub fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        self.inner
            .decode(frame)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("VP8 frame not decoded")?;
        // The decoder returns frames one `next_frame()` at a time and has to be
        // drained until it returns `None`: a frame left pending makes the next
        // `decode()` fail with "still need to call next_frame()", and then every
        // later frame is dropped until a keyframe resets the stream.
        let mut picture = None;
        while let Some(img) = self
            .inner
            .next_frame()
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            picture = Some(i420_to_rgba(&img));
        }
        Ok(picture)
    }
}

/// Converts the frame from I420 to RGBA.
///
/// Video travels as luminance plus two chrominance planes at half resolution;
/// a screen is RGB. The coefficients are BT.601 with limited range (16-235),
/// which is what WebRTC uses: with the full-range ones the picture comes out
/// washed out, and it is the kind of mistake that looks like a bad monitor.
fn i420_to_rgba(img: &vpx::DecodedFrame<'_>) -> Picture {
    let (w, h) = (img.width(), img.height());
    let (y_plane, u_plane, v_plane) = (img.y_plane(), img.u_plane(), img.v_plane());
    let (ys, us, vs) = (img.y_stride(), img.u_stride(), img.v_stride());
    let mut rgba = vec![0u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            // The chrominance planes are half the size in both directions:
            // four pixels share one colour sample.
            let y = y_plane[row * ys + col] as f32;
            let u = u_plane[(row / 2) * us + col / 2] as f32 - 128.0;
            let v = v_plane[(row / 2) * vs + col / 2] as f32 - 128.0;
            let y = (y - 16.0) * 1.164;

            let o = (row * w + col) * 4;
            rgba[o] = clamp(y + 1.596 * v);
            rgba[o + 1] = clamp(y - 0.392 * u - 0.813 * v);
            rgba[o + 2] = clamp(y + 2.017 * u);
            rgba[o + 3] = 255;
        }
    }

    Picture { width: w as u32, height: h as u32, rgba }
}

fn clamp(v: f32) -> u8 {
    v.clamp(0.0, 255.0) as u8
}

#[cfg(all(test, feature = "vp8"))]
mod drain {
    use super::*;
    use shiguredo_libvpx as vpx;

    fn encoded_frames(count: usize) -> Vec<Vec<u8>> {
        let (w, h) = (64usize, 64usize);
        let mut config = vpx::EncoderConfig::new(
            w,
            h,
            vpx::ImageFormat::I420,
            vpx::CodecConfig::Vp8(vpx::Vp8Config::default()),
        );
        config.lag_in_frames = None;
        config.deadline = vpx::EncodingDeadline::Realtime;
        let mut enc = vpx::Encoder::new(config).expect("encoder");
        let y = vec![16u8; w * h];
        let uv = vec![128u8; (w / 2) * (h / 2)];
        let mut frames = Vec::new();
        for i in 0..count {
            enc.encode(
                &vpx::ImageData::I420 { y: &y, u: &uv, v: &uv },
                &vpx::EncodeOptions { force_keyframe: i == 0 },
            )
            .expect("encode");
            while let Some(f) = enc.next_frame() {
                frames.push(f.data().to_vec());
            }
        }
        frames
    }

    #[test]
    fn decoding_frame_after_frame_needs_no_manual_drain() {
        let frames = encoded_frames(3);
        assert!(frames.len() >= 3, "expected at least three encoded frames");
        let mut dec = VpxDecoder::new().expect("decoder");
        for f in &frames {
            dec.decode(f).expect("decode");
        }
    }
}
