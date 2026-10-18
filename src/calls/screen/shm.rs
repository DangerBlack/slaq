//! Isolated X11 MIT-SHM screen capture prototype.
//!
//! The production path (`calls::video::spawn_capture`) uses the `screenshots`
//! crate, which on X11 opens a new connection per frame and calls the core
//! `GetImage` request, so every pixel crosses the X socket. This module is a
//! de-risking prototype: it opens one connection, attaches one shared-memory
//! segment, and asks the server to write pixels into it, then measures the
//! result. It is deliberately not wired into the capture loop yet.
//!
//! Geometry follows `screenshots`/`display-info` exactly: logical coordinates
//! and sizes are multiplied by `scale_factor` to get physical pixels.

use anyhow::{anyhow, bail, Result};

/// A SysV shared-memory segment and its attached client-side mapping.
struct ShmSegment {
    id: i32,
    addr: *mut libc::c_void,
    size: usize,
}

impl ShmSegment {
    fn new(size: usize) -> Result<Self> {
        if size == 0 {
            bail!("shared-memory segment has zero size");
        }
        let id = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o777) };
        if id < 0 {
            bail!("shmget({size}) failed: {}", std::io::Error::last_os_error());
        }
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if addr as isize == -1 || addr.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
            }
            bail!("shmat failed: {err}");
        }
        Ok(ShmSegment { id, addr, size })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.addr as *const u8, self.size) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.addr);
            libc::shmctl(self.id, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}

/// Captures a single monitor through MIT-SHM.
///
/// One X connection and one shared segment live for the lifetime of the
/// capturer; `capture` sends one `Shm::GetImage` per frame and returns a slice
/// into the segment, with no per-frame connection and no per-frame buffer
/// allocation.
pub struct ShmCapturer {
    conn: xcb::Connection,
    root: xcb::x::Window,
    segment: xcb::shm::Seg,
    shm: ShmSegment,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    frame_len: usize,
}

impl ShmCapturer {
    /// Opens the display, verifies MIT-SHM and attaches a segment sized for
    /// the physical geometry of `display`.
    pub fn new(display: &screenshots::DisplayInfo) -> Result<Self> {
        let (conn, index) = xcb::Connection::connect(None)
            .map_err(|e| anyhow!("xcb connect failed: {e}"))?;

        // Confirms MIT-SHM is present: if the extension is missing, the reply
        // never arrives and this errors out.
        conn.wait_for_reply(conn.send_request(&xcb::shm::QueryVersion {}))
            .map_err(|e| anyhow!("MIT-SHM unavailable (QueryVersion failed): {e}"))?;

        let root = {
            let setup = conn.get_setup();
            let screen = setup
                .roots()
                .nth(index as usize)
                .ok_or_else(|| anyhow!("xcb screen {index} not found"))?;
            screen.root()
        };

        let scale = display.scale_factor;
        let x = (display.x as f32 * scale) as i16;
        let y = (display.y as f32 * scale) as i16;
        let width = (display.width as f32 * scale) as u32;
        let height = (display.height as f32 * scale) as u32;
        if width == 0 || height == 0 {
            bail!("display geometry is {width}x{height}");
        }
        if width > u16::MAX as u32 || height > u16::MAX as u32 {
            bail!("display geometry {width}x{height} exceeds the X protocol limits");
        }

        let frame_len = width as usize * height as usize * 4;
        let segment: xcb::shm::Seg = conn.generate_id();
        let shm = ShmSegment::new(frame_len)?;
        conn.send_and_check_request(&xcb::shm::Attach {
            shmseg: segment,
            shmid: shm.id as u32,
            read_only: false,
        })
        .map_err(|e| anyhow!("MIT-SHM Attach failed: {e}"))?;

        Ok(ShmCapturer {
            conn,
            root,
            segment,
            shm,
            x,
            y,
            width: width as u16,
            height: height as u16,
            frame_len,
        })
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// The most recently captured frame, without capturing again.
    ///
    /// Valid only after a successful [`capture`]; the segment always holds a
    /// full frame, so this is the whole buffer.
    pub fn frame(&self) -> &[u8] {
        &self.shm.as_slice()[..self.frame_len]
    }

    /// Captures one frame and returns its BGRA bytes.
    ///
    /// The server writes the pixels into the shared segment before it writes
    /// the `Shm::GetImage` reply (`ProcShmGetImage` calls the screen's
    /// `GetImage` and only then `WriteToClient`), so waiting for the reply is
    /// the completion barrier and the segment is safe to read here. This
    /// matches what FFmpeg's `x11grab` does. There is no `ShmCompletion` event
    /// on this path, because the server only emits one for `Shm::PutImage`
    /// with `send_event` set.
    pub fn capture(&mut self) -> Result<&[u8]> {
        let cookie = self.conn.send_request(&xcb::shm::GetImage {
            drawable: xcb::x::Drawable::Window(self.root),
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            plane_mask: u32::MAX,
            format: xcb::x::ImageFormat::ZPixmap as u8,
            shmseg: self.segment,
            offset: 0,
        });
        let reply = self
            .conn
            .wait_for_reply(cookie)
            .map_err(|e| anyhow!("MIT-SHM GetImage failed: {e}"))?;

        let written = reply.size() as usize;
        if written > self.shm.size {
            bail!(
                "server wrote {written} bytes into a {} byte segment",
                self.shm.size
            );
        }
        let len = written.min(self.frame_len);
        Ok(&self.shm.as_slice()[..len])
    }
}

impl Drop for ShmCapturer {
    fn drop(&mut self) {
        let _ = self
            .conn
            .send_and_check_request(&xcb::shm::Detach { shmseg: self.segment });
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Nearest-neighbour downscale into a caller-provided buffer, so the
    /// measurement is of the capture pipeline rather than of an unoptimised
    /// dependency. The real integration converts to I420 in one pass instead.
    fn downscale_bgra(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
        let step = (sw << 16) / dw;
        for dy in 0..dh {
            let sy = dy * sh / dh;
            let src_row = &src[sy * sw * 4..];
            let dst_row = &mut dst[dy * dw * 4..(dy + 1) * dw * 4];
            let mut pos = 0usize;
            for chunk in dst_row.chunks_exact_mut(4) {
                let sx = pos >> 16;
                chunk.copy_from_slice(&src_row[sx * 4..sx * 4 + 4]);
                pos += step;
            }
        }
    }

    #[test]
    #[ignore]
    fn measure_shm_capture() {
        let screens = screenshots::Screen::all().expect("screens");
        let screen = screens
            .iter()
            .find(|s| s.display_info.is_primary)
            .or_else(|| screens.first())
            .expect("at least one screen");
        let info = screen.display_info;

        let mut cap = ShmCapturer::new(&info).expect("ShmCapturer");
        let (w, h) = (cap.width() as usize, cap.height() as usize);
        let native = format!("{}x{}", cap.width(), cap.height());

        std::hint::black_box(cap.capture().expect("warmup"));

        {
            let frame = cap.capture().expect("capture");
            assert_eq!(frame.len(), w * h * 4, "short SHM frame");
            let nonzero = frame.iter().filter(|&&b| b != 0).count();
            assert!(nonzero > frame.len() / 100, "SHM frame looks empty");
            std::hint::black_box(frame);
        }

        let n = 120usize;
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let frame = Instant::now();
            std::hint::black_box(cap.capture().expect("capture"));
            samples.push(frame.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "shm capture {native}: min {:.2} / avg {:.2} / p50 {:.2} / p90 {:.2} / max {:.2} ms/frame",
            samples[0],
            samples.iter().sum::<f64>() / n as f64,
            samples[n / 2],
            samples[n * 9 / 10],
            samples[n - 1],
        );

        let (dw, dh) = (1920usize, 1080usize);
        let mut dst = vec![0u8; dw * dh * 4];
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let frame = Instant::now();
            let bgra = cap.capture().expect("capture");
            downscale_bgra(bgra, w, h, &mut dst, dw, dh);
            std::hint::black_box(&dst);
            samples.push(frame.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "shm capture 1920x1080: min {:.2} / avg {:.2} / p50 {:.2} / p90 {:.2} / max {:.2} ms/frame",
            samples[0],
            samples.iter().sum::<f64>() / n as f64,
            samples[n / 2],
            samples[n * 9 / 10],
            samples[n - 1],
        );

        let bgra = cap.capture().expect("capture").to_vec();
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let frame = Instant::now();
            downscale_bgra(&bgra, w, h, &mut dst, dw, dh);
            std::hint::black_box(&dst);
            samples.push(frame.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "shm downscale 1920x1080: avg {:.2} / p50 {:.2} / max {:.2} ms/frame",
            samples.iter().sum::<f64>() / n as f64,
            samples[n / 2],
            samples[n - 1],
        );
    }
}
