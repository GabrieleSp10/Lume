//! Music the user keeps in this app rather than in a Spotify account: imported, copied and
//! self-made playlists, and songs liked without Spotify. It stays on disk across logouts.

use serde::{Deserialize, Serialize};

use crate::{api::TrackSummary, cache::Store, config::library_dir};

const PLAYLISTS_KEY: &str = "playlists";
const LIKES_KEY: &str = "likes";
/// Songs downloaded one by one, outside any playlist kept offline.
const OFFLINE_SONGS_KEY: &str = "offline-songs";
/// URI prefix of an imported playlist.
pub const LOCAL_PLAYLIST: &str = "local:playlist:";
/// A playlist the user made in the app.
pub const SOURCE_OWN: &str = "own";
/// A Spotify playlist rebuilt from YouTube videos.
pub const SOURCE_COPY: &str = "copy";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportedPlaylist {
    /// Made from the source, so importing the same playlist again replaces it.
    pub id: String,
    pub name: String,
    pub owner: String,
    pub image_url: String,
    /// "spotify" or "youtube" for imports, [`SOURCE_OWN`] or [`SOURCE_COPY`].
    pub source: String,
    pub tracks: Vec<TrackSummary>,
    /// Songs the playlist really has; more than `tracks` when the source gave only some.
    pub total: u32,
    /// The user picked the image; otherwise it is made from the songs' covers.
    pub custom_image: bool,
    /// The user asked to keep its YouTube songs for listening offline.
    pub offline: bool,
    /// For a copy, the name of the playlist it was made from.
    pub origin: String,
}

impl ImportedPlaylist {
    pub fn uri(&self) -> String {
        format!("{LOCAL_PLAYLIST}{}", self.id)
    }

    /// Its songs may be downloaded, always from YouTube: in the user's own playlists and
    /// copies, any song (it is looked up there); in imports, only YouTube's; never in a
    /// playlist that is Spotify's.
    pub fn downloadable(&self) -> bool {
        match self.source.as_str() {
            "spotify" => false,
            SOURCE_OWN | SOURCE_COPY => !self.tracks.is_empty(),
            _ => self.tracks.iter().any(|t| crate::offline::video_id(&t.uri).is_some()),
        }
    }

    /// The covers of the first four songs with different ones, for the playlist's image.
    pub fn mosaic_covers(&self) -> Vec<String> {
        let mut covers: Vec<String> = Vec::new();
        for track in &self.tracks {
            if !track.image_url.is_empty() && !covers.contains(&track.image_url) {
                covers.push(track.image_url.clone());
                if covers.len() == 4 {
                    break;
                }
            }
        }
        covers
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

    /// Saves a changed playlist where it is in the list.
    pub fn update(&self, playlist: &ImportedPlaylist) {
        let mut all = self.playlists();
        match all.iter_mut().find(|p| p.id == playlist.id) {
            Some(slot) => *slot = playlist.clone(),
            None => all.insert(0, playlist.clone()),
        }
        self.store.save(PLAYLISTS_KEY, &all);
    }

    /// A new, empty playlist made by the user.
    pub fn create(&self, name: &str, image_url: &str) -> ImportedPlaylist {
        let playlist = ImportedPlaylist {
            id: format!("own-{:016x}", rand::random::<u64>()),
            name: name.trim().to_string(),
            source: SOURCE_OWN.into(),
            image_url: image_url.to_string(),
            custom_image: !image_url.is_empty(),
            ..Default::default()
        };
        self.add(playlist.clone());
        playlist
    }

    /// Adds a song at the end of a playlist. False if it was already there.
    pub fn add_track(&self, uri: &str, track: &TrackSummary) -> Option<bool> {
        let mut playlist = self.playlist(uri)?;
        if playlist.tracks.iter().any(|t| t.uri == track.uri) {
            return Some(false);
        }
        playlist.tracks.push(track.clone());
        playlist.total = playlist.tracks.len() as u32;
        self.update(&playlist);
        Some(true)
    }

    pub fn remove_track(&self, uri: &str, track_uri: &str) -> Option<ImportedPlaylist> {
        let mut playlist = self.playlist(uri)?;
        playlist.tracks.retain(|t| t.uri != track_uri);
        playlist.total = playlist.tracks.len() as u32;
        self.update(&playlist);
        Some(playlist)
    }

    /// Songs downloaded one by one.
    pub fn offline_songs(&self) -> Vec<TrackSummary> {
        self.store.load(OFFLINE_SONGS_KEY).map(|c| c.value).unwrap_or_default()
    }

    pub fn set_offline_song(&self, track: &TrackSummary, keep: bool) {
        let mut all = self.offline_songs();
        all.retain(|t| t.uri != track.uri);
        if keep {
            all.push(track.clone());
        }
        self.store.save(OFFLINE_SONGS_KEY, &all);
    }

    /// The playlist kept offline that also has this song (other than `except`), so its file
    /// must stay; "" when the song was downloaded on its own.
    pub fn kept_offline_by(&self, track_uri: &str, except: &str) -> Option<String> {
        if self.offline_songs().iter().any(|t| t.uri == track_uri) && except != OFFLINE_SONGS_KEY {
            return Some(String::new());
        }
        self.playlists().into_iter().find(|p| p.offline && p.id != except && p.tracks.iter().any(|t| t.uri == track_uri)).map(|p| p.name)
    }

    /// Nothing is kept offline any more.
    pub fn forget_offline(&self) {
        let mut all = self.playlists();
        all.iter_mut().for_each(|p| p.offline = false);
        self.store.save(PLAYLISTS_KEY, &all);
        self.store.save(OFFLINE_SONGS_KEY, &Vec::<TrackSummary>::new());
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
    fn own_playlists_take_songs_once_and_keep_their_place() {
        let lib = library();
        lib.add(ImportedPlaylist { id: "imported".into(), source: "spotify".into(), ..Default::default() });
        let mine = lib.create("  Mine ", "");
        assert_eq!(mine.name, "Mine");
        assert!(!mine.custom_image);
        assert_eq!(lib.add_track(&mine.uri(), &track("youtube:aaaaaaaaaaa")), Some(true));
        assert_eq!(lib.add_track(&mine.uri(), &track("youtube:aaaaaaaaaaa")), Some(false));
        assert_eq!(lib.add_track("local:playlist:imported", &track("youtube:b")), Some(true));
        assert_eq!(lib.add_track("local:playlist:missing", &track("youtube:b")), None);
        lib.add(ImportedPlaylist { id: "newer".into(), ..Default::default() });
        lib.add_track(&mine.uri(), &track("spotify:track:x"));
        let ids: Vec<String> = lib.playlists().into_iter().map(|p| p.id).collect();
        assert_eq!(ids[1], mine.id);
        assert_eq!(lib.playlist(&mine.uri()).unwrap().total, 2);
        assert_eq!(lib.remove_track(&mine.uri(), "spotify:track:x").unwrap().tracks.len(), 1);
        lib.store.clear();
    }

    #[test]
    fn a_file_stays_while_something_keeps_it() {
        let lib = library();
        let song = track("youtube:aaaaaaaaaaa");
        lib.add(ImportedPlaylist { id: "a".into(), name: "A".into(), offline: true, tracks: vec![song.clone()], ..Default::default() });
        assert_eq!(lib.kept_offline_by(&song.uri, "a"), None);
        lib.set_offline_song(&song, true);
        assert_eq!(lib.kept_offline_by(&song.uri, "a"), Some(String::new()));
        assert_eq!(lib.kept_offline_by(&song.uri, "offline-songs"), Some("A".into()));
        lib.forget_offline();
        assert_eq!(lib.kept_offline_by(&song.uri, ""), None);
        lib.store.clear();
    }

    #[test]
    fn only_youtube_songs_outside_spotify_playlists_download() {
        let mut p = ImportedPlaylist { source: "spotify".into(), tracks: vec![track("youtube:aaaaaaaaaaa")], ..Default::default() };
        assert!(!p.downloadable());
        p.source = SOURCE_COPY.into();
        assert!(p.downloadable());
        p.tracks = vec![track("spotify:track:x")];
        assert!(p.downloadable(), "a Spotify song in an own playlist is looked up on YouTube");
        p.source = "youtube".into();
        assert!(!p.downloadable());
    }

    #[test]
    fn the_mosaic_takes_four_different_covers() {
        let covered = |cover: &str| TrackSummary { image_url: cover.into(), ..Default::default() };
        let p = ImportedPlaylist { tracks: ["a", "a", "", "b", "c", "d", "e"].map(covered).to_vec(), ..Default::default() };
        assert_eq!(p.mosaic_covers(), ["a", "b", "c", "d"]);
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
