//! Optional on-disk storage for the Servers feature under `SERVERS_DATA_DIR`:
//! `servers-state.json` (last results), and JSON Lines files for history, alerts and audit log.
//! Failures are logged to stderr and never stop the service.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use serde::{Serialize, de::DeserializeOwned};

/// Last results of every server.
pub(super) const STATE_FILE: &str = "servers-state.json";
/// One line per collection: `{"id", "t", "status", "cpu", "mem", "disk", "load_1"}`.
pub(super) const HISTORY_FILE: &str = "servers-history.jsonl";
/// One line per alert.
pub(super) const ALERTS_FILE: &str = "servers-alerts.jsonl";
/// One line per refresh / lookup request.
pub(super) const AUDIT_FILE: &str = "servers-audit.jsonl";

/// Files in one folder; writes are serialized so a newer snapshot is never overwritten by an older one.
#[derive(Debug, Default)]
pub(super) struct Store {
    dir: Option<PathBuf>,
    write_lock: Mutex<()>,
}

impl Store {
    /// Uses `dir` (created if missing), or stores nothing when `None`.
    pub fn new(dir: Option<PathBuf>) -> Self {
        if let Some(dir) = &dir
            && let Err(e) = fs::create_dir_all(dir)
        {
            eprintln!("servers: cannot create data dir {}: {e}", dir.display());
        }
        Self {
            dir,
            write_lock: Mutex::new(()),
        }
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.write_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads a JSON file; `None` when storage is off, the file is missing or invalid.
    pub fn read_json<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let text = fs::read_to_string(self.path(name)?).ok()?;
        serde_json::from_str(&text)
            .map_err(|e| eprintln!("servers: ignoring invalid {name}: {e}"))
            .ok()
    }

    /// Reads a JSON Lines file, skipping invalid lines.
    pub fn read_lines<T: DeserializeOwned>(&self, name: &str) -> Vec<T> {
        let Some(text) = self.path(name).and_then(|p| fs::read_to_string(p).ok()) else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Replaces a file atomically (temp file + rename). `content` runs while the write lock is held.
    pub fn rewrite(&self, name: &str, content: impl FnOnce() -> String) {
        let Some(path) = self.path(name) else { return };
        let _guard = self.guard();
        let text = content();
        let tmp = path.with_extension("tmp");
        if let Err(e) = fs::write(&tmp, text).and_then(|()| fs::rename(&tmp, &path)) {
            eprintln!("servers: cannot write {}: {e}", path.display());
        }
    }

    /// Appends one JSON line.
    pub fn append<T: Serialize>(&self, name: &str, item: &T) {
        let Some(path) = self.path(name) else { return };
        let Ok(line) = serde_json::to_string(item) else {
            return;
        };
        let _guard = self.guard();
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            eprintln!("servers: cannot append to {}: {e}", path.display());
        }
    }
}
