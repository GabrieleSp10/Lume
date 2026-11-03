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
    /// YouTube songs may be downloaded for listening offline. Off unless the user turns it on.
    pub offline: bool,
    /// "high" (the best audio YouTube has) or "saver" (about half the size).
    pub download_quality: String,
    /// Three songs at a time instead of one; only with a YouTube account linked.
    pub fast_downloads: bool,
    /// Closing the window keeps the music going, with Lume in the notification area.
    pub keep_in_tray: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { client_id: String::new(), theme: "default-dark".into(), port: DEFAULT_PORT, recent_searches: Vec::new(), language: "it".into(), animations: true, engine: "spotify".into(), ytdlp_path: String::new(), onboarded: false, offline: false, download_quality: "high".into(), fast_downloads: false, keep_in_tray: false }
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

const NAME: &str = "Lume";
/// The name the app's folders had before it was called Lume.
const OLD_NAME: &str = "SpotifyApp";

fn dirs() -> ProjectDirs {
    ProjectDirs::from("", "", NAME).expect("no home directory")
}

/// Moves the folders of versions before the rename (settings, library, logins) to the new
/// name. Call before anything reads or creates them.
pub fn move_old_folders() {
    let (Some(old), Some(new)) = (ProjectDirs::from("", "", OLD_NAME), ProjectDirs::from("", "", NAME)) else { return };
    let pairs = [(old.config_dir().parent(), new.config_dir().parent()), (old.data_local_dir().parent(), new.data_local_dir().parent())];
    for (from, to) in pairs {
        let (Some(from), Some(to)) = (from, to) else { continue };
        if from.exists() && !to.exists() {
            if let Err(e) = fs::rename(from, to) {
                eprintln!("{} not moved to {}: {e}", from.display(), to.display());
            }
        }
    }
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

/// yt-dlp and Deno as the app installs them for YouTube.
pub fn tools_dir() -> PathBuf {
    dirs().data_local_dir().join("tools")
}

/// Playlists imported by the user and songs liked without Spotify: kept on logout.
/// Songs downloaded for listening offline, one file per YouTube video.
pub fn offline_dir() -> PathBuf {
    dirs().data_local_dir().join("offline")
}

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
