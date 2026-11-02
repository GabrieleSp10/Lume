//! Experimental engine: plays Spotify's own audio without the Web API. Some accounts get
//! `error audio key` on every track; with the backup on, the first failure hands over to it.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::Sender,
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use app_core::{auth::Auth, config::cache_dir};
use librespot_core::{authentication::Credentials, cache::Cache, config::SessionConfig, session::Session, spotify_uri::SpotifyUri};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig},
    mixer::{softmixer::SoftMixer, Mixer, MixerConfig},
    player::{Player, PlayerEvent},
};
use tokio::runtime::Runtime;

use crate::{
    local::{Msg, Sink},
    types::{AudioEvent, Track},
};

/// A refused song can fail without any event, so silence this long counts as failure.
const START_TIMEOUT: Duration = Duration::from_secs(12);

struct Connection {
    player: Arc<Player>,
    mixer: SoftMixer,
    /// Runs the session and the event task, as long as the connection lives.
    _runtime: Runtime,
}

pub(crate) struct LibrespotSink {
    auth: Arc<Auth>,
    inbox: Sender<Msg>,
    connection: Arc<Mutex<Option<Arc<Connection>>>>,
    latest: Arc<AtomicU64>,
    /// The newest token that really started playing.
    started: Arc<AtomicU64>,
    volume: Arc<Mutex<f32>>,
}

impl LibrespotSink {
    pub(crate) fn new(auth: Arc<Auth>, inbox: Sender<Msg>) -> Self {
        Self { auth, inbox, connection: Arc::default(), latest: Arc::default(), started: Arc::default(), volume: Arc::new(Mutex::new(0.7)) }
    }

    fn player(&self) -> Option<Arc<Player>> {
        self.connection.lock().unwrap().as_ref().map(|c| Arc::clone(&c.player))
    }
}

fn connect(auth: &Auth, inbox: Sender<Msg>, latest: Arc<AtomicU64>, started: Arc<AtomicU64>) -> Result<Connection, String> {
    let runtime = Runtime::new().map_err(|e| e.to_string())?;
    let token = auth.access_token().map_err(|e| e.to_string())?;
    let audio_dir = cache_dir().join("librespot");
    let cache = Cache::new(Some(&audio_dir), Some(&audio_dir), None, None).map_err(|e| e.to_string())?;
    let session = runtime.block_on(async {
        let session = Session::new(SessionConfig::default(), Some(cache));
        session.connect(Credentials::with_access_token(token), true).await.map(|()| session)
    });
    let session = session.map_err(|e| format!("librespot login: {e}"))?;
    let mixer = SoftMixer::open(MixerConfig::default()).map_err(|e| e.to_string())?;
    let backend = audio_backend::find(None).ok_or("librespot: no audio backend")?;
    let player = {
        let _inside_runtime = runtime.enter();
        Player::new(PlayerConfig::default(), session.clone(), mixer.get_soft_volume(), move || backend(None, AudioFormat::default()))
    };
    let mut events = player.get_player_event_channel();
    runtime.spawn(async move {
        while let Some(event) = events.recv().await {
            let token = latest.load(Ordering::SeqCst);
            let audio = match event {
                PlayerEvent::Playing { position_ms, .. } => {
                    started.store(token, Ordering::SeqCst);
                    AudioEvent::Playing { position_ms, duration_ms: 0 }
                }
                PlayerEvent::Paused { position_ms, .. } => AudioEvent::Paused { position_ms },
                PlayerEvent::EndOfTrack { .. } => AudioEvent::Ended,
                PlayerEvent::Unavailable { .. } => AudioEvent::Failed("librespot: audio key refused".into()),
                _ => continue,
            };
            let _ = inbox.send(Msg::Audio { token, event: audio });
        }
    });
    Ok(Connection { player, mixer, _runtime: runtime })
}

impl Sink for LibrespotSink {
    fn load(&mut self, track: &Track, position_ms: u32, token: u64) {
        self.latest.store(token, Ordering::SeqCst);
        let uri = track.uri.clone();
        let (auth, inbox, connection, latest, started, volume) = (
            Arc::clone(&self.auth),
            self.inbox.clone(),
            Arc::clone(&self.connection),
            Arc::clone(&self.latest),
            Arc::clone(&self.started),
            Arc::clone(&self.volume),
        );
        thread::spawn(move || {
            let fail = |reason: String| {
                let _ = inbox.send(Msg::Audio { token, event: AudioEvent::Failed(reason) });
            };
            let uri = match SpotifyUri::from_uri(&uri) {
                Ok(uri) => uri,
                Err(e) => return fail(e.to_string()),
            };
            let live = {
                let mut slot = connection.lock().unwrap();
                if slot.is_none() {
                    match connect(&auth, inbox.clone(), Arc::clone(&latest), Arc::clone(&started)) {
                        Ok(c) => {
                            c.mixer.set_volume((*volume.lock().unwrap() * u16::MAX as f32) as u16);
                            *slot = Some(Arc::new(c));
                        }
                        Err(e) => return fail(e),
                    }
                }
                slot.as_ref().map(Arc::clone)
            };
            let Some(live) = live else { return };
            if latest.load(Ordering::SeqCst) != token {
                return;
            }
            live.player.load(uri, true, position_ms);
            thread::sleep(START_TIMEOUT);
            if latest.load(Ordering::SeqCst) == token && started.load(Ordering::SeqCst) != token {
                fail("librespot: audio key refused".into());
            }
        });
    }

    fn pause(&mut self) {
        if let Some(player) = self.player() {
            player.pause();
        }
    }

    fn resume(&mut self) {
        if let Some(player) = self.player() {
            player.play();
        }
    }

    fn seek(&mut self, position_ms: u32) {
        if let Some(player) = self.player() {
            player.seek(position_ms);
        }
    }

    fn volume(&mut self, value: f32) {
        *self.volume.lock().unwrap() = value;
        if let Some(connection) = self.connection.lock().unwrap().as_ref() {
            connection.mixer.set_volume((value.clamp(0.0, 1.0) * u16::MAX as f32) as u16);
        }
    }

    fn stop(&mut self) {
        self.latest.store(0, Ordering::SeqCst);
        if let Some(player) = self.player() {
            player.stop();
        }
    }
}
