//! Origin-scoped key-value storage for the app: the terminal's
//! `localStorage`. Each origin gets its own private file under the storage
//! directory, bounded by a byte and entry quota; an app without an origin
//! gets an ephemeral store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Limits;
use crate::bindings::rattery::tui::storage::StorageError;
use crate::http::{open_no_follow, save_private, with_jar_lock};

#[derive(Default, Serialize, Deserialize)]
struct Contents {
    entries: BTreeMap<String, Vec<u8>>,
}

pub struct Storage {
    contents: Contents,
    bytes: u64,
    path: Option<PathBuf>,
    quota_bytes: u64,
    max_entries: usize,
    max_key_bytes: usize,
    max_value_bytes: usize,
    disabled: bool,
}

/// A file name that is stable for an origin and safe on every filesystem:
/// `http://127.0.0.1:3000` becomes `http_127.0.0.1_3000.json`.
pub fn file_for(origin: &str) -> String {
    let mut name = String::new();
    for c in origin.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
            name.push(c);
        } else if !name.ends_with('_') {
            name.push('_');
        }
    }
    name.truncate(200);
    format!("{name}.json")
}

impl Storage {
    pub fn disabled() -> Self {
        Self::build(None, &Limits::default(), true)
    }

    pub fn ephemeral(limits: &Limits) -> Self {
        Self::build(None, limits, false)
    }

    /// Storage for `origin` persisted under `dir`.
    pub fn persistent(dir: &Path, origin: &str, limits: &Limits) -> Result<Self> {
        let path = dir.join(file_for(origin));
        let mut storage = Self::build(Some(path.clone()), limits, false);
        let contents: Contents = with_jar_lock(&path, || {
            Ok(open_no_follow(&path)
                .and_then(|file| serde_json::from_reader(std::io::BufReader::new(file)).ok())
                .unwrap_or_default())
        })
        .with_context(|| format!("failed to load storage {}", path.display()))?;
        storage.bytes = contents
            .entries
            .iter()
            .map(|(k, v)| (k.len() + v.len()) as u64)
            .sum();
        storage.contents = contents;
        Ok(storage)
    }

    fn build(path: Option<PathBuf>, limits: &Limits, disabled: bool) -> Self {
        Self {
            contents: Contents::default(),
            bytes: 0,
            path,
            quota_bytes: limits.storage_bytes as u64,
            max_entries: limits.storage_entries,
            max_key_bytes: limits.storage_key_bytes,
            max_value_bytes: limits.storage_value_bytes,
            disabled,
        }
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.contents.entries.get(key).cloned()
    }

    pub fn set(&mut self, key: String, value: Vec<u8>) -> Result<(), StorageError> {
        if self.disabled {
            return Err(StorageError::Disabled);
        }
        if key.len() > self.max_key_bytes || value.len() > self.max_value_bytes {
            return Err(StorageError::TooLarge);
        }
        let existing = self
            .contents
            .entries
            .get(&key)
            .map(|v| (key.len() + v.len()) as u64);
        let after = self.bytes - existing.unwrap_or(0) + (key.len() + value.len()) as u64;
        let entries_after = self.contents.entries.len() + usize::from(existing.is_none());
        if after > self.quota_bytes || entries_after > self.max_entries {
            return Err(StorageError::QuotaExceeded);
        }
        self.contents.entries.insert(key, value);
        self.bytes = after;
        self.persist();
        Ok(())
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(value) = self.contents.entries.remove(key) {
            self.bytes -= (key.len() + value.len()) as u64;
            self.persist();
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.contents.entries.keys().cloned().collect()
    }

    pub fn clear(&mut self) {
        if !self.contents.entries.is_empty() {
            self.contents.entries.clear();
            self.bytes = 0;
            self.persist();
        }
    }

    pub fn usage(&self) -> (u64, u64) {
        (self.bytes, self.quota_bytes)
    }

    /// Write through, privately and atomically, like the cookie jar.
    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let result = with_jar_lock(path, || {
            save_private(path, |file| {
                serde_json::to_writer(file, &self.contents).map_err(std::io::Error::other)
            })?;
            Ok(())
        });
        if let Err(err) = result {
            eprintln!(
                "rattery: could not save storage to {}: {err}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotas_and_persistence() {
        let dir = std::env::temp_dir().join(format!("rattery-storage-{}", std::process::id()));
        let limits = Limits {
            storage_bytes: 30,
            storage_entries: 3,
            ..Limits::default()
        };
        let origin = "http://127.0.0.1:3000";

        let mut s = Storage::persistent(&dir, origin, &limits).unwrap();
        s.set("a".into(), vec![1; 10]).unwrap();
        assert_eq!(
            s.set("b".into(), vec![1; 25]),
            Err(StorageError::QuotaExceeded)
        );
        s.set("b".into(), vec![1; 5]).unwrap();
        s.set("c".into(), vec![]).unwrap();
        assert_eq!(
            s.set("d".into(), vec![]),
            Err(StorageError::QuotaExceeded),
            "entry cap"
        );
        s.set("a".into(), vec![2; 12]).unwrap();
        assert_eq!(s.usage(), (13 + 6 + 1, 30));

        // Another origin is another file; the same origin reloads its data.
        let other = Storage::persistent(&dir, "http://example.com", &limits).unwrap();
        assert!(other.get("a").is_none());
        let again = Storage::persistent(&dir, origin, &limits).unwrap();
        assert_eq!(again.get("a"), Some(vec![2; 12]));
        assert_eq!(again.keys(), ["a", "b", "c"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(file_for(origin)))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);

        let mut off = Storage::disabled();
        assert_eq!(off.set("k".into(), vec![]), Err(StorageError::Disabled));
        let mut big = Storage::ephemeral(&Limits::default());
        assert_eq!(
            big.set(
                "k".into(),
                vec![0; Limits::default().storage_value_bytes + 1]
            ),
            Err(StorageError::TooLarge)
        );
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(
            file_for("http://127.0.0.1:3000"),
            "http_127.0.0.1_3000.json"
        );
        assert_eq!(
            file_for("https://Ex.ample/../x"),
            "https_Ex.ample_.._x.json"
        );
    }
}
