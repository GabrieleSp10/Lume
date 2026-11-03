//! Playback backend: a local loopback server (OAuth callback + player page + token
//! endpoint), a hidden WebView2 that hosts the official Spotify Web Playback SDK, and local
//! engines (YouTube, librespot) for when that path is closed. The UI only talks to the
//! [`PlaybackEngine`] trait and the [`Event`] channel.

mod engine;
#[cfg(feature = "librespot")]
mod librespot;
mod local;
mod server;
mod types;
mod youtube;

use std::{
    io,
    process::Command as Process,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use app_core::{api::Api, auth::Auth};
use local::{Msg, Sink};
use serde_json::{json, Value};

/// How long a song waits for Spotify's player before the backup takes over.
const DEVICE_WAIT: Duration = Duration::from_secs(6);

pub use types::{Command, EngineKind, Event, PlayRequest, PlaybackState, Track};
pub use youtube::{video_for, Fetch, Quality, Ytdlp, YOUTUBE_URI};

/// Anything that can play audio and be controlled by the UI.
pub trait PlaybackEngine: Send + Sync {
    fn send(&self, cmd: Command);
    /// Whether a song can be started right now.
    fn ready(&self) -> bool;
    /// What will play next when this app keeps the queue itself; None when Spotify does.
    fn local_queue(&self) -> Option<Vec<Track>>;
}

/// Which engine plays music, and what happens when it cannot.
#[derive(Debug, Clone)]
pub struct EngineChoice {
    pub kind: EngineKind,
    pub ytdlp: Ytdlp,
}

pub struct Backend {
    shared: Arc<Shared>,
}

struct Shared {
    auth: Arc<Auth>,
    api: Arc<Api>,
    port: u16,
    key: String,
    events: mpsc::Sender<Event>,
    page: Arc<Mutex<Option<engine::WebViewEngine>>>,
    page_loaded: AtomicBool,
    /// Logged in to Spotify: its player should run in the page.
    spotify_wanted: AtomicBool,
    /// The song left Spotify for a backup: going back to Spotify could loop.
    left_spotify: AtomicBool,
    device: Mutex<Option<String>>,
    choice: Mutex<EngineChoice>,
    /// A local engine plays right now, not Spotify's player.
    local_active: AtomicBool,
    local: mpsc::Sender<Msg>,
    panel: Arc<Mutex<Vec<Track>>>,
    spotify: mpsc::Sender<Command>,
    /// The song asked of Spotify, in case its player refuses it.
    last_request: Mutex<Option<PlayRequest>>,
    /// Spotify's player took that song: later errors are about a control, not the song.
    spotify_started: AtomicBool,
}

impl Backend {
    /// Starts the local server and returns the backend plus the channel of engine events.
    pub fn new(auth: Arc<Auth>, api: Arc<Api>, port: u16, choice: EngineChoice) -> io::Result<(Self, mpsc::Receiver<Event>)> {
        let (events, rx) = mpsc::channel();
        // Per-run secret: the player page must present it to read tokens, which keeps other
        // web pages (or DNS-rebinding tricks) from asking the loopback server for a token.
        let key = format!("{:032x}", rand::random::<u128>());
        server::spawn(Arc::clone(&auth), port, key.clone(), events.clone())?;
        let (local, local_inbox) = mpsc::channel();
        let (spotify, spotify_inbox) = mpsc::channel();
        let panel: Arc<Mutex<Vec<Track>>> = Arc::default();
        let shared = Arc::new(Shared {
            auth,
            api,
            port,
            key,
            events: events.clone(),
            page: Arc::default(),
            page_loaded: AtomicBool::new(false),
            spotify_wanted: AtomicBool::new(false),
            left_spotify: AtomicBool::new(false),
            device: Mutex::default(),
            choice: Mutex::new(choice),
            local_active: AtomicBool::new(false),
            local,
            panel: Arc::clone(&panel),
            spotify,
            last_request: Mutex::default(),
            spotify_started: AtomicBool::new(false),
        });
        let (primary, fallback) = shared.sinks();
        let rescuer = Arc::clone(&shared);
        let player = local::Player::new(primary, fallback, events, panel).with_rescue(Box::new(move |tracks| rescuer.rescue(tracks)));
        thread::Builder::new().name("local-player".into()).spawn(move || player.run(local_inbox))?;
        let worker = Arc::clone(&shared);
        thread::Builder::new().name("spotify-commands".into()).spawn(move || {
            for cmd in spotify_inbox {
                worker.spotify_call(cmd);
            }
        })?;
        Ok((Self { shared }, rx))
    }

    pub fn has_saved_login(&self) -> bool {
        self.shared.auth.has_saved_login()
    }

    /// Opens the Spotify login page in the user's default browser. The result
    /// arrives as [`Event::Login`].
    pub fn open_login_in_browser(&self) -> io::Result<()> {
        let url = self.shared.auth.begin_login();
        // rundll32 avoids cmd.exe quoting problems with '&' in the URL.
        Process::new("rundll32").args(["url.dll,FileProtocolHandler", &url]).spawn().map(|_| ())
    }

    /// Runs Spotify's player too, once logged in.
    pub fn connect_spotify(&self) {
        self.shared.spotify_wanted.store(true, Ordering::SeqCst);
        if self.shared.page_loaded.load(Ordering::SeqCst) {
            self.shared.page_command(json!({ "cmd": "connect_spotify" }));
        }
    }

    /// Starts the hidden page that plays audio (no-op if already running).
    pub fn start_engine(&self) {
        let mut page = self.shared.page.lock().unwrap();
        if page.is_none() {
            let shared = Arc::clone(&self.shared);
            *page = Some(engine::WebViewEngine::start(
                self.shared.port,
                self.shared.key.clone(),
                self.shared.events.clone(),
                Arc::new(move |message| shared.page_message(message)),
            ));
        }
    }

    /// Opens the YouTube login window; the result arrives as [`Event::YouTubeAccount`].
    pub fn youtube_login(&self) {
        self.start_engine();
        if let Some(page) = self.shared.page.lock().unwrap().as_ref() {
            page.request(engine::Request::OpenYouTubeLogin);
        }
    }

    pub fn youtube_logout(&self) {
        self.start_engine();
        if let Some(page) = self.shared.page.lock().unwrap().as_ref() {
            page.request(engine::Request::YouTubeLogout);
        }
    }

    pub fn logout(&self) {
        let _ = self.shared.local.send(Msg::Stop);
        self.shared.local_active.store(false, Ordering::SeqCst);
        self.shared.spotify_wanted.store(false, Ordering::SeqCst);
        *self.shared.device.lock().unwrap() = None;
        self.shared.page_command(json!({ "cmd": "disconnect_spotify" }));
        self.shared.auth.logout();
        self.shared.api.forget_account();
    }

    /// A new choice in Settings; it applies from the next song started.
    pub fn set_engine(&self, choice: EngineChoice) {
        *self.shared.choice.lock().unwrap() = choice;
        let (primary, fallback) = self.shared.sinks();
        let _ = self.shared.local.send(Msg::Configure { primary, fallback });
    }
}

impl Shared {
    fn page_send(self: &Arc<Self>) -> youtube::PageSend {
        let page = Arc::clone(&self.page);
        Arc::new(move |command| {
            if let Some(engine) = page.lock().unwrap().as_ref() {
                engine.send(command);
            }
        })
    }

    fn youtube_sink(self: &Arc<Self>, ytdlp: Ytdlp) -> Box<dyn Sink> {
        let resolver = youtube::Resolver::new(ytdlp, self.api.store().clone());
        Box::new(youtube::YouTubeSink::new(self.page_send(), self.local.clone(), Arc::new(resolver)))
    }

    /// The local engine and its backup. When Spotify's player is the choice, YouTube is
    /// the local engine, since it only runs as the backup.
    fn sinks(self: &Arc<Self>) -> (Box<dyn Sink>, Option<Box<dyn Sink>>) {
        let choice = self.choice.lock().unwrap().clone();
        let youtube = self.youtube_sink(choice.ytdlp.clone());
        match choice.kind {
            #[cfg(feature = "librespot")]
            EngineKind::Librespot => {
                let sink: Box<dyn Sink> = Box::new(librespot::LibrespotSink::new(Arc::clone(&self.auth), self.local.clone()));
                (sink, Some(youtube))
            }
            _ => (youtube, None),
        }
    }

    fn page_message(self: &Arc<Self>, message: types::PageMessage) {
        use types::PageMessage as M;
        match message {
            M::Loaded => {
                self.page_loaded.store(true, Ordering::SeqCst);
                if self.spotify_wanted.load(Ordering::SeqCst) {
                    self.page_command(json!({ "cmd": "connect_spotify" }));
                }
                let _ = self.events.send(Event::Ready { device_id: None });
            }
            M::Audio { token, event } => {
                let _ = self.local.send(Msg::Audio { token, event });
            }
            M::Event(Event::Ready { device_id }) => {
                *self.device.lock().unwrap() = device_id.clone();
                let _ = self.events.send(Event::Ready { device_id });
            }
            M::Event(Event::State(state)) => {
                if state.track.is_some() {
                    self.spotify_started.store(true, Ordering::SeqCst);
                }
                let _ = self.events.send(Event::State(state));
            }
            M::Event(Event::Error(message))
                if message.starts_with("playback_error") && !self.spotify_started.load(Ordering::SeqCst) && self.fall_back(0) =>
            {
                eprintln!("spotify player refused the song: {message}");
            }
            M::Event(event) => {
                let _ = self.events.send(event);
            }
        }
    }

    fn page_command(&self, command: Value) {
        if let Some(engine) = self.page.lock().unwrap().as_ref() {
            engine.send(command.to_string());
        }
    }

    /// Retries the last song on a local engine. False if there was nothing to retry.
    fn fall_back(self: &Arc<Self>, blocked_seconds: u64) -> bool {
        let Some(request) = self.last_request.lock().unwrap().clone() else { return false };
        self.left_spotify.store(true, Ordering::SeqCst);
        self.go_local(request);
        let _ = self.events.send(Event::Switched { to: EngineKind::YouTube, blocked_seconds });
        true
    }

    /// A local engine broke: Spotify's player takes the queue, if it can play every song.
    fn rescue(self: &Arc<Self>, tracks: Vec<Track>) -> bool {
        let spotify_ready = self.device.lock().unwrap().is_some() && self.api.playback_blocked_seconds() == 0;
        let all_spotify = tracks.iter().all(|t| t.uri.starts_with("spotify:track:"));
        if !spotify_ready || !all_spotify || self.left_spotify.load(Ordering::SeqCst) {
            return false;
        }
        self.local_active.store(false, Ordering::SeqCst);
        let request = PlayRequest { tracks, start: Some(0), ..Default::default() };
        *self.last_request.lock().unwrap() = Some(request.clone());
        self.spotify_started.store(false, Ordering::SeqCst);
        // The song has moved once: if Spotify fails too, it must not bounce back here.
        self.left_spotify.store(true, Ordering::SeqCst);
        let _ = self.spotify.send(Command::Play(request));
        let _ = self.events.send(Event::Switched { to: EngineKind::Spotify, blocked_seconds: 0 });
        true
    }

    fn go_local(self: &Arc<Self>, request: PlayRequest) {
        if !self.local_active.swap(true, Ordering::SeqCst) && self.device.lock().unwrap().is_some() {
            self.to_page(&Command::Pause);
        }
        let _ = self.local.send(Msg::Command(Command::Play(request)));
    }

    fn play(self: &Arc<Self>, request: PlayRequest) {
        self.left_spotify.store(false, Ordering::SeqCst);
        let kind = self.choice.lock().unwrap().kind;
        // Songs that only exist on YouTube, or no Spotify login, leave just the local engines.
        let spotify_can = self.spotify_wanted.load(Ordering::SeqCst)
            && request.tracks.iter().all(|t| t.uri.starts_with("spotify:track:"));
        if kind != EngineKind::Spotify || !spotify_can {
            self.go_local(request);
            return;
        }
        *self.last_request.lock().unwrap() = Some(request.clone());
        self.spotify_started.store(false, Ordering::SeqCst);
        let blocked = self.api.playback_blocked_seconds();
        if blocked > 0 && self.fall_back(blocked) {
            return;
        }
        if self.local_active.swap(false, Ordering::SeqCst) {
            let _ = self.local.send(Msg::Stop);
        }
        let _ = self.spotify.send(Command::Play(request));
    }

    /// Spotify's player connects a few seconds after the page loads.
    fn wait_for_device(&self) -> Option<String> {
        let asked = Instant::now();
        loop {
            let device = self.device.lock().unwrap().clone();
            if device.is_some() || asked.elapsed() > DEVICE_WAIT {
                return device;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Requests of Spotify's player that go through the Web API, one at a time.
    fn spotify_call(self: &Arc<Self>, cmd: Command) {
        let Some(device) = self.wait_for_device() else {
            if !(matches!(cmd, Command::Play(_)) && self.fall_back(0)) {
                let _ = self.events.send(Event::Error("engine not ready".into()));
            }
            return;
        };
        let result = match &cmd {
            Command::Play(request) => {
                let Some(body) = play_body(request) else { return };
                // Lets the page start sound, which browsers tie to a user action.
                self.page_command(json!({ "cmd": "activate" }));
                self.api.play(&device, &body)
            }
            Command::Shuffle(on) => self.api.set_shuffle(&device, *on),
            Command::Repeat(mode) => self.api.set_repeat(&device, types::repeat_name(*mode)),
            Command::Queue(track) => self.api.add_to_queue(&track.uri, Some(&device)),
            _ => return,
        };
        let Err(e) = result else { return };
        eprintln!("spotify player command failed: {e}");
        let blocked = self.api.playback_blocked_seconds();
        if matches!(cmd, Command::Play(_)) && e.status() != Some(403) && self.fall_back(blocked) {
            return;
        }
        let _ = self.events.send(Event::Error(format!("API -> {} {e}", e.status().unwrap_or(0))));
    }

    fn to_page(&self, cmd: &Command) {
        if let (Some(engine), Some(json)) = (self.page.lock().unwrap().as_ref(), cmd.to_page_json()) {
            engine.send(json);
        }
    }
}

/// What Spotify's play endpoint needs for `request`; None if there is nothing to play.
fn play_body(request: &PlayRequest) -> Option<Value> {
    let mut body = match &request.context {
        Some(context) => {
            let start = request.start.and_then(|start| request.tracks.get(start));
            match start {
                Some(track) => json!({ "context_uri": context, "offset": { "uri": track.uri } }),
                None => json!({ "context_uri": context }),
            }
        }
        None => {
            let uris: Vec<&str> = request.tracks.iter().skip(request.start.unwrap_or(0)).map(|t| t.uri.as_str()).collect();
            if uris.is_empty() {
                return None;
            }
            json!({ "uris": uris })
        }
    };
    if request.position_ms > 0 {
        body["position_ms"] = json!(request.position_ms);
    }
    Some(body)
}

impl PlaybackEngine for Backend {
    fn send(&self, cmd: Command) {
        let shared = &self.shared;
        // Whichever engine plays next starts at the same volume.
        if let Command::Volume(value) = cmd {
            let _ = shared.local.send(Msg::Command(Command::Volume(value)));
            if shared.device.lock().unwrap().is_some() {
                shared.to_page(&cmd);
            }
            return;
        }
        if let Command::Play(request) = cmd {
            shared.play(request);
            return;
        }
        if shared.local_active.load(Ordering::SeqCst) {
            let _ = shared.local.send(Msg::Command(cmd));
        } else if cmd.to_page_json().is_some() {
            if shared.page.lock().unwrap().is_none() {
                let _ = shared.events.send(Event::Error("playback engine is not running".into()));
            }
            shared.to_page(&cmd);
        } else {
            let _ = shared.spotify.send(cmd);
        }
    }

    fn ready(&self) -> bool {
        let shared = &self.shared;
        shared.device.lock().unwrap().is_some() || shared.page_loaded.load(Ordering::SeqCst)
    }

    fn local_queue(&self) -> Option<Vec<Track>> {
        self.shared.local_active.load(Ordering::SeqCst).then(|| self.shared.panel.lock().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks() -> Vec<Track> {
        ["spotify:track:a", "spotify:track:b"].map(|uri| Track { uri: uri.into(), ..Default::default() }).to_vec()
    }

    #[test]
    fn a_context_starts_at_the_chosen_song() {
        let request = PlayRequest { tracks: tracks(), start: Some(1), context: Some("spotify:playlist:p".into()), ..Default::default() };
        assert_eq!(play_body(&request).unwrap(), json!({ "context_uri": "spotify:playlist:p", "offset": { "uri": "spotify:track:b" } }));
    }

    #[test]
    fn a_context_without_a_start_lets_spotify_choose() {
        let request = PlayRequest { tracks: tracks(), start: None, context: Some("spotify:album:p".into()), ..Default::default() };
        assert_eq!(play_body(&request).unwrap(), json!({ "context_uri": "spotify:album:p" }));
    }

    #[test]
    fn a_plain_list_plays_from_the_start_on() {
        let mut request = PlayRequest { tracks: tracks(), start: Some(1), ..Default::default() };
        assert_eq!(play_body(&request).unwrap(), json!({ "uris": ["spotify:track:b"] }));
        assert!(play_body(&PlayRequest::default()).is_none());
        request.position_ms = 60_000;
        assert_eq!(play_body(&request).unwrap(), json!({ "uris": ["spotify:track:b"], "position_ms": 60_000 }));
    }
}
