//! The HTTP API, as specified in `workspace/openapi.yaml` (served at
//! `/api/openapi.yaml`). A small HTTP/1.1 server on std, one thread per
//! connection, `Connection: close` except for the event stream. GETs
//! outside `/api/` serve the web UI's files ([`crate::ui`]).
//!
//! Security: `POST /api/generators` starts programs, so the server binds to
//! loopback only. It answers only requests whose `Host` is its loopback
//! address (against DNS rebinding) and whose `Origin`, if any, is a page on
//! localhost (against other websites). Local dev servers on any port are
//! allowed cross-origin (CORS).

use crate::api::{ApiError, Ctl};
use crate::gen::Exit;
use crate::log::{self, wlog};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const SPEC: &str = include_str!("../openapi.yaml");
const MAX_HEAD: u64 = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
const EVENT_INTERVAL: Duration = Duration::from_millis(50);
/// Log lines replayed to a new event stream.
const LOG_REPLAY: usize = 200;

pub fn bind(addr: SocketAddr) -> Result<TcpListener, String> {
    if !addr.ip().is_loopback() {
        return Err(format!("{addr}: the HTTP API can start programs, so it only binds to loopback (127.0.0.1 or ::1)"));
    }
    TcpListener::bind(addr).map_err(|e| format!("http {addr}: {e}"))
}

/// Serves the API under `/api/` and the web UI's files from `ui` elsewhere.
pub fn serve(listener: TcpListener, ctl: Ctl, cwd: PathBuf, ui: PathBuf) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let (ctl, cwd, ui) = (ctl.clone(), cwd.clone(), ui.clone());
        thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let _ = handle(stream, &ctl, &cwd, &ui, port);
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> io::Result<Option<Request>> {
    let mut r = BufReader::new(stream.take(MAX_HEAD));
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return Ok(None) };
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    // Whatever the BufReader already pulled in belongs to the body.
    let mut body = r.buffer().to_vec();
    let mut rest = r.into_inner().into_inner();
    if body.len() < len {
        let mut more = vec![0; len - body.len()];
        rest.read_exact(&mut more)?;
        body.extend(more);
    }
    body.truncate(len);
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    Ok(Some(Request { method, path: path.to_owned(), query, headers, body }))
}

fn is_local_host(host: &str, port: Option<u16>) -> bool {
    let (name, p) = match host.rsplit_once(':') {
        Some((n, p)) if !n.ends_with(']') || host.starts_with('[') => (n, p.parse::<u16>().ok()),
        _ => (host, None),
    };
    let local = matches!(name, "localhost" | "127.0.0.1" | "[::1]");
    local && (port.is_none() || p == port)
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: &impl Serialize) -> Response {
        Response { status, content_type: "application/json", body: serde_json::to_vec(value).unwrap_or_default() }
    }

    fn error(e: &ApiError) -> Response {
        Response::json(e.status(), &serde_json::json!({ "error": e.to_string() }))
    }

    fn empty(status: u16) -> Response {
        Response { status, content_type: "text/plain", body: Vec::new() }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn write_head(stream: &mut TcpStream, status: u16, content_type: &str, len: Option<usize>, cors: &str) -> io::Result<()> {
    let mut h = format!("HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\n{cors}", reason(status));
    match len {
        Some(n) => h.push_str(&format!("Content-Length: {n}\r\nConnection: close\r\n\r\n")),
        None => h.push_str("Connection: keep-alive\r\n\r\n"),
    }
    stream.write_all(h.as_bytes())
}

fn handle(mut stream: TcpStream, ctl: &Ctl, cwd: &PathBuf, ui: &Path, port: u16) -> io::Result<()> {
    let Some(req) = read_request(&stream)? else { return Ok(()) };

    let host_ok = req.headers.get("host").is_some_and(|h| is_local_host(h, Some(port)));
    let origin = req.headers.get("origin");
    let origin_ok = origin.is_none_or(|o| o.strip_prefix("http://").is_some_and(|h| is_local_host(h, None)));
    if !host_ok || !origin_ok {
        let r = Response::json(403, &serde_json::json!({ "error": "only local pages may use this API" }));
        write_head(&mut stream, r.status, r.content_type, Some(r.body.len()), "")?;
        return stream.write_all(&r.body);
    }
    let cors = match origin {
        Some(o) => format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n"),
        None => String::new(),
    };

    if req.method == "OPTIONS" {
        let pre = format!(
            "{cors}Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Max-Age: 600\r\n"
        );
        return write_head(&mut stream, 204, "text/plain", Some(0), &pre);
    }
    if req.method == "GET" && req.path == "/api/events" {
        return events(stream, ctl, &cors);
    }
    if req.method == "GET" && req.path != "/api" && !req.path.starts_with("/api/") {
        let f = crate::ui::file(ui, &req.path);
        write_head(&mut stream, f.status, f.content_type, Some(f.body.len()), &cors)?;
        return stream.write_all(&f.body);
    }

    let r = route(&req, ctl, cwd);
    write_head(&mut stream, r.status, r.content_type, Some(r.body.len()), &cors)?;
    stream.write_all(&r.body)
}

fn body<T: DeserializeOwned>(req: &Request) -> Result<T, Response> {
    let ct = req.headers.get("content-type").map(String::as_str).unwrap_or("");
    if !ct.starts_with("application/json") {
        return Err(Response::json(415, &serde_json::json!({ "error": "send Content-Type: application/json" })));
    }
    serde_json::from_slice(&req.body).map_err(|e| Response::error(&ApiError::BadRequest(format!("request body: {e}"))))
}

fn reply<T: Serialize>(status: u16, r: Result<T, ApiError>) -> Response {
    match r {
        Ok(v) => Response::json(status, &v),
        Err(e) => Response::error(&e),
    }
}

fn route(req: &Request, ctl: &Ctl, cwd: &PathBuf) -> Response {
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let m = req.method.as_str();
    macro_rules! body {
        () => {
            match body(req) {
                Ok(b) => b,
                Err(r) => return r,
            }
        };
    }
    match (m, segs.as_slice()) {
        ("GET", ["api", "openapi.yaml"]) => Response { status: 200, content_type: "application/yaml", body: SPEC.as_bytes().to_vec() },
        ("GET", ["api", "state"]) => reply(200, ctl.state()),
        ("PATCH", ["api", "transport"]) => reply(200, ctl.patch_transport(body!())),
        ("POST", ["api", "transport", "seek"]) => reply(200, ctl.seek(body!())),
        ("GET", ["api", "generators"]) => reply(200, ctl.generators()),
        ("POST", ["api", "generators"]) => reply(201, ctl.add(body!(), cwd)),
        ("PATCH", ["api", "generators", name]) => reply(200, ctl.patch_generator(name.to_string(), body!())),
        ("DELETE", ["api", "generators", name]) => match exit_of(req).and_then(|x| ctl.remove(name.to_string(), x).map_err(|e| Response::error(&e))) {
            Ok(()) => Response::empty(204),
            Err(r) => r,
        },
        ("PATCH", ["api", "generators", name, "params"]) => match glide_of(req) {
            Ok(g) => reply(200, ctl.set_params(name.to_string(), body!(), g)),
            Err(r) => r,
        },
        ("POST", ["api", "generators", name, "inserts"]) => reply(201, ctl.add_effect(body!(), Some(name.to_string()), cwd)),
        ("GET", ["api", "effects"]) => reply(200, ctl.effects()),
        ("POST", ["api", "effects"]) => reply(201, ctl.add_effect(body!(), None, cwd)),
        ("PATCH", ["api", "effects", name]) => reply(200, ctl.patch_effect(name.to_string(), body!())),
        ("DELETE", ["api", "effects", name]) => match exit_of(req).and_then(|x| ctl.remove_effect(name.to_string(), x).map_err(|e| Response::error(&e))) {
            Ok(()) => Response::empty(204),
            Err(r) => r,
        },
        ("PATCH", ["api", "effects", name, "params"]) => match glide_of(req) {
            Ok(g) => reply(200, ctl.set_effect_params(name.to_string(), body!(), g)),
            Err(r) => r,
        },
        ("POST", ["api", "generators", name, "randomize"]) => reply(200, ctl.randomize(name.to_string())),
        ("POST", ["api", "effects", name, "randomize"]) => reply(200, ctl.randomize_effect(name.to_string())),
        ("POST", ["api", "effects", name, "restart"]) => reply(202, ctl.restart_effect(name.to_string())),
        ("POST", ["api", "generators", name, "restart"]) => reply(202, ctl.restart(name.to_string())),
        ("GET", ["api", "catalog"]) => Response::json(200, &ctl.catalog_list()),
        ("GET", ["api", "favorites"]) => Response::json(200, &ctl.favorites.list()),
        ("PUT" | "DELETE", ["api", "favorites", name]) => match ctl.set_favorite(name, m == "PUT") {
            Ok(()) => Response::empty(204),
            Err(e) => Response::error(&e),
        },
        ("GET", ["api", "sequencer"]) => reply(200, ctl.sequencer()),
        ("PUT", ["api", "sequencer", lane, "banks", bank]) => reply(200, ctl.set_bank(lane, bank, body!())),
        ("DELETE", ["api", "sequencer", lane, "banks", bank]) => reply(200, ctl.set_bank(lane, bank, Default::default())),
        ("POST", ["api", "sequencer", lane, "cue"]) => reply(200, ctl.cue(lane, body!())),
        ("POST", ["api", "sequencer", lane, "banks", bank, "repeat"]) => reply(200, ctl.repeat_bank(lane, bank, body!())),
        ("POST", ["api", "sequencer", "copy"]) => reply(200, ctl.copy_banks(body!())),
        ("GET", ["api", "log"]) => {
            let after = req.query.get("after").and_then(|a| a.parse().ok()).unwrap_or(0);
            Response::json(200, &log_lines(after))
        }
        (_, ["api", "state" | "transport" | "generators" | "effects" | "catalog" | "favorites" | "sequencer" | "log" | "openapi.yaml", ..]) => {
            Response::json(405, &serde_json::json!({ "error": format!("{m} not allowed on {}", req.path) }))
        }
        _ => Response::json(404, &serde_json::json!({ "error": format!("no such endpoint {}", req.path) })),
    }
}

/// The optional `?glide_bars=N` on param changes.
fn exit_of(req: &Request) -> Result<Exit, Response> {
    match req.query.get("exit") {
        None => Ok(Exit::Now),
        Some(v) => Exit::parse(v).ok_or_else(|| Response::error(&ApiError::BadRequest(format!("exit={v}: one of now, cycle, epoch, fade_cycle, fade_epoch")))),
    }
}

fn glide_of(req: &Request) -> Result<Option<f64>, Response> {
    match req.query.get("glide_bars") {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| Response::error(&ApiError::BadRequest(format!("glide_bars={v}: not a number")))),
    }
}

#[derive(Serialize)]
struct LogLine {
    seq: u64,
    line: String,
}

fn log_lines(after: u64) -> Vec<LogLine> {
    log::since(after).into_iter().map(|(seq, line)| LogLine { seq, line }).collect()
}

/// Server-sent events: `state` about 20 times a second, and each new log
/// line as a `log` event (the last few hundred are replayed on connect).
fn events(mut stream: TcpStream, ctl: &Ctl, cors: &str) -> io::Result<()> {
    write_head(&mut stream, 200, "text/event-stream", None, cors)?;
    let mut lines = log_lines(0);
    let skip = lines.len().saturating_sub(LOG_REPLAY);
    let mut last = 0;
    let mut first = true;
    loop {
        let mut out = String::new();
        if !first {
            lines = log_lines(last);
        }
        for l in lines.iter().skip(if first { skip } else { 0 }) {
            out.push_str(&format!("event: log\nid: {}\ndata: {}\n\n", l.seq, serde_json::to_string(l).unwrap_or_default()));
            last = l.seq;
        }
        if let Some(l) = lines.last() {
            last = last.max(l.seq);
        }
        first = false;
        match ctl.state() {
            Ok(s) => out.push_str(&format!("event: state\ndata: {}\n\n", serde_json::to_string(&s).unwrap_or_default())),
            Err(_) => return Ok(()), // daemon stopping
        }
        if stream.write_all(out.as_bytes()).is_err() {
            return Ok(()); // client went away
        }
        thread::sleep(EVENT_INTERVAL);
    }
}

pub fn log_bind_failure(e: &str) {
    wlog!("{e}; continuing without the HTTP API");
}
