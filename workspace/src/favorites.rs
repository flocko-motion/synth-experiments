//! Favourite plugins, kept on disk so they survive daemon restarts:
//! `<state dir>/favorites.json`, a sorted JSON array of plugin names.
//! Names that are no longer in the catalog simply stay in the list.

use crate::log::wlog;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Favorites {
    path: PathBuf,
    names: Mutex<BTreeSet<String>>,
}

impl Favorites {
    /// Loads `<dir>/favorites.json`. A missing file is an empty list; an
    /// unreadable one is logged and moved aside, never silently overwritten.
    pub fn load(dir: PathBuf) -> Favorites {
        let path = dir.join("favorites.json");
        let names = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<BTreeSet<String>>(&text) {
                Ok(names) => names,
                Err(e) => {
                    let aside = path.with_extension("json.broken");
                    let _ = std::fs::rename(&path, &aside);
                    wlog!("favorites: {} is not a JSON list of names ({e}); moved to {}", path.display(), aside.display());
                    BTreeSet::new()
                }
            },
            Err(_) => BTreeSet::new(),
        };
        Favorites { path, names: Mutex::new(names) }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn list(&self) -> Vec<String> {
        self.names.lock().unwrap().iter().cloned().collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names.lock().unwrap().contains(name)
    }

    /// Stars or unstars a plugin and saves the list. Idempotent.
    pub fn set(&self, name: &str, favorite: bool) -> Result<(), String> {
        let mut names = self.names.lock().unwrap();
        let changed = if favorite { names.insert(name.to_owned()) } else { names.remove(name) };
        if changed {
            self.save(&names)?;
        }
        Ok(())
    }

    /// Writes a temp file and renames it over the old one, so a crash
    /// can't leave a half-written list.
    fn save(&self, names: &BTreeSet<String>) -> Result<(), String> {
        let dir = self.path.parent().ok_or("favorites path has no directory")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(names).map_err(|e| e.to_string())? + "\n";
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}
