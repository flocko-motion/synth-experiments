use std::io::{self, Write};

/// 24-bit PCM WAV header. `frames: None` writes the 0xFFFFFFFF "unknown
/// length" sizes; streaming readers read such files until EOF (sox needs
/// `--ignore-length`).
pub fn write_header(w: &mut impl Write, sample_rate: u32, channels: u16, frames: Option<u64>) -> io::Result<()> {
    let block_align = channels as u32 * 3;
    let data_len = frames.map_or(u32::MAX, |n| (n * block_align as u64).min(u32::MAX as u64 - 36) as u32);
    w.write_all(b"RIFF")?;
    w.write_all(&data_len.saturating_add(36).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * block_align).to_le_bytes())?;
    w.write_all(&(block_align as u16).to_le_bytes())?;
    w.write_all(&24u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())
}

pub fn write_samples(w: &mut impl Write, samples: &[f32], scratch: &mut Vec<u8>) -> io::Result<()> {
    scratch.clear();
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) as f64 * 8_388_607.0).round() as i32;
        scratch.extend_from_slice(&v.to_le_bytes()[..3]);
    }
    w.write_all(scratch)
}
