//! Plays one song through a real engine and prints what happens, without the UI.
//! Usage: engine_smoke [youtube|librespot|spotify] [seconds]

use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use app_core::{api::Api, auth::Auth, config::AppConfig};
use app_player::{Backend, Command, EngineChoice, EngineKind, Event, PlayRequest, PlaybackEngine, Track, Ytdlp};

/// Not the app's port, so this can run beside it.
const PORT: u16 = 8899;

fn main() {
    let kind = EngineKind::from_name(&env::args().nth(1).unwrap_or_else(|| "youtube".into()));
    let seconds: u64 = env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(25);
    let config = AppConfig::load_or_create().expect("config");
    let auth = Arc::new(Auth::new(config.client_id.trim(), config.redirect_uri()));
    let api = Arc::new(Api::new(Arc::clone(&auth)));
    let choice = EngineChoice {
        kind,
        ytdlp: Ytdlp::from_config(&config),
    };
    println!("yt-dlp: {:?}, playback blocked for {}s", choice.ytdlp.version(), api.playback_blocked_seconds());
    let (backend, events) = Backend::new(auth, api, PORT, choice).expect("backend");
    backend.start_engine();
    if backend.has_saved_login() {
        backend.connect_spotify();
    }

    // SONG="Artist|Title|seconds" plays another song, never looked up before.
    let song = env::var("SONG").unwrap_or_else(|_| "Daft Punk|Get Lucky|248".into());
    let parts: Vec<&str> = song.split('|').collect();
    let track = Track {
        uri: if parts[1] == "Get Lucky" { "spotify:track:69kOkLUCkxIZYexIgSG8rq".into() } else { format!("smoke:{song}") },
        name: parts[1].into(),
        artists: vec![parts[0].into()],
        duration_ms: parts.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) * 1000,
        ..Default::default()
    };
    let started = Instant::now();
    let mut asked = false;
    let mut sought = false;
    while started.elapsed() < Duration::from_secs(seconds) {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::State(s)) => println!(
                "{:>5.1}s state paused={} position={}ms duration={}ms track={:?}",
                started.elapsed().as_secs_f32(),
                s.paused,
                s.position_ms,
                s.duration_ms,
                s.track.map(|t| t.name)
            ),
            Ok(other) => println!("{:>5.1}s {other:?}", started.elapsed().as_secs_f32()),
            Err(_) => {}
        }
        // SEEK_MS=<ms> jumps there 12 seconds in, to check that playing goes on.
        if let (true, Some(ms)) = (started.elapsed() > Duration::from_secs(12) && !sought, env::var("SEEK_MS").ok().and_then(|v| v.parse().ok())) {
            sought = true;
            println!("{:>5.1}s seeking to {ms}ms", started.elapsed().as_secs_f32());
            backend.send(Command::SeekMs(ms));
        }
        if !asked && backend.ready() {
            asked = true;
            println!("{:>5.1}s engine ready, playing", started.elapsed().as_secs_f32());
            backend.send(Command::Volume(0.15));
            backend.send(Command::Play(PlayRequest { tracks: vec![track.clone()], start: Some(0), ..Default::default() }));
        }
    }
    println!("queue: {:?}", backend.local_queue().map(|q| q.len()));
    std::process::exit(0);
}
