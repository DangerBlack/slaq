//! Capture abstraction for screen sharing.
//!
//! Two backends exist. On Linux the SHM backend writes pixels straight into
//! a shared segment and is the fast path; everywhere else, or if the SHM
//! setup fails, the `screenshots` backend is used. `open` picks one and
//! returns it as a boxed trait object. The capturer is created on the capture
//! thread, so it never has to cross a thread boundary.

use anyhow::{anyhow, bail, Context, Result};
use tracing::warn;

/// Byte order of the four bytes of a source pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// `[R, G, B, A]`, as produced by the `screenshots` crate.
    Rgba,
    /// `[B, G, R, A]`, the ZPixmap layout the X server hands back on
    /// little-endian hosts.
    Bgra,
}

/// A screen source that can be captured repeatedly.
pub trait Capture {
    /// Size of a captured frame, in pixels.
    fn dimensions(&self) -> (u32, u32);
    /// Byte order of the slice returned by [`Capture::bytes`].
    fn pixel_order(&self) -> PixelOrder;
    /// Captures one frame into the backend's buffer.
    fn capture(&mut self) -> Result<()>;
    /// The last captured frame, in the order given by [`Capture::pixel_order`].
    fn bytes(&self) -> &[u8];
}

/// Opens the best available backend for `display_id`, falling back to
/// `screenshots` if SHM is unavailable.
pub fn open(display_id: Option<u32>) -> Result<Box<dyn Capture>> {
    let info = select_display(display_id)?;

    #[cfg(target_os = "linux")]
    {
        match ShmCapture::new(&info) {
            Ok(capture) => return Ok(Box::new(capture)),
            Err(e) => warn!("SHM capture unavailable, falling back to screenshots: {e:#}"),
        }
    }

    Ok(Box::new(ScreenshotsCapture::new(&info)?))
}

/// Picks a monitor by id, falling back to the primary one when the requested
/// id is gone, preserving the picker's selection semantics.
fn select_display(display_id: Option<u32>) -> Result<screenshots::DisplayInfo> {
    let screens = screenshots::Screen::all().context("no screen found for capture")?;
    let primary = screens
        .iter()
        .find(|s| s.display_info.is_primary)
        .or_else(|| screens.first())
        .map(|s| s.display_info);

    match display_id {
        Some(want) => match screens.iter().find(|s| s.display_info.id == want) {
            Some(s) => Ok(s.display_info),
            None => match primary {
                Some(p) => {
                    warn!("Display {want} not found: using the primary screen");
                    Ok(p)
                }
                None => bail!("No screen found for capture"),
            },
        },
        None => primary.ok_or_else(|| anyhow!("No screen found for capture")),
    }
}

#[cfg(target_os = "linux")]
struct ShmCapture {
    inner: super::shm::ShmCapturer,
}

#[cfg(target_os = "linux")]
impl ShmCapture {
    fn new(info: &screenshots::DisplayInfo) -> Result<Self> {
        Ok(ShmCapture {
            inner: super::shm::ShmCapturer::new(info)?,
        })
    }
}

#[cfg(target_os = "linux")]
impl Capture for ShmCapture {
    fn dimensions(&self) -> (u32, u32) {
        (self.inner.width() as u32, self.inner.height() as u32)
    }

    fn pixel_order(&self) -> PixelOrder {
        PixelOrder::Bgra
    }

    fn capture(&mut self) -> Result<()> {
        self.inner.capture().map(|_| ())
    }

    fn bytes(&self) -> &[u8] {
        self.inner.frame()
    }
}

struct ScreenshotsCapture {
    screen: screenshots::Screen,
    dims: (u32, u32),
    frame: Option<screenshots::Image>,
}

impl ScreenshotsCapture {
    fn new(info: &screenshots::DisplayInfo) -> Result<Self> {
        let scale = info.scale_factor;
        let dims = (
            (info.width as f32 * scale) as u32,
            (info.height as f32 * scale) as u32,
        );
        Ok(ScreenshotsCapture {
            screen: screenshots::Screen::new(info),
            dims,
            frame: None,
        })
    }
}

impl Capture for ScreenshotsCapture {
    fn dimensions(&self) -> (u32, u32) {
        self.dims
    }

    fn pixel_order(&self) -> PixelOrder {
        PixelOrder::Rgba
    }

    fn capture(&mut self) -> Result<()> {
        let image = self.screen.capture().map_err(|e| anyhow!("{e}"))?;
        self.dims = (image.width(), image.height());
        self.frame = Some(image);
        Ok(())
    }

    fn bytes(&self) -> &[u8] {
        self.frame
            .as_ref()
            .map(|image| image.rgba().as_slice())
            .unwrap_or(&[])
    }
}
