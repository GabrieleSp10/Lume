use std::{fs, io, path::PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Port of the local loopback server. The Spotify app's Redirect URI must be
/// exactly `http://127.0.0.1:<port>/`.
pub const DEFAULT_PORT: u16 = 8898;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Client ID of the user's own Spotify developer app (public, not a secret).
    pub client_id: String,
    /// File name (without extension) of the active theme in the themes folder.
    pub theme: String,
    pub port: u16,
    /// Latest searches, newest first, offered on the empty search page.
    pub recent_searches: Vec<String>,
    /// Interface language: "it" or "en".
    pub language: String,
    pub animations: bool,
    /// What plays the music: "spotify", "youtube" or "librespot".
    pub engine: String,
    /// Full path of yt-dlp; empty to find it on the PATH.
    pub ytdlp_path: String,
    /// The first-run choice between Spotify and YouTube was made.
    pub onboarded: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { client_id: String::new(), theme: "default-dark".into(), port: DEFAULT_PORT, recent_searches: Vec::new(), language: "it".into(), animations: true, engine: "spotify".into(), ytdlp_path: String::new(), onboarded: false }
    }
}

impl AppConfig {
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Loads the config, writing a commented default file the first time.
    pub fn load_or_create() -> io::Result<Self> {
        let path = config_file();
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save()?;
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_file();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, text)
    }
}

fn dirs() -> ProjectDirs {
    ProjectDirs::from("", "", "SpotifyApp").expect("no home directory")
}

pub fn config_dir() -> PathBuf {
    dirs().config_dir().to_path_buf()
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn themes_dir() -> PathBuf {
    config_dir().join("themes")
}

/// Downloaded cover art, so it appears at once on the next run.
pub fn covers_dir() -> PathBuf {
    dirs().data_local_dir().join("covers")
}

/// Lyrics already found, so they survive an outage of the sources.
pub fn lyrics_dir() -> PathBuf {
    dirs().data_local_dir().join("lyrics")
}

/// Library data and the rate-limit state, so restarting costs no requests.
pub fn cache_dir() -> PathBuf {
    dirs().data_local_dir().join("cache")
}

/// Playlists imported by the user and songs liked without Spotify: kept on logout.
pub fn library_dir() -> PathBuf {
    dirs().data_local_dir().join("library")
}

/// The YouTube login made in the app, in the cookie format yt-dlp reads. It holds session
/// cookies: logging out of YouTube in Settings deletes it.
pub fn youtube_cookies_file() -> PathBuf {
    dirs().data_local_dir().join("youtube-cookies.txt")
}

/// Persistent browser profile for the hidden playback WebView.
pub fn webview_dir() -> PathBuf {
    dirs().data_local_dir().join("webview")
}
