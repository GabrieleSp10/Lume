//! Library data kept on disk, so an app restart costs no Spotify requests. Each key is one JSON
//! file. Reads tell the age; deciding what is still fresh is up to the caller.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config::cache_dir;

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    saved_at: u64,
    value: T,
}

pub struct Cached<T> {
    pub age: Duration,
    pub value: T,
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Default for Store {
    fn default() -> Self {
        Self { dir: cache_dir().join("library") }
    }
}

impl Store {
    /// A store kept in `dir` instead of the cache.
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn load<T: DeserializeOwned>(&self, key: &str) -> Option<Cached<T>> {
        let text = fs::read_to_string(self.path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_str(&text).ok()?;
        Some(Cached { age: Duration::from_secs(now().saturating_sub(entry.saved_at)), value: entry.value })
    }

    pub fn save<T: Serialize>(&self, key: &str, value: &T) {
        let path = self.path(key);
        let write = || -> std::io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            // Written aside first: a crash must not leave half a file.
            let tmp = path.with_extension(format!("{}.tmp", rand::random::<u32>()));
            fs::write(&tmp, serde_json::to_vec(&Entry { saved_at: now(), value })?)?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            eprintln!("cache not saved ({key}): {e}");
        }
    }

    pub fn remove(&self, key: &str) {
        let _ = fs::remove_file(self.path(key));
    }

    /// Forgets everything: the data belongs to the account that logged out.
    pub fn clear(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }

    fn path(&self, key: &str) -> PathBuf {
        let name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        self.dir.join(name).with_extension("json")
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store { dir: std::env::temp_dir().join(format!("lume-cache-{}", rand::random::<u32>())) }
    }

    #[test]
    fn saves_loads_and_clears() {
        let store = store();
        assert!(store.load::<Vec<String>>("spotify:playlist:a").is_none());
        store.save("spotify:playlist:a", &vec!["x".to_string()]);
        let back = store.load::<Vec<String>>("spotify:playlist:a").unwrap();
        assert_eq!(back.value, ["x"]);
        assert!(back.age < Duration::from_secs(5));
        store.remove("spotify:playlist:a");
        assert!(store.load::<Vec<String>>("spotify:playlist:a").is_none());
        store.save("b", &1);
        store.clear();
        assert!(store.load::<i32>("b").is_none());
    }

    #[test]
    fn a_damaged_file_counts_as_missing() {
        let store = store();
        store.save("c", &1);
        fs::write(store.path("c"), "{ not json").unwrap();
        assert!(store.load::<i32>("c").is_none());
        store.clear();
    }

    #[test]
    fn keys_cannot_leave_the_folder() {
        let store = store();
        assert_eq!(store.path("../../evil").parent(), Some(store.dir.as_path()));
    }
}
