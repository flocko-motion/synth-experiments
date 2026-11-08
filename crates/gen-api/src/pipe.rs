//! The pipe protocol between the workspace (host) and a generator subprocess.
//!
//! The host starts the generator with `--pipe` as an extra last argument and
//! talks to it over stdin/stdout. The generator's stderr is free for logging;
//! the host prefixes it with the generator's name. Everything is
//! little-endian.
//!
//! ```text
//! host -> gen   hello     16 bytes   "WSH3" | u32 sample_rate | u32 max_frames | u32 reserved
//! gen  -> host  hello     16 bytes   "WSG3" | u16 channels | u16 inputs | u16 params
//!                                    | u16 reserved | u32 json_len
//!                         + json_len bytes: the plugin's description, the
//!                           same JSON `--info --json` prints
//! then, repeated:
//! host -> gen   request   32 bytes   see Request
//!                         + params * f64: every parameter's value, in
//!                           description order (a full snapshot each time)
//!                         + frames * inputs * f32, interleaved (effects only)
//! gen  -> host  chunk      8 bytes   "WSC3" | u32 frames
//!                          + frames * channels * f32, interleaved
//! ```
//!
//! Plugins without inputs or params send requests that are just the 32-byte
//! header. The host sends parameter defaults until something is set, and
//! adapts input channels, so an effect always gets exactly `inputs`.
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
//! write 16 + its JSON, then loop reading `32 + params * 8 + frames * inputs
//! * 4` and writing `8 + frames * channels * 4` bytes, and log only to stderr.

use crate::{describe_json, Config, Context, Generator, Scale, Tonality, Transport};
use std::io::{self, BufWriter, Read, Write};

pub const HOST_MAGIC: [u8; 4] = *b"WSH3";
pub const GEN_MAGIC: [u8; 4] = *b"WSG3";
pub const CHUNK_MAGIC: [u8; 4] = *b"WSC3";
pub const HELLO_LEN: usize = 16;
pub const GEN_HELLO_LEN: usize = 16;
/// Upper bound on the description in the generator hello.
pub const MAX_JSON_LEN: u32 = 1 << 20;
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
    /// Input channels; 0 for generators, more for effects.
    pub inputs: u16,
    /// Number of f64 values in every request's parameter snapshot.
    pub params: u16,
    /// Length of the JSON description that follows the hello.
    pub json_len: u32,
}

impl GenHello {
    pub fn encode(&self) -> [u8; GEN_HELLO_LEN] {
        let mut b = [0u8; GEN_HELLO_LEN];
        b[0..4].copy_from_slice(&GEN_MAGIC);
        b[4..6].copy_from_slice(&self.channels.to_le_bytes());
        b[6..8].copy_from_slice(&self.inputs.to_le_bytes());
        b[8..10].copy_from_slice(&self.params.to_le_bytes());
        b[12..16].copy_from_slice(&self.json_len.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; GEN_HELLO_LEN]) -> io::Result<GenHello> {
        if b[0..4] != GEN_MAGIC {
            return Err(bad("not a pipe generator, or a different protocol version (bad hello magic)"));
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let h = GenHello { channels: u16_at(4), inputs: u16_at(6), params: u16_at(8), json_len: u32_at(b, 12) };
        if h.channels == 0 || h.channels > 8 {
            return Err(bad("generator reported an unsupported channel count"));
        }
        if h.inputs > 8 {
            return Err(bad("generator reported an unsupported input channel count"));
        }
        if h.json_len > MAX_JSON_LEN {
            return Err(bad("generator description too long"));
        }
        Ok(h)
    }
}

/// Host side: read the generator hello and the JSON description after it.
pub fn read_gen_hello(r: &mut impl Read) -> io::Result<(GenHello, String)> {
    let mut b = [0u8; GEN_HELLO_LEN];
    if !read_message(r, &mut b)? {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let h = GenHello::decode(&b)?;
    let mut json = vec![0u8; h.json_len as usize];
    if !json.is_empty() && !read_message(r, &mut json)? {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let json = String::from_utf8(json).map_err(|_| bad("generator description is not UTF-8"))?;
    Ok((h, json))
}

/// Host side: write one request with its parameter snapshot and, for
/// effects, its input audio (`frames * inputs` samples, interleaved).
pub fn write_request(w: &mut impl Write, req: &Request, params: &[f64], input: &[f32]) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(REQUEST_LEN + params.len() * 8 + input.len() * 4);
    bytes.extend_from_slice(&req.encode());
    for p in params {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    for s in input {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    w.write_all(&bytes)
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
    let params = gen.params();
    let ch = info.channels as usize;
    let inputs = gen.inputs() as usize;
    let json = describe_json(&gen);
    eprintln!("{}  seed {seed}  {} Hz  {} ch  {} in  {} params", info.name, hello.sample_rate, ch, inputs, params.len());

    let gh = GenHello { channels: info.channels, inputs: inputs as u16, params: params.len() as u16, json_len: json.len() as u32 };
    stdout.write_all(&gh.encode())?;
    stdout.write_all(json.as_bytes())?;
    stdout.flush()?;

    let max = hello.max_frames.max(1) as usize;
    let mut rb = [0u8; REQUEST_LEN];
    let mut pb = vec![0u8; params.len() * 8];
    let mut values: Vec<f64> = params.iter().map(|p| p.default).collect();
    let mut ib: Vec<u8> = Vec::new();
    let mut input: Vec<f32> = Vec::with_capacity(max * inputs);
    let mut buf = vec![0.0f32; max * ch];
    let mut bytes = Vec::with_capacity(buf.len() * 4);
    while read_message(&mut stdin, &mut rb)? {
        let req = Request::decode(&rb)?;
        let n = req.frames as usize;
        if !pb.is_empty() && !read_message(&mut stdin, &mut pb)? {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        for (i, (v, p)) in values.iter_mut().zip(params).enumerate() {
            *v = p.clamp(f64::from_le_bytes(pb[i * 8..i * 8 + 8].try_into().unwrap()));
        }
        input.clear();
        if inputs > 0 {
            ib.resize(n * inputs * 4, 0);
            if !ib.is_empty() && !read_message(&mut stdin, &mut ib)? {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            input.extend(ib.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())));
        }
        if buf.len() < n * ch {
            buf.resize(n * ch, 0.0);
        }
        let ctx = Context { sample_rate, transport: req.transport, tonality: req.tonality, params: &values, input: &input };
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
        let g = GenHello { channels: 2, inputs: 2, params: 3, json_len: 120 };
        assert_eq!(GenHello::decode(&g.encode()).unwrap(), g);
        let mut wire = g.encode().to_vec();
        wire.extend_from_slice(&[b'x'; 120]);
        let (h, json) = read_gen_hello(&mut &wire[..]).unwrap();
        assert_eq!((h, json.len()), (g, 120));
        assert_eq!(decode_chunk_header(&encode_chunk_header(512)).unwrap(), 512);
        assert!(decode_chunk_header(b"hello!\n\0").is_err());
    }
}
