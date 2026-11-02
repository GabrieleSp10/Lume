//! Music the user keeps in this app rather than in a Spotify account: imported playlists and
//! songs liked without Spotify. It stays on disk across logouts.

use serde::{Deserialize, Serialize};

use crate::{api::TrackSummary, cache::Store, config::library_dir};

const PLAYLISTS_KEY: &str = "playlists";
const LIKES_KEY: &str = "likes";
/// URI prefix of an imported playlist.
pub const LOCAL_PLAYLIST: &str = "local:playlist:";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportedPlaylist {
    /// Made from the source, so importing the same playlist again replaces it.
    pub id: String,
    pub name: String,
    pub owner: String,
    pub image_url: String,
    /// "spotify" or "youtube".
    pub source: String,
    pub tracks: Vec<TrackSummary>,
    /// Songs the playlist really has; more than `tracks` when the source gave only some.
    pub total: u32,
}

impl ImportedPlaylist {
    pub fn uri(&self) -> String {
        format!("{LOCAL_PLAYLIST}{}", self.id)
    }
}

#[derive(Clone)]
pub struct LocalLibrary {
    store: Store,
}

impl Default for LocalLibrary {
    fn default() -> Self {
        Self { store: Store::at(library_dir()) }
    }
}

impl LocalLibrary {
    pub fn playlists(&self) -> Vec<ImportedPlaylist> {
        self.store.load(PLAYLISTS_KEY).map(|c| c.value).unwrap_or_default()
    }

    pub fn playlist(&self, uri: &str) -> Option<ImportedPlaylist> {
        let id = uri.strip_prefix(LOCAL_PLAYLIST)?;
        self.playlists().into_iter().find(|p| p.id == id)
    }

    /// Adds `playlist` first, replacing an earlier import of the same one.
    pub fn add(&self, playlist: ImportedPlaylist) {
        let mut all = self.playlists();
        all.retain(|p| p.id != playlist.id);
        all.insert(0, playlist);
        self.store.save(PLAYLISTS_KEY, &all);
    }

    pub fn remove(&self, uri: &str) {
        let mut all = self.playlists();
        all.retain(|p| p.uri() != uri);
        self.store.save(PLAYLISTS_KEY, &all);
    }

    /// Liked songs, newest first.
    pub fn liked(&self) -> Vec<TrackSummary> {
        self.store.load(LIKES_KEY).map(|c| c.value).unwrap_or_default()
    }

    pub fn is_liked(&self, uri: &str) -> bool {
        self.liked().iter().any(|t| t.uri == uri)
    }

    pub fn set_liked(&self, track: &TrackSummary, liked: bool) {
        let mut all = self.liked();
        all.retain(|t| t.uri != track.uri);
        if liked {
            all.insert(0, track.clone());
        }
        self.store.save(LIKES_KEY, &all);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> LocalLibrary {
        LocalLibrary { store: Store::at(std::env::temp_dir().join(format!("lume-library-{}", rand::random::<u32>()))) }
    }

    fn track(uri: &str) -> TrackSummary {
        TrackSummary { uri: uri.into(), name: uri.into(), ..Default::default() }
    }

    #[test]
    fn importing_again_replaces_and_moves_first() {
        let lib = library();
        lib.add(ImportedPlaylist { id: "a".into(), name: "A".into(), ..Default::default() });
        lib.add(ImportedPlaylist { id: "b".into(), name: "B".into(), ..Default::default() });
        lib.add(ImportedPlaylist { id: "a".into(), name: "A2".into(), ..Default::default() });
        let names: Vec<String> = lib.playlists().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["A2", "B"]);
        assert_eq!(lib.playlist("local:playlist:b").unwrap().name, "B");
        lib.remove("local:playlist:a");
        assert_eq!(lib.playlists().len(), 1);
        lib.store.clear();
    }

    #[test]
    fn likes_toggle_without_duplicates() {
        let lib = library();
        lib.set_liked(&track("youtube:x"), true);
        lib.set_liked(&track("youtube:x"), true);
        assert_eq!(lib.liked().len(), 1);
        assert!(lib.is_liked("youtube:x"));
        lib.set_liked(&track("youtube:x"), false);
        assert!(!lib.is_liked("youtube:x"));
        lib.store.clear();
    }
}
