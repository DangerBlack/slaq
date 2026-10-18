//! AV1 decoding, with rav1d.
//!
//! Slack shares screens in AV1 — its own log says so
//! (`[HUDDLE-VIDEO-CODECS] Setting preferred content share video codec to
//! AV1`) — and for AV1 there is a decoder in Rust: `rav1d`, the port of dav1d
//! by memorysafety.org, BSD-2. No C toolchain, no system library, and a
//! couple of minutes to build against the quarter of an hour libvpx costs.
//!
//! What it exposes is dav1d's C interface, so the calls are `unsafe` and the
//! wrapping is ours. The invariants are dav1d's: a context from `dav1d_open`
//! is used until `dav1d_close`, data handed to `dav1d_send_data` belongs to
//! the decoder, and every picture obtained has to be released.

use anyhow::{bail, Result};
use std::ptr::NonNull;
use tracing::{debug, warn};

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_default_settings, dav1d_get_picture, dav1d_open,
    dav1d_picture_unref, dav1d_send_data,
};

use super::Picture;

pub struct Av1Decoder {
    ctx: Option<Dav1dContext>,
}

// The context is only ever touched from the task that owns the decoder.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new() -> Result<Self> {
        // SAFETY: `settings` is a valid, writable allocation, and
        // `dav1d_default_settings` only writes to it.
        let settings = unsafe {
            let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::zeroed();
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: both pointers are to live locals, as `dav1d_open` requires.
        let res = unsafe {
            let mut s = settings;
            dav1d_open(
                NonNull::new(&mut ctx as *mut _),
                NonNull::new(&mut s as *mut _),
            )
        };
        if res.0 < 0 {
            bail!("rav1d not initialized: code {}", res.0);
        }
        if ctx.is_none() {
            bail!("rav1d did not return a context");
        }
        Ok(Self { ctx })
    }

    /// Decodes a temporal unit.
    ///
    /// `None` is an ordinary outcome: the decoder keeps frames back, and the
    /// picture comes out on one of the calls that follow.
    pub fn decode(&mut self, unit: &[u8]) -> Result<Option<Picture>> {
        if unit.is_empty() {
            return Ok(None);
        }
        // SAFETY: `data` is zeroed, which is a valid empty `Dav1dData`, and
        // `dav1d_data_create` gives back a buffer of exactly `unit.len()`
        // that we fill before handing it over.
        unsafe {
            let mut data = std::mem::MaybeUninit::<Dav1dData>::zeroed().assume_init();
            let buf = dav1d_data_create(NonNull::new(&mut data as *mut _), unit.len());
            if buf.is_null() {
                bail!("rav1d: buffer not allocated");
            }
            std::ptr::copy_nonoverlapping(unit.as_ptr(), buf, unit.len());
            let res = dav1d_send_data(self.ctx, NonNull::new(&mut data as *mut _));
            // EAGAIN means the decoder is full: the data stays pending and is
            // consumed once pictures are drained, so it is not an error.
            if res.0 < 0 && res.0 != -11 {
                bail!("rav1d: data rejected, code {}", res.0);
            }

            let mut pic = std::mem::MaybeUninit::<Dav1dPicture>::zeroed().assume_init();
            let res = dav1d_get_picture(self.ctx, NonNull::new(&mut pic as *mut _));
            if res.0 == -11 {
                debug!("rav1d: no picture (EAGAIN)");
                return Ok(None);
            }
            if res.0 < 0 {
                bail!("rav1d: decode error, code {}", res.0);
            }
            let bpc = pic.p.bpc;
            let out = to_rgba(&pic);
            dav1d_picture_unref(NonNull::new(&mut pic as *mut _));
            if out.is_none() {
                warn!("rav1d: picture not convertible (bpc={bpc})");
            }
            Ok(out)
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        // SAFETY: the context comes from `dav1d_open` and is closed once.
        unsafe { dav1d_close(NonNull::new(&mut self.ctx as *mut _)) };
    }
}

/// Converts a decoded picture into RGBA.
///
/// Only 8 bits per component and 4:2:0, which is what a screen share is: the
/// other layouts would need their own chrominance arithmetic and have never
/// turned up here.
///
/// # Safety
///
/// `pic` must be a picture returned by `dav1d_get_picture` and not yet
/// released.
unsafe fn to_rgba(pic: &Dav1dPicture) -> Option<Picture> {
    let (w, h) = (pic.p.w.max(0) as usize, pic.p.h.max(0) as usize);
    if w == 0 || h == 0 || pic.p.bpc != 8 {
        return None;
    }
    let y_plane = pic.data[0]?.as_ptr() as *const u8;
    let u_plane = pic.data[1]?.as_ptr() as *const u8;
    let v_plane = pic.data[2]?.as_ptr() as *const u8;
    let ys = pic.stride[0] as usize;
    let cs = pic.stride[1] as usize;

    let mut rgba = vec![0u8; w * h * 4];
    for row in 0..h {
        for col in 0..w {
            // SAFETY: inside the declared width and height, with the strides
            // the decoder gives.
            let y = unsafe { *y_plane.add(row * ys + col) } as f32;
            // The chrominance planes are half the size in both directions.
            let u = unsafe { *u_plane.add((row / 2) * cs + col / 2) } as f32 - 128.0;
            let v = unsafe { *v_plane.add((row / 2) * cs + col / 2) } as f32 - 128.0;
            let y = (y - 16.0) * 1.164;

            let o = (row * w + col) * 4;
            rgba[o] = clamp(y + 1.596 * v);
            rgba[o + 1] = clamp(y - 0.392 * u - 0.813 * v);
            rgba[o + 2] = clamp(y + 2.017 * u);
            rgba[o + 3] = 255;
        }
    }
    Some(Picture { width: w as u32, height: h as u32, rgba })
}

fn clamp(v: f32) -> u8 {
    v.clamp(0.0, 255.0) as u8
}
