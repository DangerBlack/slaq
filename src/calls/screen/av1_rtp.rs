//! Rebuilding AV1 frames from RTP.
//!
//! `webrtc-rs` can packetise AV1 but not unpacketise it, so this is ours. The
//! format is the one in the AV1 RTP specification, and it has two quirks that
//! have to be handled or the decoder sees rubbish:
//!
//! - a packet carries a sequence of **OBU elements**, preceded by a one-byte
//!   aggregation header. The `W` field says how many there are: when it is
//!   zero every element carries its own length as a leb128, and when it is
//!   not, the last element has no length and runs to the end of the packet;
//! - an OBU can be **split across packets**: `Z` says this packet starts with
//!   the continuation of the previous one, `Y` that the last element will
//!   continue in the next.
//!
//! And one conversion: on the wire the OBUs travel **without** their size
//! field, because the length is already in the element. A decoder expects the
//! low-overhead format, where every OBU carries its own size. So the field is
//! put back, which means rewriting one bit of the header too.

/// Reads a leb128. Returns the value and how many bytes it took.
fn leb128(b: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (i, byte) in b.iter().take(8).enumerate() {
        value |= ((byte & 0x7f) as usize) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Writes a leb128.
fn put_leb128(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Appends an OBU to a temporal unit, giving it back its size field.
///
/// `obu` is header plus payload, without the size. The header's bit 1 says
/// whether a size follows: on the wire it is zero, and a decoder reading the
/// low-overhead format needs it to be one.
fn push_obu(out: &mut Vec<u8>, obu: &[u8]) {
    let Some(&first) = obu.first() else { return };
    // Bit 2 of the header says a second header byte follows.
    let header_len = if first & 0b0000_0100 != 0 { 2 } else { 1 };
    if obu.len() < header_len {
        return;
    }
    out.push(first | 0b0000_0010);
    out.extend_from_slice(&obu[1..header_len]);
    put_leb128(out, obu.len() - header_len);
    out.extend_from_slice(&obu[header_len..]);
}

/// Reassembles the temporal units of an AV1 stream.
#[derive(Default)]
pub struct Av1Frames {
    /// The temporal unit being built.
    unit: Vec<u8>,
    /// An OBU split across packets, still incomplete.
    partial: Vec<u8>,
    last_seq: Option<u16>,
    /// Something was lost: we drop until the next temporal unit.
    broken: bool,
}

impl Av1Frames {
    /// Adds a packet. Returns the temporal unit when the marker closes it.
    pub fn push(&mut self, seq: u16, marker: bool, payload: &[u8]) -> Option<Vec<u8>> {
        if let Some(prev) = self.last_seq {
            // In 16-bit arithmetic, or every wrap of the counter would look
            // like a loss.
            if seq.wrapping_sub(prev) != 1 {
                self.broken = true;
            }
        }
        self.last_seq = Some(seq);

        if self.broken {
            if marker {
                // The end of the damaged unit: from the next one we start
                // again.
                self.reset();
            }
            return None;
        }

        if self.parse(payload).is_none() {
            self.broken = true;
            return None;
        }

        if !marker {
            return None;
        }
        // An OBU left open at the end of a temporal unit is a damaged unit.
        if !self.partial.is_empty() {
            self.reset();
            return None;
        }
        let unit = std::mem::take(&mut self.unit);
        self.reset();
        if unit.is_empty() { None } else { Some(unit) }
    }

    pub fn reset(&mut self) {
        self.unit.clear();
        self.partial.clear();
        self.broken = false;
    }

    /// Reads the elements of one packet. `None` if it is malformed.
    fn parse(&mut self, payload: &[u8]) -> Option<()> {
        let (&header, mut rest) = payload.split_first()?;
        let continues_previous = header & 0b1000_0000 != 0;
        let continues_after = header & 0b0100_0000 != 0;
        let count = (header & 0b0011_0000) >> 4;

        if !continues_previous && !self.partial.is_empty() {
            // The previous OBU was left open and this packet does not take it
            // up: there is a hole.
            return None;
        }

        let mut index = 0u8;
        while !rest.is_empty() {
            index += 1;
            // With a declared count the last element has no length and takes
            // what is left; with zero every element declares its own.
            let (element, remainder) = if count != 0 && index == count {
                (rest, &[][..])
            } else {
                let (len, used) = leb128(rest)?;
                let body = rest.get(used..used + len)?;
                (body, &rest[used + len..])
            };
            rest = remainder;

            let first_element = index == 1;
            let last_element = rest.is_empty();

            if first_element && continues_previous {
                self.partial.extend_from_slice(element);
            } else {
                // A new element: whatever was pending is complete.
                self.flush_partial();
                self.partial.extend_from_slice(element);
            }

            if !(last_element && continues_after) {
                self.flush_partial();
            }
        }
        Some(())
    }

    fn flush_partial(&mut self) {
        if self.partial.is_empty() {
            return;
        }
        let obu = std::mem::take(&mut self.partial);
        push_obu(&mut self.unit, &obu);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An OBU of `n` bytes of payload, header included, without size field.
    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![kind << 3];
        v.extend_from_slice(payload);
        v
    }

    /// A packet: aggregation header plus elements with their lengths.
    fn packet(z: bool, y: bool, elements: &[&[u8]]) -> Vec<u8> {
        let mut head = 0u8;
        if z {
            head |= 0b1000_0000;
        }
        if y {
            head |= 0b0100_0000;
        }
        let mut v = vec![head];
        for e in elements {
            put_leb128(&mut v, e.len());
            v.extend_from_slice(e);
        }
        v
    }

    #[test]
    fn leb128_round_trip() {
        for n in [0usize, 1, 127, 128, 300, 70000] {
            let mut v = Vec::new();
            put_leb128(&mut v, n);
            assert_eq!(leb128(&v), Some((n, v.len())), "{n}");
        }
    }

    #[test]
    fn an_obu_gets_its_size_back() {
        // On the wire there is no size; a decoder wants it, and wants the bit
        // in the header that says so.
        let mut out = Vec::new();
        push_obu(&mut out, &obu(6, b"xyz"));
        assert_eq!(out, vec![(6 << 3) | 0b10, 3, b'x', b'y', b'z']);
    }

    #[test]
    fn an_obu_with_the_extension_byte_keeps_it() {
        let mut src = vec![(6u8 << 3) | 0b100, 0xAB];
        src.extend_from_slice(b"xy");
        let mut out = Vec::new();
        push_obu(&mut out, &src);
        assert_eq!(out, vec![(6 << 3) | 0b110, 0xAB, 2, b'x', b'y']);
    }

    #[test]
    fn one_packet_one_temporal_unit() {
        let mut f = Av1Frames::default();
        let p = packet(false, false, &[&obu(6, b"ab")]);
        let unit = f.push(1, true, &p).expect("unit");
        assert_eq!(unit, vec![(6 << 3) | 0b10, 2, b'a', b'b']);
    }

    #[test]
    fn several_obus_in_one_packet() {
        let mut f = Av1Frames::default();
        let p = packet(false, false, &[&obu(1, b"a"), &obu(6, b"bc")]);
        let unit = f.push(1, true, &p).expect("unit");
        assert_eq!(
            unit,
            vec![(1 << 3) | 0b10, 1, b'a', (6 << 3) | 0b10, 2, b'b', b'c']
        );
    }

    #[test]
    fn an_obu_split_across_two_packets_is_joined() {
        // Y on the first, Z on the second: the two halves are one OBU, and
        // joining them wrongly would give the decoder two broken ones.
        let mut f = Av1Frames::default();
        let mut first = obu(6, b"ab");
        let tail = first.split_off(2);
        assert_eq!(f.push(1, false, &packet(false, true, &[&first])), None);
        let unit = f.push(2, true, &packet(true, false, &[&tail])).expect("unit");
        assert_eq!(unit, vec![(6 << 3) | 0b10, 2, b'a', b'b']);
    }

    #[test]
    fn a_lost_packet_throws_the_unit_away() {
        let mut f = Av1Frames::default();
        f.push(1, false, &packet(false, true, &[&obu(6, b"ab")]));
        assert_eq!(f.push(3, true, &packet(true, false, &[b"c"])), None);
    }

    #[test]
    fn after_a_loss_the_next_unit_comes_out() {
        let mut f = Av1Frames::default();
        f.push(1, false, &packet(false, true, &[&obu(6, b"ab")]));
        f.push(3, true, &packet(true, false, &[b"c"]));
        let unit = f.push(4, true, &packet(false, false, &[&obu(6, b"zz")]));
        assert_eq!(unit, Some(vec![(6 << 3) | 0b10, 2, b'z', b'z']));
    }

    #[test]
    fn a_unit_left_open_is_not_delivered() {
        // The marker closes the temporal unit: an OBU still open at that
        // point means something was lost.
        let mut f = Av1Frames::default();
        assert_eq!(f.push(1, true, &packet(false, true, &[&obu(6, b"ab")])), None);
    }

    #[test]
    fn a_truncated_packet_does_not_panic() {
        let mut f = Av1Frames::default();
        // Length says 10, two bytes follow.
        let p = vec![0u8, 10, b'a', b'b'];
        assert_eq!(f.push(1, true, &p), None);
    }

    #[test]
    fn an_empty_packet_is_ignored() {
        let mut f = Av1Frames::default();
        assert_eq!(f.push(1, true, &[]), None);
    }
}
