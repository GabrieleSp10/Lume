//! Spotify Web API client (blocking; call it from worker threads, never the UI thread).
//!
//! Responses are parsed leniently from `serde_json::Value`: Spotify's Development Mode
//! changed field names in 2026 (e.g. playlist `tracks` -> `items`, entry `track` -> `item`),
//! and playlist entries can be null, so every parser tolerates both shapes.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    auth::{percent_encode, Auth, AuthError},
    cache::Store,
    limiter::{endpoint_of, Limiter, Priority, Usage},
};

const BASE: &str = "https://api.spotify.com/v1";
const PAGE: usize = 50;
const PARALLEL_PAGES: usize = 3;
/// A wait this short is sat out and the request repeated without bothering the user.
const MAX_AUTO_WAIT_S: u64 = 5;
/// Used when a 429 answer does not say how long to wait.
const DEFAULT_WAIT_S: u64 = 5;
/// The same search within this time costs no request (typing back and forth, language change).
const SEARCH_FRESH: Duration = Duration::from_secs(600);
const LIKES_KEY: &str = "likes";
/// Likes can change on other devices, so what is known on disk is asked again after this.
const LIKES_FRESH: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("network error: {0}")]
    Network(String),
    #[error("Spotify error {status}: {message}")]
    Http { status: u16, message: String },
    #[error("Spotify asked to wait {seconds} seconds")]
    RateLimited { seconds: u64 },
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Http { status, .. } => Some(*status),
            ApiError::RateLimited { .. } => Some(429),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackSummary {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub image_url: String,
    pub duration_ms: u32,
    pub explicit: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistSummary {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub owner: String,
    pub image_url: String,
    pub track_count: Option<u32>,
    /// Changes whenever the playlist does; a list saved with the same one is still right.
    pub snapshot_id: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlbumSummary {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub image_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub tracks: Vec<TrackSummary>,
    pub albums: Vec<AlbumSummary>,
    pub playlists: Vec<PlaylistSummary>,
}

#[derive(Debug, Clone, Default)]
pub struct QueueInfo {
    pub current: Option<TrackSummary>,
    pub upcoming: Vec<TrackSummary>,
}

pub struct Api {
    auth: Arc<Auth>,
    agent: ureq::Agent,
    limiter: Limiter,
    store: Store,
    /// Liked state of songs already asked about or changed; kept on disk between runs.
    liked: Mutex<HashMap<String, bool>>,
    searches: Mutex<HashMap<String, (Instant, SearchResults)>>,
    /// The newer library endpoints failed once: go straight to the older ones.
    legacy_library: AtomicBool,
}

impl Api {
    pub fn new(auth: Arc<Auth>) -> Self {
        let store = Store::default();
        let liked = store.load(LIKES_KEY).filter(|c| c.age < LIKES_FRESH).map(|c| c.value).unwrap_or_default();
        Self {
            auth,
            agent: crate::http::agent(Duration::from_secs(20)),
            limiter: Limiter::load(),
            store,
            liked: Mutex::new(liked),
            searches: Mutex::default(),
            legacy_library: AtomicBool::new(false),
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn usage(&self) -> Usage {
        self.limiter.usage()
    }

    /// Seconds until Spotify lets a song start again (0 when it does now).
    pub fn playback_blocked_seconds(&self) -> u64 {
        self.limiter.playback_blocked_seconds()
    }

    /// Drops what belongs to the account that logged out.
    pub fn forget_account(&self) {
        self.store.clear();
        self.liked.lock().unwrap().clear();
        self.searches.lock().unwrap().clear();
    }

    /// A new Client ID is a new Spotify app, with its own quota.
    pub fn forget_limits(&self) {
        self.limiter.reset();
    }

    // ---- account ----

    pub fn display_name(&self) -> Result<String, ApiError> {
        let me = self.get("/me")?;
        Ok(str_at(&me, "display_name").or_else(|| str_at(&me, "id")).unwrap_or_default())
    }

    // ---- library ----

    pub fn playlists(&self, max: usize) -> Result<Vec<PlaylistSummary>, ApiError> {
        let items = self.get_paged("/me/playlists", "", max)?;
        Ok(items.iter().filter_map(parse_playlist).collect())
    }

    pub fn playlist_tracks(&self, id: &str, max: usize) -> Result<Vec<TrackSummary>, ApiError> {
        let items = self.get_paged(&format!("/playlists/{id}/items"), "", max)?;
        Ok(items.iter().filter_map(|e| parse_track(e.get("item").or_else(|| e.get("track"))?)).collect())
    }

    pub fn liked_tracks(&self, max: usize) -> Result<Vec<TrackSummary>, ApiError> {
        let items = self.get_paged("/me/tracks", "", max)?;
        let tracks: Vec<TrackSummary> = items.iter().filter_map(|e| parse_track(e.get("track").or_else(|| e.get("item"))?)).collect();
        // Everything in this list is liked: no need to ask about these songs again.
        self.liked.lock().unwrap().extend(tracks.iter().map(|t| (t.uri.clone(), true)));
        Ok(tracks)
    }

    pub fn recently_played(&self) -> Result<Vec<TrackSummary>, ApiError> {
        let v = self.get("/me/player/recently-played?limit=50")?;
        let mut seen = std::collections::HashSet::new();
        Ok(items_of(&v)
            .iter()
            .filter_map(|e| parse_track(e.get("track")?))
            .filter(|t| seen.insert(t.uri.clone()))
            .collect())
    }

    pub fn album_tracks(&self, id: &str, album: &AlbumSummary) -> Result<Vec<TrackSummary>, ApiError> {
        let items = self.get_paged(&format!("/albums/{id}/tracks"), "", 100)?;
        Ok(items
            .iter()
            .filter_map(parse_track)
            .map(|mut t| {
                // Album track objects carry no album block; fill it in from the album.
                t.album = album.name.clone();
                t.image_url = album.image_url.clone();
                t
            })
            .collect())
    }

    // ---- search ----

    pub fn search(&self, query: &str) -> Result<SearchResults, ApiError> {
        let key = query.to_lowercase();
        {
            let mut searches = self.searches.lock().unwrap();
            searches.retain(|_, (at, _)| at.elapsed() < SEARCH_FRESH);
            if let Some((_, results)) = searches.get(&key) {
                return Ok(results.clone());
            }
        }
        let v = self.get(&format!("/search?q={}&type=track,album,playlist&limit=10", percent_encode(query)))?;
        let list = |key: &str| v.get(key).map(items_of).unwrap_or_default();
        let results = SearchResults {
            tracks: list("tracks").iter().filter_map(parse_track).collect(),
            albums: list("albums").iter().filter_map(parse_album).collect(),
            playlists: list("playlists").iter().filter_map(parse_playlist).collect(),
        };
        self.searches.lock().unwrap().insert(key, (Instant::now(), results.clone()));
        Ok(results)
    }

    // ---- playback ----

    /// Starts playback on `device_id`. `body` is what Spotify's play endpoint takes.
    pub fn play(&self, device_id: &str, body: &Value) -> Result<(), ApiError> {
        let path = format!("/me/player/play?device_id={}", percent_encode(device_id));
        self.send(Priority::Playback, "PUT", &path, Some(&body.to_string())).map(|_| ())
    }

    pub fn set_shuffle(&self, device_id: &str, on: bool) -> Result<(), ApiError> {
        let path = format!("/me/player/shuffle?state={on}&device_id={}", percent_encode(device_id));
        self.send(Priority::Playback, "PUT", &path, None).map(|_| ())
    }

    /// `state` is "off", "context" or "track".
    pub fn set_repeat(&self, device_id: &str, state: &str) -> Result<(), ApiError> {
        let path = format!("/me/player/repeat?state={state}&device_id={}", percent_encode(device_id));
        self.send(Priority::Playback, "PUT", &path, None).map(|_| ())
    }

    // ---- queue ----

    pub fn queue(&self) -> Result<QueueInfo, ApiError> {
        let v = self.get("/me/player/queue")?;
        Ok(QueueInfo {
            current: v.get("currently_playing").and_then(parse_track),
            upcoming: v.get("queue").and_then(Value::as_array).map(|a| a.iter().filter_map(parse_track).collect()).unwrap_or_default(),
        })
    }

    /// Adds a track to the queue of `device_id` (or the active device).
    pub fn add_to_queue(&self, uri: &str, device_id: Option<&str>) -> Result<(), ApiError> {
        let device = device_id.map(|d| format!("&device_id={}", percent_encode(d))).unwrap_or_default();
        self.send(Priority::Playback, "POST", &format!("/me/player/queue?uri={}{device}", percent_encode(uri)), None).map(|_| ())
    }

    // ---- likes ----

    /// For each URI, whether it is saved in the user's library. Only songs not seen yet are
    /// asked about, so browsing the same lists costs no requests. This is background work:
    /// when the quota is busy, songs not known yet simply count as not liked for now.
    pub fn contains(&self, uris: &[String]) -> Result<Vec<bool>, ApiError> {
        let unknown: Vec<String> = {
            let known = self.liked.lock().unwrap();
            let mut unknown: Vec<String> = uris.iter().filter(|u| !known.contains_key(*u)).cloned().collect();
            unknown.sort();
            unknown.dedup();
            unknown
        };
        for chunk in unknown.chunks(40) {
            let Ok(flags) = self.contains_chunk(chunk) else { break };
            let mut known = self.liked.lock().unwrap();
            known.extend(chunk.iter().cloned().zip(flags.into_iter().chain(std::iter::repeat(false))));
        }
        if !unknown.is_empty() {
            self.save_likes();
        }
        let known = self.liked.lock().unwrap();
        Ok(uris.iter().map(|u| known.get(u).copied().unwrap_or(false)).collect())
    }

    fn contains_chunk(&self, chunk: &[String]) -> Result<Vec<bool>, ApiError> {
        let get = |path: String| self.send(Priority::Background, "GET", &path, None).map(|v| v.unwrap_or(Value::Null));
        let older = || get(format!("/me/tracks/contains?ids={}", ids_of(chunk).join(",")));
        let v = if self.legacy_library.load(Ordering::Relaxed) {
            older()?
        } else {
            let joined = chunk.iter().map(|u| percent_encode(u)).collect::<Vec<_>>().join(",");
            match get(format!("/me/library/contains?uris={joined}")) {
                Ok(v) => v,
                // Older endpoint, in case the new library API is unavailable.
                Err(e) if matches!(e.status(), Some(400 | 404)) => {
                    self.legacy_library.store(true, Ordering::Relaxed);
                    older()?
                }
                Err(e) => return Err(e),
            }
        };
        Ok(v.as_array().map(|a| a.iter().map(|b| b.as_bool().unwrap_or(false)).collect()).unwrap_or_default())
    }

    pub fn set_liked(&self, uri: &str, liked: bool) -> Result<(), ApiError> {
        let method = if liked { "PUT" } else { "DELETE" };
        let older = format!("/me/tracks?ids={}", ids_of(&[uri.to_string()]).join(","));
        if self.legacy_library.load(Ordering::Relaxed) {
            self.send(Priority::Normal, method, &older, None)?;
        } else {
            match self.send(Priority::Normal, method, &format!("/me/library?uris={}", percent_encode(uri)), None) {
                Err(e) if matches!(e.status(), Some(400 | 404)) => {
                    self.legacy_library.store(true, Ordering::Relaxed);
                    self.send(Priority::Normal, method, &older, None)?;
                }
                other => {
                    other?;
                }
            }
        }
        self.liked.lock().unwrap().insert(uri.to_string(), liked);
        self.save_likes();
        Ok(())
    }

    fn save_likes(&self) {
        self.store.save(LIKES_KEY, &*self.liked.lock().unwrap());
    }

    // ---- plumbing ----

    fn get(&self, path: &str) -> Result<Value, ApiError> {
        Ok(self.send(Priority::Normal, "GET", path, None)?.unwrap_or(Value::Null))
    }

    /// Fetches `limit`/`offset` pages up to `max` items. The first page says how many there
    /// are; the rest then come in parallel, a few at a time, and keep their order.
    fn get_paged(&self, path: &str, extra: &str, max: usize) -> Result<Vec<Value>, ApiError> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let page = |offset: usize, limit: usize| self.get(&format!("{path}{sep}limit={limit}&offset={offset}{extra}"));
        let first = page(0, PAGE.min(max))?;
        let mut out = items_of(&first);
        let total = first.get("total").and_then(Value::as_u64).map_or(out.len(), |n| n as usize).min(max);
        let offsets: Vec<usize> = (PAGE..total).step_by(PAGE).collect();
        for batch in offsets.chunks(PARALLEL_PAGES) {
            let pages = thread::scope(|scope| {
                let workers: Vec<_> = batch.iter().map(|&offset| scope.spawn(move || page(offset, PAGE.min(total - offset)))).collect();
                workers.into_iter().map(|w| w.join().expect("page worker panicked")).collect::<Vec<_>>()
            });
            for result in pages {
                out.extend(items_of(&result?));
            }
        }
        Ok(out)
    }

    /// Sends a request, keeping to the rate limit: on a 429 nothing goes out for as long as
    /// Spotify asks, and a short wait is sat out and the request repeated once.
    fn send(&self, priority: Priority, method: &str, path: &str, body: Option<&str>) -> Result<Option<Value>, ApiError> {
        let endpoint = endpoint_of(path);
        let mut retried = false;
        loop {
            self.limiter.acquire(priority, &endpoint).map_err(|seconds| ApiError::RateLimited { seconds })?;
            let token = self.auth.access_token()?;
            let url = format!("{BASE}{path}");
            let bearer = format!("Bearer {token}");
            let result = match method {
                "GET" => self.agent.get(&url).header("Authorization", &bearer).call(),
                "DELETE" => self.agent.delete(&url).header("Authorization", &bearer).call(),
                "PUT" => match body {
                    Some(body) => self.agent.put(&url).header("Authorization", &bearer).header("Content-Type", "application/json").send(body),
                    None => self.agent.put(&url).header("Authorization", &bearer).send_empty(),
                },
                _ => self.agent.post(&url).header("Authorization", &bearer).send_empty(),
            };
            let mut resp = result.map_err(|e| ApiError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            if status == 429 {
                let seconds = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(DEFAULT_WAIT_S);
                let wait = Duration::from_secs(seconds);
                self.limiter.block(&endpoint, seconds);
                eprintln!("429 on {endpoint}: Retry-After {seconds}s, usage {:?}", self.limiter.usage());
                if retried || seconds > MAX_AUTO_WAIT_S {
                    return Err(ApiError::RateLimited { seconds });
                }
                retried = true;
                thread::sleep(wait);
                continue;
            }
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            if status >= 400 {
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(String::from))
                    .unwrap_or(body);
                return Err(ApiError::Http { status, message });
            }
            return Ok(if body.trim().is_empty() { None } else { serde_json::from_str(&body).ok() });
        }
    }
}

// ---- parsing ----

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

/// `items` of a paging object, or the value itself if it is already an array.
fn items_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        _ => v.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
    }
}

fn ids_of(uris: &[String]) -> Vec<String> {
    uris.iter().filter_map(|u| u.rsplit(':').next().map(String::from)).collect()
}

/// Picks the image closest to 300px (covers are listed from largest to smallest).
pub fn pick_image(images: Option<&Value>) -> String {
    let Some(list) = images.and_then(Value::as_array) else { return String::new() };
    list.iter()
        .filter_map(|i| Some((i.get("url")?.as_str()?, i.get("width").and_then(Value::as_i64).unwrap_or(300))))
        .min_by_key(|(_, w)| (w - 300).abs())
        .map(|(u, _)| u.to_string())
        .unwrap_or_default()
}

fn names_of(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| str_at(x, "name")).collect())
        .unwrap_or_default()
}

pub fn parse_track(v: &Value) -> Option<TrackSummary> {
    let uri = str_at(v, "uri")?;
    if !uri.starts_with("spotify:track:") {
        return None; // episodes and local files are not supported yet
    }
    let album = v.get("album");
    Some(TrackSummary {
        name: str_at(v, "name")?,
        artists: names_of(v.get("artists")),
        album: album.and_then(|a| str_at(a, "name")).unwrap_or_default(),
        image_url: pick_image(album.and_then(|a| a.get("images"))),
        duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
        explicit: v.get("explicit").and_then(Value::as_bool).unwrap_or(false),
        uri,
    })
}

pub fn parse_playlist(v: &Value) -> Option<PlaylistSummary> {
    let uri = str_at(v, "uri")?;
    let track_count = ["tracks", "items"]
        .iter()
        .find_map(|k| v.get(k)?.get("total")?.as_u64())
        .map(|n| n as u32);
    Some(PlaylistSummary {
        id: str_at(v, "id").or_else(|| uri.rsplit(':').next().map(String::from))?,
        name: str_at(v, "name")?,
        owner: v.get("owner").and_then(|o| str_at(o, "display_name").or_else(|| str_at(o, "id"))).unwrap_or_default(),
        image_url: pick_image(v.get("images")),
        track_count,
        snapshot_id: str_at(v, "snapshot_id").unwrap_or_default(),
        uri,
    })
}

pub fn parse_album(v: &Value) -> Option<AlbumSummary> {
    let uri = str_at(v, "uri")?;
    Some(AlbumSummary {
        id: str_at(v, "id").or_else(|| uri.rsplit(':').next().map(String::from))?,
        name: str_at(v, "name")?,
        artists: names_of(v.get("artists")),
        image_url: pick_image(v.get("images")),
        uri,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_track_and_picks_medium_image() {
        let v = json!({
            "uri": "spotify:track:abc", "name": "Song", "duration_ms": 215000, "explicit": true,
            "artists": [{"name": "A"}, {"name": "B"}],
            "album": {"name": "Alb", "images": [
                {"url": "big", "width": 640}, {"url": "mid", "width": 300}, {"url": "small", "width": 64}]}
        });
        let t = parse_track(&v).unwrap();
        assert_eq!(t.artists, ["A", "B"]);
        assert_eq!(t.image_url, "mid");
        assert!(t.explicit);
        assert_eq!(t.duration_ms, 215000);
    }

    #[test]
    fn skips_episodes_and_nulls() {
        assert!(parse_track(&json!({"uri": "spotify:episode:x", "name": "Pod"})).is_none());
        assert!(parse_track(&Value::Null).is_none());
    }

    #[test]
    fn playlist_accepts_old_and_new_shapes() {
        let old = json!({"id": "p1", "uri": "spotify:playlist:p1", "name": "Mix", "owner": {"display_name": "Me"}, "tracks": {"total": 12}});
        let new = json!({"id": "p2", "uri": "spotify:playlist:p2", "name": "Mix2", "owner": {"id": "me"}, "items": {"total": 3}});
        assert_eq!(parse_playlist(&old).unwrap().track_count, Some(12));
        assert_eq!(parse_playlist(&new).unwrap().track_count, Some(3));
        assert_eq!(parse_playlist(&new).unwrap().owner, "me");
    }

    #[test]
    fn paging_object_and_plain_array() {
        assert_eq!(items_of(&json!({"items": [1, 2]})).len(), 2);
        assert_eq!(items_of(&json!([1, 2, 3])).len(), 3);
        assert!(items_of(&Value::Null).is_empty());
    }

    #[test]
    fn extracts_ids_from_uris() {
        assert_eq!(ids_of(&["spotify:track:abc".into(), "spotify:track:def".into()]), ["abc", "def"]);
    }
}
