use std::io::{self, Read, Write};

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

#[derive(Clone, Copy, Debug)]
enum SampleFormat {
    I16,
    I24,
    I32,
    F32,
}

/// Streaming WAV reader for effect input: PCM 16/24/32-bit or float 32,
/// any channel count, known or unknown ("streaming") length.
pub struct WavReader<R: Read> {
    r: R,
    pub channels: u16,
    pub sample_rate: u32,
    format: SampleFormat,
    /// Data bytes left, if the header gave a length.
    remaining: Option<u64>,
    bytes: Vec<u8>,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("input is not a usable WAV stream: {msg}"))
}

impl<R: Read> WavReader<R> {
    pub fn new(mut r: R) -> io::Result<WavReader<R>> {
        let mut head = [0u8; 12];
        r.read_exact(&mut head).map_err(|_| invalid("too short"))?;
        if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
            return Err(invalid("no RIFF/WAVE header"));
        }
        let mut fmt: Option<(u16, u32, SampleFormat)> = None;
        loop {
            let mut ch = [0u8; 8];
            r.read_exact(&mut ch).map_err(|_| invalid("no data chunk"))?;
            let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
            match &ch[0..4] {
                b"fmt " => {
                    let mut b = vec![0u8; size as usize + (size as usize & 1)];
                    r.read_exact(&mut b)?;
                    if b.len() < 16 {
                        return Err(invalid("short fmt chunk"));
                    }
                    let mut tag = u16::from_le_bytes([b[0], b[1]]);
                    if tag == 0xFFFE && b.len() >= 26 {
                        tag = u16::from_le_bytes([b[24], b[25]]); // WAVE_FORMAT_EXTENSIBLE subformat
                    }
                    let channels = u16::from_le_bytes([b[2], b[3]]);
                    let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                    let bits = u16::from_le_bytes([b[14], b[15]]);
                    let format = match (tag, bits) {
                        (1, 16) => SampleFormat::I16,
                        (1, 24) => SampleFormat::I24,
                        (1, 32) => SampleFormat::I32,
                        (3, 32) => SampleFormat::F32,
                        _ => return Err(invalid(&format!("unsupported format {tag}/{bits}-bit"))),
                    };
                    if channels == 0 {
                        return Err(invalid("zero channels"));
                    }
                    fmt = Some((channels, rate, format));
                }
                b"data" => {
                    let (channels, sample_rate, format) = fmt.ok_or_else(|| invalid("data before fmt"))?;
                    let remaining = if size == 0 || size == u32::MAX { None } else { Some(size as u64) };
                    return Ok(WavReader { r, channels, sample_rate, format, remaining, bytes: Vec::new() });
                }
                _ => {
                    let skip = size as u64 + (size as u64 & 1);
                    io::copy(&mut (&mut r).take(skip), &mut io::sink())?;
                }
            }
        }
    }

    fn sample_bytes(&self) -> usize {
        match self.format {
            SampleFormat::I16 => 2,
            SampleFormat::I24 => 3,
            SampleFormat::I32 | SampleFormat::F32 => 4,
        }
    }

    /// Frames in the stream, if the header said.
    pub fn frames(&self) -> Option<u64> {
        self.remaining.map(|b| b / (self.sample_bytes() * self.channels as usize) as u64)
    }

    /// Read up to `frames` frames into `out`, interleaved with `out_ch`
    /// channels: mono is copied to every channel, more channels than wanted
    /// are averaged into mono or truncated. Returns the frames read; 0 at
    /// the end of the stream.
    pub fn read(&mut self, frames: usize, out_ch: usize, out: &mut [f32]) -> io::Result<usize> {
        let in_ch = self.channels as usize;
        let sb = self.sample_bytes();
        let align = sb * in_ch;
        let mut want = frames * align;
        if let Some(rem) = self.remaining {
            want = want.min(rem as usize / align * align);
        }
        self.bytes.resize(want, 0);
        let mut got = 0;
        while got < want {
            match self.r.read(&mut self.bytes[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let n = got / align;
        if let Some(rem) = self.remaining.as_mut() {
            *rem -= got as u64;
        }
        let sample = |b: &[u8]| -> f32 {
            match self.format {
                SampleFormat::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0,
                SampleFormat::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
                SampleFormat::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
                SampleFormat::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            }
        };
        for f in 0..n {
            let frame = &self.bytes[f * align..(f + 1) * align];
            let get = |c: usize| sample(&frame[c * sb..(c + 1) * sb]);
            let dst = &mut out[f * out_ch..(f + 1) * out_ch];
            if out_ch == 1 && in_ch > 1 {
                dst[0] = (0..in_ch).map(get).sum::<f32>() / in_ch as f32;
            } else {
                for (c, d) in dst.iter_mut().enumerate() {
                    *d = get(if in_ch == 1 { 0 } else { c.min(in_ch - 1) });
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_reads_back() {
        let samples: Vec<f32> = (0..200).map(|i| ((i as f32) * 0.05).sin() * 0.9).collect();
        let mut wav = Vec::new();
        write_header(&mut wav, 48_000, 2, Some(100)).unwrap();
        write_samples(&mut wav, &samples, &mut Vec::new()).unwrap();

        let mut r = WavReader::new(&wav[..]).unwrap();
        assert_eq!((r.channels, r.sample_rate, r.frames()), (2, 48_000, Some(100)));
        let mut out = vec![0.0f32; 200];
        assert_eq!(r.read(100, 2, &mut out).unwrap(), 100);
        assert!(samples.iter().zip(&out).all(|(a, b)| (a - b).abs() < 1e-6));
        assert_eq!(r.read(100, 2, &mut out).unwrap(), 0);
    }

    #[test]
    fn adapts_channels() {
        let mut wav = Vec::new();
        write_header(&mut wav, 48_000, 2, None).unwrap();
        write_samples(&mut wav, &[0.5, -0.5, 0.25, 0.75], &mut Vec::new()).unwrap();
        let mut r = WavReader::new(&wav[..]).unwrap();
        let mut mono = [0.0f32; 2];
        assert_eq!(r.read(10, 1, &mut mono).unwrap(), 2); // unknown length: reads to EOF
        assert!(mono[0].abs() < 1e-6 && (mono[1] - 0.5).abs() < 1e-6);
    }
}
