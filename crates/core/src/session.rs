//! What the app remembers between runs: window size, volume and what was playing.

use std::{fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTrack {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub image_url: String,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Logical size of the window when not maximized; 0 means never saved.
    pub window_width: f32,
    pub window_height: f32,
    pub maximized: bool,
    pub volume: f32,
    pub muted: bool,
    pub track: Option<SavedTrack>,
    pub position_ms: u32,
    /// Playlist or album the track was playing from.
    pub context_uri: Option<String>,
    /// Tracks that followed it when it was not playing from a context.
    pub play_uris: Vec<String>,
    /// Songs the user added to the queue that have not played yet.
    pub queue: Vec<SavedTrack>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            window_width: 0.0,
            window_height: 0.0,
            maximized: false,
            volume: 0.7,
            muted: false,
            track: None,
            position_ms: 0,
            context_uri: None,
            play_uris: Vec::new(),
            queue: Vec::new(),
        }
    }
}

fn file() -> PathBuf {
    config_dir().join("session.toml")
}

impl Session {
    /// The saved session, or a fresh one if it is missing or unreadable.
    pub fn load() -> Self {
        fs::read_to_string(file()).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        fs::create_dir_all(config_dir())?;
        let text = toml::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(file(), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_partial_files() {
        let session = Session {
            window_width: 1200.0,
            window_height: 760.0,
            muted: true,
            track: Some(SavedTrack { uri: "spotify:track:a".into(), name: "Song".into(), ..Default::default() }),
            context_uri: Some("spotify:playlist:p".into()),
            queue: vec![SavedTrack { uri: "spotify:track:b".into(), ..Default::default() }],
            ..Default::default()
        };
        let back: Session = toml::from_str(&toml::to_string_pretty(&session).unwrap()).unwrap();
        assert_eq!(back, session);
        // An older or hand-edited file keeps defaults for what it lacks.
        let partial: Session = toml::from_str("volume = 0.2\n").unwrap();
        assert_eq!(partial.volume, 0.2);
        assert!(partial.track.is_none());
    }
}
