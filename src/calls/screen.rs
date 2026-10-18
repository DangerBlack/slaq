//! Receiving a shared screen.
//!
//! In a huddle the screen is not a protocol of its own: whoever shares joins
//! the meeting a second time as an attendee whose id ends in `#content` and
//! publishes a video stream, on the same connection as everything else. So
//! all that is needed here is to turn the RTP that arrives back into
//! pictures.
//!
//! Two steps, kept apart because only the second needs a codec: the packets
//! are reassembled into frames ([`Frames`]), and a frame is decoded into
//! pixels ([`Decoder`]).

use anyhow::Result;

/// A picture ready to be shown, in RGBA.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Reassembles the RTP packets of a video stream into whole frames.
///
/// A frame travels in several packets and ends with the marker bit. A packet
/// lost in the middle makes the frame unusable, and a half frame handed to
/// the decoder does not only come out wrong — it corrupts what follows,
/// because the frames after it are encoded as differences from this one. So a
/// gap in the sequence numbers throws away what has been collected and waits
/// for the next frame.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
    /// The sequence number of the last packet taken, to see the gaps.
    last_seq: Option<u16>,
    /// A frame already lost: we keep dropping until the next one starts.
    broken: bool,
}

impl Frames {
    /// Adds a packet. Returns the frame when it is complete.
    pub fn push(&mut self, seq: u16, marker: bool, payload: &[u8]) -> Option<Vec<u8>> {
        // Sequence numbers wrap around at 16 bits: the difference has to be
        // computed in that arithmetic, or every wrap would look like an
        // enormous gap.
        if let Some(prev) = self.last_seq {
            if seq.wrapping_sub(prev) != 1 {
                self.broken = true;
                self.buf.clear();
            }
        }
        self.last_seq = Some(seq);

        if self.broken {
            // The end of the broken frame: from the next packet we can start
            // again.
            if marker {
                self.broken = false;
                self.buf.clear();
            }
            return None;
        }

        self.buf.extend_from_slice(payload);
        if !marker {
            return None;
        }
        let frame = std::mem::take(&mut self.buf);
        if frame.is_empty() { None } else { Some(frame) }
    }

    /// Whether we are waiting for a frame after losing something.
    pub fn recovering(&self) -> bool {
        self.broken
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.last_seq = None;
        self.broken = false;
    }
}

/// Turns encoded frames into pictures.
///
/// It is the only place that knows about a codec: everything above it works
/// on bytes, so changing decoder — or one day using a Rust one, when there is
/// a credible one — touches this file alone.
pub enum Decoder {
    #[cfg(feature = "video")]
    Av1(Av1Decoder),
    #[cfg(feature = "vp8")]
    Vp8(VpxDecoder),
    /// Built without the codecs: the window says who is sharing and no more.
    None,
}

impl Decoder {
    /// Picks the decoder from the codec that is actually arriving.
    ///
    /// Slack shares the screen in AV1, but a different client or platform may
    /// well settle on something else, so the choice is made from the wire and
    /// not assumed.
    #[allow(unused_variables)]
    pub fn new(mime: &str) -> Result<Self> {
        #[cfg(feature = "video")]
        {
            match mime {
                "video/AV1" => Ok(Self::Av1(Av1Decoder::new()?)),
                #[cfg(feature = "vp8")]
                "video/VP8" => Ok(Self::Vp8(VpxDecoder::new()?)),
                other => anyhow::bail!("codec {other} not yet decodable"),
            }
        }
        #[cfg(not(feature = "video"))]
        {
            anyhow::bail!("build without the `video` feature: a shared screen cannot be shown")
        }
    }

    /// Decodes a frame. `None` when the frame carries no picture of its own.
    #[allow(unused_variables)]
    pub fn decode(&mut self, frame: &[u8]) -> Result<Option<Picture>> {
        match self {
            #[cfg(feature = "video")]
            Self::Av1(d) => d.decode(frame),
            #[cfg(feature = "vp8")]
            Self::Vp8(d) => d.decode(frame),
            Self::None => Ok(None),
        }
    }
}

#[cfg(feature = "video")]
pub use av1::Av1Decoder;
#[cfg(feature = "vp8")]
pub use vpx::VpxDecoder;

#[cfg(feature = "video")]
mod av1;
#[cfg(feature = "vp8")]
mod vpx;

#[cfg(all(target_os = "linux", feature = "vp8"))]
pub mod shm;
#[cfg(feature = "vp8")]
pub mod capture;

pub mod av1_rtp;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_in_one_packet_comes_out_whole() {
        let mut f = Frames::default();
        assert_eq!(f.push(1, true, b"abc"), Some(b"abc".to_vec()));
    }

    #[test]
    fn a_frame_in_several_packets_is_joined_in_order() {
        let mut f = Frames::default();
        assert_eq!(f.push(10, false, b"ab"), None);
        assert_eq!(f.push(11, false, b"cd"), None);
        assert_eq!(f.push(12, true, b"ef"), Some(b"abcdef".to_vec()));
    }

    #[test]
    fn a_lost_packet_throws_the_frame_away() {
        // Half a frame does not only come out wrong: it corrupts the ones
        // after it, which are encoded as differences from this one.
        let mut f = Frames::default();
        f.push(1, false, b"ab");
        assert_eq!(f.push(3, true, b"ef"), None, "the hole must be noticed");
        assert!(!f.recovering(), "the marker closes the broken frame");
    }

    #[test]
    fn after_a_loss_the_next_frame_is_whole() {
        let mut f = Frames::default();
        f.push(1, false, b"ab");
        f.push(3, true, b"ef");
        assert_eq!(f.push(4, false, b"gh"), None);
        assert_eq!(f.push(5, true, b"ij"), Some(b"ghij".to_vec()));
    }

    #[test]
    fn the_sequence_wrapping_round_is_not_a_gap() {
        // 65535 -> 0 is the normal case, not a loss: computing the difference
        // as an ordinary subtraction would throw away a frame at every wrap.
        let mut f = Frames::default();
        f.push(65535, false, b"ab");
        assert_eq!(f.push(0, true, b"cd"), Some(b"abcd".to_vec()));
    }

    #[test]
    fn an_empty_frame_is_not_a_frame() {
        let mut f = Frames::default();
        assert_eq!(f.push(1, true, b""), None);
    }
}
