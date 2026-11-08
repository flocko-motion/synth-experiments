//! The pipe protocol between the workspace (host) and a generator subprocess.
//!
//! The host starts the generator with `--pipe` as an extra last argument and
//! talks to it over stdin/stdout. The generator's stderr is free for logging;
//! the host prefixes it with the generator's name. Everything is
//! little-endian.
//!
//! ```text
//! host -> gen   hello     16 bytes   "WSH2" | u32 sample_rate | u32 max_frames | u32 reserved
//! gen  -> host  hello      8 bytes   "WSG2" | u16 channels    | u16 reserved
//! then, repeated:
//! host -> gen   request   32 bytes   see Request
//! gen  -> host  chunk      8 bytes   "WSC2" | u32 frames
//!                          + frames * channels * f32, interleaved
//! ```
//!
//! The chunk header lets the host notice when stray bytes (a debug print on
//! stdout) have corrupted the stream; it then restarts the generator.
//! Generators answer requests strictly in order, and the host may send the
//! next request before reading the previous chunk.
//!
//! The host closes stdin to stop the generator; the generator should then
//! exit. Requests are normally contiguous (each `frame`/`beat` follows on from
//! the previous request); when they aren't, the host seeked or skipped.
//!
//! A generator in another language only has to implement this file: read 16,
//! write 8, then loop reading 32 and writing `8 + frames * channels * 4`
//! bytes, and log only to stderr.

use crate::{Config, Context, Generator, Scale, Tonality, Transport};
use std::io::{self, BufWriter, Read, Write};

pub const HOST_MAGIC: [u8; 4] = *b"WSH2";
pub const GEN_MAGIC: [u8; 4] = *b"WSG2";
pub const CHUNK_MAGIC: [u8; 4] = *b"WSC2";
pub const HELLO_LEN: usize = 16;
pub const GEN_HELLO_LEN: usize = 8;
pub const REQUEST_LEN: usize = 32;
pub const CHUNK_HEADER_LEN: usize = 8;

pub fn encode_chunk_header(frames: u32) -> [u8; CHUNK_HEADER_LEN] {
    let mut b = [0u8; CHUNK_HEADER_LEN];
    b[0..4].copy_from_slice(&CHUNK_MAGIC);
    b[4..8].copy_from_slice(&frames.to_le_bytes());
    b
}

/// The frame count of a chunk header, or an error if the bytes aren't one.
pub fn decode_chunk_header(b: &[u8; CHUNK_HEADER_LEN]) -> io::Result<u32> {
    if b[0..4] != CHUNK_MAGIC {
        return Err(bad("garbled chunk (stray output on stdout?)"));
    }
    Ok(u32_at(b, 4))
}

/// Host greeting, sent once before any request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hello {
    pub sample_rate: u32,
    /// Upper bound on `Request::frames`, so the generator can size buffers.
    pub max_frames: u32,
}

impl Hello {
    pub fn encode(&self) -> [u8; HELLO_LEN] {
        let mut b = [0u8; HELLO_LEN];
        b[0..4].copy_from_slice(&HOST_MAGIC);
        b[4..8].copy_from_slice(&self.sample_rate.to_le_bytes());
        b[8..12].copy_from_slice(&self.max_frames.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; HELLO_LEN]) -> io::Result<Hello> {
        if b[0..4] != HOST_MAGIC {
            return Err(bad("not a workspace host, or a different protocol version (bad hello magic)"));
        }
        Ok(Hello { sample_rate: u32_at(b, 4), max_frames: u32_at(b, 8) })
    }
}

/// Generator greeting, sent once in reply to [`Hello`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GenHello {
    pub channels: u16,
}

impl GenHello {
    pub fn encode(&self) -> [u8; GEN_HELLO_LEN] {
        let mut b = [0u8; GEN_HELLO_LEN];
        b[0..4].copy_from_slice(&GEN_MAGIC);
        b[4..6].copy_from_slice(&self.channels.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; GEN_HELLO_LEN]) -> io::Result<GenHello> {
        if b[0..4] != GEN_MAGIC {
            return Err(bad("not a pipe generator, or a different protocol version (bad hello magic)"));
        }
        let channels = u16::from_le_bytes([b[4], b[5]]);
        if channels == 0 || channels > 8 {
            return Err(bad("generator reported an unsupported channel count"));
        }
        Ok(GenHello { channels })
    }
}

/// "Render the next `frames` frames, starting here."
///
/// ```text
///  0  u32  frames
///  4  u8   playing (0/1)
///  5  u8   root pitch class, 0 = C
///  6  u8   scale, index into Scale::ALL
///  7  u8   beats_per_bar
///  8  f64  bpm
/// 16  f64  beat     position of the first frame, quarter notes since 0
/// 24  u64  frame    absolute sample index of the first frame
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Request {
    pub frames: u32,
    pub frame: u64,
    pub transport: Transport,
    pub tonality: Tonality,
}

impl Request {
    pub fn encode(&self) -> [u8; REQUEST_LEN] {
        let mut b = [0u8; REQUEST_LEN];
        b[0..4].copy_from_slice(&self.frames.to_le_bytes());
        b[4] = self.transport.playing as u8;
        b[5] = self.tonality.root % 12;
        b[6] = self.tonality.scale.index() as u8;
        b[7] = self.transport.beats_per_bar as u8;
        b[8..16].copy_from_slice(&self.transport.bpm.to_le_bytes());
        b[16..24].copy_from_slice(&self.transport.beat.to_le_bytes());
        b[24..32].copy_from_slice(&self.frame.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; REQUEST_LEN]) -> io::Result<Request> {
        let scale = Scale::ALL.get(b[6] as usize).copied().ok_or_else(|| bad("unknown scale index"))?;
        Ok(Request {
            frames: u32_at(b, 0),
            frame: u64::from_le_bytes(b[24..32].try_into().unwrap()),
            transport: Transport {
                bpm: f64::from_le_bytes(b[8..16].try_into().unwrap()),
                beat: f64::from_le_bytes(b[16..24].try_into().unwrap()),
                beats_per_bar: b[7].max(1) as u32,
                playing: b[4] != 0,
            },
            tonality: Tonality { root: b[5] % 12, scale },
        })
    }
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Reads exactly `buf.len()` bytes. `Ok(false)` on a clean EOF before the
/// first byte (the host hung up between messages).
pub fn read_message(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Takes fd 1 for the protocol and points fd 1 at stderr, so a stray
/// `println!` in a generator lands in the log instead of in the audio stream.
fn claim_stdout() -> io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    extern "C" {
        fn dup(fd: i32) -> i32;
        fn dup2(src: i32, dst: i32) -> i32;
    }
    io::stdout().flush()?;
    // SAFETY: plain fd syscalls; the duplicated fd is owned by the File.
    unsafe {
        let proto = dup(1);
        if proto < 0 || dup2(2, 1) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::fs::File::from_raw_fd(proto))
    }
}

/// Generator side: serve requests on stdin/stdout until the host hangs up.
pub(crate) fn serve<G: Generator>(seed: u64) -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = BufWriter::new(claim_stdout()?);

    let mut hb = [0u8; HELLO_LEN];
    if !read_message(&mut stdin, &mut hb)? {
        return Ok(());
    }
    let hello = Hello::decode(&hb)?;
    let sample_rate = hello.sample_rate as f64;
    let mut gen = G::new(&Config { sample_rate, seed });
    let info = gen.info();
    let ch = info.channels as usize;
    eprintln!("{}  seed {seed}  {} Hz  {} ch", info.name, hello.sample_rate, ch);

    stdout.write_all(&GenHello { channels: info.channels }.encode())?;
    stdout.flush()?;

    let mut rb = [0u8; REQUEST_LEN];
    let mut buf = vec![0.0f32; hello.max_frames.max(1) as usize * ch];
    let mut bytes = Vec::with_capacity(buf.len() * 4);
    while read_message(&mut stdin, &mut rb)? {
        let req = Request::decode(&rb)?;
        let n = req.frames as usize;
        if buf.len() < n * ch {
            buf.resize(n * ch, 0.0);
        }
        let ctx = Context { sample_rate, transport: req.transport, tonality: req.tonality };
        let out = &mut buf[..n * ch];
        gen.process(&ctx, out);
        bytes.clear();
        bytes.extend_from_slice(&encode_chunk_header(req.frames));
        for s in out.iter() {
            // NaN passes through on purpose: the host detects it and
            // restarts the generator, whose state is likely broken.
            bytes.extend_from_slice(&s.clamp(-1.0, 1.0).to_le_bytes());
        }
        stdout.write_all(&bytes)?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = Request {
            frames: 512,
            frame: 1 << 40,
            transport: Transport { bpm: 145.5, beat: 1234.25, beats_per_bar: 4, playing: true },
            tonality: Tonality { root: 6, scale: Scale::Dorian },
        };
        assert_eq!(Request::decode(&req.encode()).unwrap(), req);
    }

    #[test]
    fn hellos_round_trip() {
        let h = Hello { sample_rate: 44_100, max_frames: 1024 };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let g = GenHello { channels: 2 };
        assert_eq!(GenHello::decode(&g.encode()).unwrap(), g);
        assert_eq!(decode_chunk_header(&encode_chunk_header(512)).unwrap(), 512);
        assert!(decode_chunk_header(b"hello!\n\0").is_err());
    }
}
