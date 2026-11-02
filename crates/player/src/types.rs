use std::time::Instant;

use serde_json::{json, Value};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub image_url: String,
    /// 0 when unknown.
    pub duration_ms: u32,
}

#[derive(Debug, Clone)]
pub struct PlaybackState {
    pub paused: bool,
    /// Position at `updated_at`; while playing, the UI interpolates from there.
    pub position_ms: u32,
    pub duration_ms: u32,
    pub shuffle: bool,
    /// 0 = off, 1 = context, 2 = track.
    pub repeat_mode: u8,
    pub track: Option<Track>,
    /// Playlist or album being played, when there is one.
    pub context_uri: Option<String>,
    /// The song is being found or buffered: no sound yet.
    pub loading: bool,
    pub updated_at: Instant,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Result of the browser login (Err carries a message for the user).
    Login(Result<(), String>),
    /// The hidden page is up (None) or Spotify's player inside it connected (Some).
    Ready { device_id: Option<String> },
    State(PlaybackState),
    Error(String),
    /// Another engine took over the song because the one in use failed. `blocked_seconds`
    /// is how long Spotify asked to wait, 0 if it failed for another reason.
    Switched { to: EngineKind, blocked_seconds: u64 },
    /// The YouTube account was connected (true) or disconnected (false).
    YouTubeAccount(bool),
}

/// What plays the music.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Spotify,
    YouTube,
    Librespot,
}

impl EngineKind {
    pub fn from_name(name: &str) -> Self {
        match name {
            "youtube" => Self::YouTube,
            "librespot" => Self::Librespot,
            _ => Self::Spotify,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Spotify => "spotify",
            Self::YouTube => "youtube",
            Self::Librespot => "librespot",
        }
    }
}

/// What to play. Every engine reads what it needs: Spotify the context or URIs, the local
/// engines the names to search for and the order.
#[derive(Debug, Clone, Default)]
pub struct PlayRequest {
    pub tracks: Vec<Track>,
    /// Index in `tracks` to start from; None leaves it to the engine.
    pub start: Option<usize>,
    /// Playlist or album the tracks belong to.
    pub context: Option<String>,
    /// Where in the first song to start.
    pub position_ms: u32,
}

#[derive(Debug, Clone)]
pub enum Command {
    Play(PlayRequest),
    Pause,
    Resume,
    Toggle,
    Next,
    Prev,
    SeekMs(u32),
    /// 0.0 ..= 1.0
    Volume(f32),
    Shuffle(bool),
    /// 0 = off, 1 = context, 2 = track.
    Repeat(u8),
    Queue(Track),
}

pub(crate) fn repeat_name(mode: u8) -> &'static str {
    match mode {
        0 => "off",
        1 => "context",
        _ => "track",
    }
}

impl Command {
    /// The call for Spotify's player in the hidden page; None for what needs the Web API.
    pub(crate) fn to_page_json(&self) -> Option<String> {
        let v = match self {
            Command::Pause => json!({ "cmd": "pause" }),
            Command::Resume => json!({ "cmd": "resume" }),
            Command::Toggle => json!({ "cmd": "toggle" }),
            Command::Next => json!({ "cmd": "next" }),
            Command::Prev => json!({ "cmd": "prev" }),
            Command::SeekMs(ms) => json!({ "cmd": "seek", "position_ms": ms }),
            Command::Volume(v) => json!({ "cmd": "volume", "value": v.clamp(0.0, 1.0) }),
            Command::Play(_) | Command::Shuffle(_) | Command::Repeat(_) | Command::Queue(_) => return None,
        };
        Some(v.to_string())
    }
}

/// Audio of a local engine, as told by the hidden page or librespot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AudioEvent {
    Playing { position_ms: u32, duration_ms: u32 },
    Paused { position_ms: u32 },
    Ended,
    Failed(String),
}

#[derive(Debug, Clone)]
pub(crate) enum PageMessage {
    /// The page runs, whether or not Spotify's player connects.
    Loaded,
    Audio { token: u64, event: AudioEvent },
    Event(Event),
}

/// Decodes a message posted by `player.html`.
pub(crate) fn parse_page_message(body: &str) -> Option<PageMessage> {
    let v: Value = serde_json::from_str(body).ok()?;
    let ms = |key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0) as u32;
    match v.get("type")?.as_str()? {
        "loaded" => Some(PageMessage::Loaded),
        "ready" => Some(PageMessage::Event(Event::Ready { device_id: Some(v.get("device_id")?.as_str()?.to_string()) })),
        "error" => Some(PageMessage::Event(Event::Error(v.get("message")?.as_str()?.to_string()))),
        "audio" => {
            let event = match v.get("event")?.as_str()? {
                "playing" => AudioEvent::Playing { position_ms: ms("position_ms"), duration_ms: ms("duration_ms") },
                "paused" => AudioEvent::Paused { position_ms: ms("position_ms") },
                "ended" => AudioEvent::Ended,
                "error" => AudioEvent::Failed(format!("audio: {}", str_of(&v, "message"))),
                _ => return None,
            };
            Some(PageMessage::Audio { token: v.get("token")?.as_u64()?, event })
        }
        "state" => {
            let track = v.get("track").map(|t| Track {
                uri: str_of(t, "uri"),
                name: str_of(t, "name"),
                album: str_of(t, "album"),
                image_url: str_of(t, "image_url"),
                artists: t
                    .get("artists")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
            });
            Some(PageMessage::Event(Event::State(PlaybackState {
                paused: v.get("paused").and_then(Value::as_bool).unwrap_or(true),
                position_ms: v.get("position_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
                duration_ms: v.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) as u32,
                shuffle: v.get("shuffle").and_then(Value::as_bool).unwrap_or(false),
                repeat_mode: v.get("repeat_mode").and_then(Value::as_u64).unwrap_or(0) as u8,
                track,
                context_uri: v.get("context_uri").and_then(Value::as_str).filter(|c| !c.is_empty()).map(String::from),
                loading: false,
                updated_at: Instant::now(),
            })))
        }
        _ => None,
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_message() {
        let msg = r#"{"type":"state","paused":false,"position_ms":1200,"duration_ms":210000,
            "shuffle":true,"repeat_mode":1,
            "track":{"uri":"spotify:track:x","name":"Song","artists":["A","B"],"album":"Alb","image_url":"u"}}"#;
        match parse_page_message(msg) {
            Some(PageMessage::Event(Event::State(s))) => {
                assert!(!s.paused && s.shuffle);
                assert_eq!(s.position_ms, 1200);
                assert_eq!(s.track.unwrap().artists, vec!["A", "B"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn rejects_garbage_and_serializes_commands() {
        assert!(parse_page_message("not json").is_none());
        assert!(parse_page_message(r#"{"type":"nope"}"#).is_none());
        assert_eq!(Command::Pause.to_page_json().unwrap(), r#"{"cmd":"pause"}"#);
        assert!(Command::Repeat(2).to_page_json().is_none());
        assert_eq!(repeat_name(2), "track");
    }

    #[test]
    fn parses_audio_messages() {
        let msg = r#"{"type":"audio","token":7,"event":"playing","position_ms":1200.5,"duration_ms":210000.2}"#;
        match parse_page_message(msg) {
            Some(PageMessage::Audio { token: 7, event: AudioEvent::Playing { position_ms: 1200, duration_ms: 210000 } }) => {}
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(parse_page_message(r#"{"type":"loaded"}"#), Some(PageMessage::Loaded)));
    }

    #[test]
    fn engine_names_roundtrip() {
        for kind in [EngineKind::Spotify, EngineKind::YouTube, EngineKind::Librespot] {
            assert_eq!(EngineKind::from_name(kind.name()), kind);
        }
        assert_eq!(EngineKind::from_name("old value"), EngineKind::Spotify);
    }
}
