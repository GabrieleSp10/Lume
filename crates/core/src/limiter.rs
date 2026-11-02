//! Spends the app's Spotify quota. Every Web API request of the app passes through here, so
//! starting a song never competes with background work for the same 30-second window.
//!
//! Spotify limits in two ways, told apart by the length of Retry-After. A short wait is the
//! rolling window, which covers the whole app. A wait of hours is a quota on that endpoint
//! alone: on 2026-11-01 `/me/tracks` answered 429 for 16 hours while `/me/player/play`,
//! `/me/playlists` and `/search` kept working, so such a block must not stop everything.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::config::cache_dir;

/// Spotify counts requests over a rolling window of this length.
const WINDOW: Duration = Duration::from_secs(30);
/// Smallest gap between two requests, so bursts never reach the rate limit.
const REQUEST_GAP: Duration = Duration::from_millis(150);
/// Above this many requests in the window, background work is dropped.
const BACKGROUND_BUDGET: usize = 25;
/// Above this many, what the user asked for waits; only playback still goes out.
const NORMAL_BUDGET: usize = 60;
/// A longer Retry-After concerns the endpoint, not the rolling window.
const WINDOW_BLOCK_MAX_S: u64 = 120;
const PLAY_ENDPOINT: &str = "/me/player/play";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Starting a song: the reason the app exists, never held back by the budget.
    Playback,
    /// Something the user is waiting for: slowed down, never dropped.
    Normal,
    /// Nice to have, like hearts on a list: dropped when the window is busy.
    Background,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub last_window: usize,
    pub total: u64,
    /// The endpoint called most since the app started, with its count.
    pub busiest: Option<(String, u64)>,
    /// Endpoints Spotify refuses for now, however long.
    pub blocked_endpoints: usize,
    /// Seconds until the last of them is accepted again (0 when none is blocked).
    pub blocked_seconds: u64,
    /// Starting a song is refused too.
    pub playback_blocked: bool,
}

struct State {
    sent: VecDeque<Instant>,
    next_slot: Instant,
    /// Nothing goes out until then: the rolling window is used up.
    paused_until: Option<Instant>,
    /// Endpoints whose own quota is used up, and until when.
    blocked: HashMap<String, Instant>,
    total: u64,
    by_endpoint: HashMap<String, u64>,
}

pub struct Limiter {
    state: Mutex<State>,
    /// Remembers blocks across restarts: asking early only makes them longer.
    file: Option<PathBuf>,
}

impl Limiter {
    pub fn load() -> Self {
        let file = cache_dir().join("blocked-endpoints.json");
        let saved: HashMap<String, u64> = fs::read_to_string(&file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        let now = unix_now();
        let blocked = saved
            .into_iter()
            .filter(|(_, until)| *until > now)
            .map(|(endpoint, until)| (endpoint, Instant::now() + Duration::from_secs(until - now)))
            .collect();
        Self::with_state(blocked, Some(file))
    }

    fn with_state(blocked: HashMap<String, Instant>, file: Option<PathBuf>) -> Self {
        let state = State { sent: VecDeque::new(), next_slot: Instant::now(), paused_until: None, blocked, total: 0, by_endpoint: HashMap::new() };
        Self { state: Mutex::new(state), file }
    }

    /// Waits for this request's turn. Err carries the seconds to wait when it may not go out.
    pub fn acquire(&self, priority: Priority, endpoint: &str) -> Result<(), u64> {
        let start = self.reserve(priority, endpoint, Instant::now())?;
        thread::sleep(start.saturating_duration_since(Instant::now()));
        Ok(())
    }

    fn reserve(&self, priority: Priority, endpoint: &str, now: Instant) -> Result<Instant, u64> {
        let mut s = self.state.lock().unwrap();
        let until = s.paused_until.into_iter().chain(s.blocked.get(endpoint).copied()).max();
        if let Some(left) = until.map(|until| until.saturating_duration_since(now)).filter(|left| !left.is_zero()) {
            return Err(left.as_secs() + 1);
        }
        while s.sent.front().is_some_and(|at| now.saturating_duration_since(*at) >= WINDOW) {
            s.sent.pop_front();
        }
        let mut start = s.next_slot.max(now);
        let budget = match priority {
            Priority::Playback => None,
            Priority::Normal => Some(NORMAL_BUDGET),
            Priority::Background => Some(BACKGROUND_BUDGET),
        };
        if let Some(budget) = budget.filter(|b| s.sent.len() >= *b) {
            let free_at = s.sent[s.sent.len() - budget] + WINDOW;
            if priority == Priority::Background {
                return Err(free_at.saturating_duration_since(now).as_secs() + 1);
            }
            start = start.max(free_at);
        }
        s.next_slot = start + REQUEST_GAP;
        s.sent.push_back(start);
        s.total += 1;
        *s.by_endpoint.entry(endpoint.to_string()).or_default() += 1;
        Ok(start)
    }

    /// Spotify answered 429 on `endpoint`.
    pub fn block(&self, endpoint: &str, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds);
        let mut s = self.state.lock().unwrap();
        if seconds <= WINDOW_BLOCK_MAX_S {
            s.paused_until = Some(until);
            return;
        }
        s.blocked.insert(endpoint.to_string(), until);
        Self::save(&self.file, &s.blocked);
    }

    fn save(file: &Option<PathBuf>, blocked: &HashMap<String, Instant>) {
        let Some(file) = file else { return };
        let now = Instant::now();
        let until: HashMap<&String, u64> =
            blocked.iter().map(|(endpoint, at)| (endpoint, unix_now() + at.saturating_duration_since(now).as_secs())).collect();
        if let Some(dir) = file.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(&until) {
            let _ = fs::write(file, text);
        }
    }

    /// A different Spotify app has its own quota.
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        s.paused_until = None;
        s.blocked.clear();
        Self::save(&self.file, &s.blocked);
    }

    /// Seconds until a song can be started through the Web API (0 when it can now).
    pub fn playback_blocked_seconds(&self) -> u64 {
        let s = self.state.lock().unwrap();
        let until = s.paused_until.into_iter().chain(s.blocked.get(PLAY_ENDPOINT).copied()).max();
        until.map(|u| u.saturating_duration_since(Instant::now())).filter(|left| !left.is_zero()).map_or(0, |left| left.as_secs() + 1)
    }

    pub fn usage(&self) -> Usage {
        let playback_blocked = self.playback_blocked_seconds() > 0;
        let now = Instant::now();
        let s = self.state.lock().unwrap();
        let active: Vec<Instant> = s.blocked.values().copied().chain(s.paused_until).filter(|until| *until > now).collect();
        Usage {
            last_window: s.sent.iter().filter(|at| now.saturating_duration_since(**at) < WINDOW).count(),
            total: s.total,
            busiest: s.by_endpoint.iter().max_by_key(|(_, n)| **n).map(|(e, n)| (e.clone(), *n)),
            blocked_endpoints: s.blocked.values().filter(|until| **until > now).count(),
            blocked_seconds: active.iter().max().map_or(0, |until| until.saturating_duration_since(now).as_secs() + 1),
            playback_blocked,
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The path without its query and ids, so calls to the same endpoint count together.
pub fn endpoint_of(path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let is_id = |s: &str| s.len() == 22 && s.chars().all(|c| c.is_ascii_alphanumeric());
    path.split('/').map(|part| if is_id(part) { "*" } else { part }).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(limiter: &Limiter, count: usize, at: Instant) {
        for _ in 0..count {
            limiter.reserve(Priority::Playback, "/x", at).unwrap();
        }
    }

    #[test]
    fn background_is_dropped_first_and_playback_never() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        let now = Instant::now();
        fill(&limiter, BACKGROUND_BUDGET, now);
        assert!(limiter.reserve(Priority::Background, "/hearts", now).is_err());
        assert!(limiter.reserve(Priority::Normal, "/list", now).is_ok());
        fill(&limiter, NORMAL_BUDGET, now);
        assert!(limiter.reserve(Priority::Playback, "/play", now).is_ok());
    }

    #[test]
    fn normal_requests_wait_for_room_in_the_window() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        let now = Instant::now();
        fill(&limiter, NORMAL_BUDGET, now);
        let start = limiter.reserve(Priority::Normal, "/list", now).unwrap();
        assert!(start >= now + WINDOW);
    }

    #[test]
    fn a_short_block_stops_everything_until_it_ends() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        limiter.block("/a", 30);
        let refused = limiter.reserve(Priority::Playback, PLAY_ENDPOINT, Instant::now()).unwrap_err();
        assert!((29..=31).contains(&refused));
        assert!((29..=31).contains(&limiter.playback_blocked_seconds()));
        limiter.reset();
        assert_eq!(limiter.playback_blocked_seconds(), 0);
        assert!(limiter.reserve(Priority::Playback, PLAY_ENDPOINT, Instant::now()).is_ok());
    }

    #[test]
    fn a_long_block_only_stops_its_endpoint() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        limiter.block("/me/tracks", 59_000);
        let now = Instant::now();
        assert!(limiter.reserve(Priority::Normal, "/me/tracks", now).is_err());
        assert!(limiter.reserve(Priority::Playback, PLAY_ENDPOINT, now).is_ok());
        assert!(limiter.reserve(Priority::Normal, "/me/playlists", now).is_ok());
        assert_eq!(limiter.playback_blocked_seconds(), 0);
        let usage = limiter.usage();
        assert_eq!(usage.blocked_endpoints, 1);
        assert!(usage.blocked_seconds > 58_000);
        assert!(!usage.playback_blocked);
    }

    #[test]
    fn a_block_on_starting_songs_is_reported() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        limiter.block(PLAY_ENDPOINT, 3600);
        assert!(limiter.playback_blocked_seconds() > 3500);
        assert!(limiter.usage().playback_blocked);
    }

    #[test]
    fn a_block_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("lume-limiter-{}", rand::random::<u32>()));
        let file = dir.join("blocked-endpoints.json");
        Limiter::with_state(HashMap::new(), Some(file.clone())).block("/me/tracks", 600);
        let saved: HashMap<String, u64> = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert!(saved["/me/tracks"] > unix_now() + 590);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn usage_names_the_busiest_endpoint() {
        let limiter = Limiter::with_state(HashMap::new(), None);
        let now = Instant::now();
        limiter.reserve(Priority::Normal, "/a", now).unwrap();
        limiter.reserve(Priority::Normal, "/b", now).unwrap();
        limiter.reserve(Priority::Normal, "/b", now).unwrap();
        let usage = limiter.usage();
        assert_eq!(usage.total, 3);
        assert_eq!(usage.busiest, Some(("/b".to_string(), 2)));
    }

    #[test]
    fn endpoints_group_by_shape() {
        assert_eq!(endpoint_of("/playlists/37i9dQZF1DXcBWIGoYBM5M/items?limit=50"), "/playlists/*/items");
        assert_eq!(endpoint_of("/me/tracks?limit=1"), "/me/tracks");
    }
}
