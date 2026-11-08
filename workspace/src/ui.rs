//! The web UI: files served from a directory on disk (`--ui DIR`), read on
//! every request, so new or edited files show up on reload. Requests can
//! only reach files under that directory: the path is percent-decoded, then
//! `..`, absolute paths and NUL bytes are refused, and the resolved file
//! must still lie under the directory after resolving symlinks.

use std::fs;
use std::path::{Path, PathBuf};

pub struct File {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl File {
    fn text(status: u16, msg: String) -> File {
        File { status, content_type: "text/plain; charset=utf-8", body: msg.into_bytes() }
    }

    fn not_found() -> File {
        File::text(404, "not found\n".into())
    }
}

/// Serves the file for the URL path `path` (without query) from `root`.
pub fn file(root: &Path, path: &str) -> File {
    let Ok(root_real) = root.canonicalize() else {
        return File::text(
            404,
            format!("no web UI: {} does not exist (start the daemon with --ui DIR)\n", root.display()),
        );
    };
    let Some(rel) = relative(path) else { return File::not_found() };
    let mut target = root_real.join(rel);
    if target.is_dir() {
        target.push("index.html");
    }
    let Ok(real) = target.canonicalize() else { return File::not_found() };
    if !real.starts_with(&root_real) || !real.is_file() {
        return File::not_found();
    }
    match fs::read(&real) {
        Ok(body) => File { status: 200, content_type: content_type(&real), body },
        Err(_) => File::not_found(),
    }
}

/// The decoded URL path as a relative path with only normal components, or
/// None if it is malformed or tries to leave the directory.
fn relative(path: &str) -> Option<PathBuf> {
    let decoded = String::from_utf8(percent_decode(path)?).ok()?;
    let rest = decoded.strip_prefix('/')?;
    if rest.starts_with('/') || decoded.contains(['\0', '\\']) {
        return None;
    }
    let mut rel = PathBuf::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => rel.push(s),
        }
    }
    Some(rel)
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
