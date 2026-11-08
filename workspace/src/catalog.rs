//! Generators available to add: the executables in one directory
//! (`target/release` by default) that answer `--info` the way `gen-api`'s
//! CLI does. A freshly built generator shows up without restarting the
//! daemon. Answers are cached per binary until it's rebuilt.

use serde::Serialize;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Serialize, Clone)]
pub struct Entry {
    pub name: String,
    pub description: String,
    pub channels: u16,
    pub path: String,
}

pub struct Catalog {
    dir: PathBuf,
    cache: Mutex<HashMap<PathBuf, (SystemTime, Option<Entry>)>>,
}

impl Catalog {
    pub fn new(dir: PathBuf) -> Catalog {
        Catalog { dir, cache: Mutex::new(HashMap::new()) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn list(&self) -> Vec<Entry> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out: Vec<Entry> = rd.filter_map(|e| e.ok()).filter_map(|e| self.probe(&e.path())).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The binary for a catalog name, if it is a generator.
    pub fn path_of(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return None;
        }
        let path = self.dir.join(name);
        self.probe(&path).map(|_| path)
    }

    fn probe(&self, path: &Path) -> Option<Entry> {
        let meta = std::fs::metadata(path).ok()?;
        let file = path.file_name()?.to_str()?;
        // Executables only, and not cargo's build artifacts or ws itself.
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 || file.contains('.') || file == "ws" {
            return None;
        }
        let mtime = meta.modified().ok()?;
        if let Some((t, entry)) = self.cache.lock().unwrap().get(path) {
            if *t == mtime {
                return entry.clone();
            }
        }
        let entry = ask_info(path, file);
        self.cache.lock().unwrap().insert(path.to_owned(), (mtime, entry.clone()));
        entry
    }
}

/// Runs `path --info` and parses gen-api's three lines: name, description,
/// `channels: N`. Anything else, or no answer within the timeout, means
/// it's not a generator.
fn ask_info(path: &Path, file: &str) -> Option<Entry> {
    let mut child = Command::new(path)
        .arg("--info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let until = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
            _ => {
                crate::sys::kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    let mut lines = text.lines();
    let _name = lines.next()?;
    let description = lines.next()?.to_owned();
    let channels = lines.next()?.strip_prefix("channels:")?.trim().parse().ok()?;
    Some(Entry { name: file.to_owned(), description, channels, path: path.to_string_lossy().into_owned() })
}
