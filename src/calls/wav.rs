//! Reading and writing 16-bit PCM WAV, with no new dependencies.
//!
//! It serves the diagnostic `aec-probe`: it reads a signal to play back and
//! writes the raw capture as it is, to measure how much the AEC removes. It is
//! not a complete WAV decoder: only the case that matters (RIFF/WAVE PCM 16
//! bit) and, for writing, a single fixed 48 kHz mono format.

use anyhow::{Context, Result};
use std::path::Path;

/// Reads any RIFF PCM 16-bit WAV (1 or 2 channels, any sample rate), brings it
/// to mono and resamples it to 48 kHz.
///
/// The resampling is linear: it is a test signal, nothing more is needed. The
/// result is the sequence of 48 kHz i16 samples that the audio path expects.
pub fn read_pcm16_mono_48k(path: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {} failed", path.display()))?;

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("{} is not a RIFF/WAVE file", path.display());
    }

    // The chunks follow one another from position 12, each with a 4-byte id and
    // a little-endian size; after the body a padding byte is skipped if the
    // size is odd.
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size =
            u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());

        match id {
            b"fmt " if body_end - body_start >= 16 => {
                let b = &bytes[body_start..body_end];
                let audio_format = u16::from_le_bytes([b[0], b[1]]);
                let channels = u16::from_le_bytes([b[2], b[3]]);
                let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                let bits = u16::from_le_bytes([b[14], b[15]]);
                fmt = Some((audio_format, channels, rate, bits));
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }

        pos = body_start + size + (size & 1);
    }

    let (audio_format, channels, rate, bits) = fmt.context("missing `fmt ` chunk")?;
    // 1 = classic PCM; 0xFFFE = WAVE_FORMAT_EXTENSIBLE (PCM subtype, which is
    // assumed here: it is the only case the test files use).
    anyhow::ensure!(
        audio_format == 1 || audio_format == 0xFFFE,
        "WAV format not PCM: {audio_format}"
    );
    anyhow::ensure!(bits == 16, "only 16-bit PCM, not {bits}");
    anyhow::ensure!(channels >= 1, "invalid number of channels: {channels}");

    let data = data.context("missing `data` chunk")?;
    let n = data.len() / 2;
    let interleaved: Vec<i16> = (0..n)
        .map(|i| i16::from_le_bytes([data[i * 2], data[i * 2 + 1]]))
        .collect();

    let mono: Vec<i16> = if channels == 1 {
        interleaved
    } else {
        interleaved
            .chunks_exact(channels as usize)
            .map(|c| {
                let sum: i32 = c.iter().map(|&s| s as i32).sum();
                (sum / channels as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16
            })
            .collect()
    };

    if rate == 48_000 {
        return Ok(mono);
    }
    Ok(resample_linear(&mono, rate, 48_000))
}

/// Linear resampling to a fixed output rate.
fn resample_linear(input: &[i16], from: u32, to: u32) -> Vec<i16> {
    if input.is_empty() || from == to {
        return input.to_vec();
    }
    // Input samples consumed per output sample.
    let step = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / step).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * step;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let a = input.get(idx).copied().unwrap_or(0) as f64;
        let b = input.get(idx + 1).copied().unwrap_or(a as i16) as f64;
        out.push((a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

/// Incremental writer for 48 kHz mono 16-bit PCM WAV.
///
/// On creation it writes a header with zero sizes; samples are appended with
/// [`WavWriter::write`] and the sizes are corrected at the end
/// ([`WavWriter::finish`] or on `Drop`), so the file stays valid whatever its
/// duration.
pub struct WavWriter {
    file: std::fs::File,
    data_len: u32,
}

impl WavWriter {
    /// Creates (or truncates) the file and writes the provisional header.
    pub fn create(path: &Path) -> Result<Self> {
        let file = std::fs::File::create(path)
            .with_context(|| format!("creating {} failed", path.display()))?;
        let mut w = WavWriter { file, data_len: 0 };
        w.write_header(0)?;
        Ok(w)
    }

    /// Appends samples (little-endian i16) to the data.
    pub fn write(&mut self, samples: &[i16]) -> Result<()> {
        use std::io::Write as _;
        let mut buf = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&buf)?;
        self.data_len = self.data_len.saturating_add(buf.len() as u32);
        Ok(())
    }

    /// Rewrites the header with the real sizes and forces the write to disk.
    pub fn finish(&mut self) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write as _};
        self.file.seek(SeekFrom::Start(0))?;
        self.write_header(self.data_len)?;
        self.file.flush()?;
        Ok(())
    }

    /// RIFF/fmt/data header for 48 kHz mono 16 bit, with `data_len` bytes of data.
    fn write_header(&mut self, data_len: u32) -> Result<()> {
        use std::io::Write as _;
        const BYTE_RATE: u32 = 48_000 * 2; // 48 kHz * 2 bytes per sample
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1u16.to_le_bytes()); // mono
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&BYTE_RATE.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes()); // block align
        h.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        self.file.write_all(&h)?;
        Ok(())
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        // An error here has no caller to bubble up to: the header save is
        // attempted, which is the normal case, and the rest is ignored.
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a few hundred samples and reads them back: the values must come
    /// back identical and the size header must be consistent.
    #[test]
    fn round_trip_wav_writer_reader() {
        let path = std::env::temp_dir().join(format!(
            "slaq-wav-test-{}-{:?}.wav",
            std::process::id(),
            std::thread::current().id()
        ));

        let samples: Vec<i16> = (0..500)
            .map(|i| ((i as i32 * 137) % 2000 - 1000) as i16)
            .collect();

        {
            let mut w = WavWriter::create(&path).expect("create");
            w.write(&samples).expect("write");
            w.finish().expect("finish");
        }

        let back = read_pcm16_mono_48k(&path).expect("read back");
        assert_eq!(back, samples, "the samples read back do not match");

        // Header: `RIFF` and `WAVE` in place, `data` with the right size.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[0..4], b"RIFF");
        assert_eq!(&raw[8..12], b"WAVE");
        assert_eq!(&raw[36..40], b"data");
        let data_len = u32::from_le_bytes(raw[40..44].try_into().unwrap());
        assert_eq!(data_len as usize, samples.len() * 2);
        assert_eq!(raw.len() as u32, 44 + data_len);

        std::fs::remove_file(&path).ok();
    }

    /// A 44.1 kHz stereo file is brought to 48 kHz mono with the right
    /// duration (within one sample).
    #[test]
    fn resample_e_downmix() {
        let path =
            std::env::temp_dir().join(format!("slaq-wav-resample-{}.wav", std::process::id()));
        let n = 44_100usize; // 1 second stereo at 44.1 kHz
        let mut raw = Vec::new();
        raw.extend_from_slice(b"RIFF");
        raw.extend_from_slice(&0u32.to_le_bytes()); // patched later
        raw.extend_from_slice(b"WAVE");
        raw.extend_from_slice(b"fmt ");
        raw.extend_from_slice(&16u32.to_le_bytes());
        raw.extend_from_slice(&1u16.to_le_bytes());
        raw.extend_from_slice(&2u16.to_le_bytes());
        raw.extend_from_slice(&44_100u32.to_le_bytes());
        raw.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        raw.extend_from_slice(&4u16.to_le_bytes());
        raw.extend_from_slice(&16u16.to_le_bytes());
        raw.extend_from_slice(b"data");
        let data_len = (n * 2 * 2) as u32;
        raw.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..n {
            let s = ((i as i32 * 71) % 3000 - 1500) as i16;
            raw.extend_from_slice(&s.to_le_bytes()); // L
            raw.extend_from_slice(&s.to_le_bytes()); // R
        }
        let riff_len = (raw.len() - 8) as u32;
        raw[4..8].copy_from_slice(&riff_len.to_le_bytes());

        std::fs::write(&path, &raw).unwrap();
        let out = read_pcm16_mono_48k(&path).unwrap();
        std::fs::remove_file(&path).ok();

        // The duration must stay ~1 s: 48000 samples, within one.
        assert!(
            (out.len() as i64 - 48_000).abs() <= 1,
            "expected duration 48000 samples, got {}",
            out.len()
        );
    }
}
