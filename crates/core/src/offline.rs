//! Songs downloaded for listening offline. Only YouTube videos are kept, one file each,
//! named after the video, so a song in several playlists is stored once.

use std::{collections::HashSet, fs, path::PathBuf};

use crate::config::offline_dir;

/// URI prefix of a song that lives on YouTube.
const YOUTUBE_URI: &str = "youtube:";

/// The video of a YouTube song, checked, since it ends up in a file name.
pub fn video_id(uri: &str) -> Option<&str> {
    uri.strip_prefix(YOUTUBE_URI).filter(|id| is_video_id(id))
}

pub fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The downloaded file of a video, if it is complete.
pub fn file_for(video: &str) -> Option<PathBuf> {
    if !is_video_id(video) {
        return None;
    }
    fs::read_dir(offline_dir()).ok()?.flatten().map(|e| e.path()).find(|path| {
        let stem = path.file_stem().and_then(|s| s.to_str());
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or_default();
        stem == Some(video) && !matches!(ext, "part" | "ytdl" | "tmp")
    })
}

/// The videos downloaded so far.
pub fn downloaded() -> HashSet<String> {
    let Ok(entries) = fs::read_dir(offline_dir()) else { return HashSet::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| !matches!(path.extension().and_then(|s| s.to_str()), Some("part" | "ytdl" | "tmp")))
        .filter_map(|path| path.file_stem().and_then(|s| s.to_str()).filter(|id| is_video_id(id)).map(String::from))
        .collect()
}

/// Deletes the files of these videos.
pub fn remove(videos: &[&str]) {
    for video in videos {
        if let Some(path) = file_for(video) {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn remove_all() {
    let _ = fs::remove_dir_all(offline_dir());
}

/// Space the downloads take, in bytes.
pub fn size() -> u64 {
    let Ok(entries) = fs::read_dir(offline_dir()) else { return 0 };
    entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_youtube_videos_have_a_file_name() {
        assert_eq!(video_id("youtube:fOT0BUpITw8"), Some("fOT0BUpITw8"));
        assert_eq!(video_id("spotify:track:69kOkLUCkxIZYexIgSG8rq"), None);
        assert_eq!(video_id("youtube:../../x"), None);
        assert!(file_for("../evil.exe").is_none());
    }
}
