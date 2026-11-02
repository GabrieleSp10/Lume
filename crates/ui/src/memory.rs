//! Keeps the session file up to date and restores it at startup. UI thread only.

use std::{cell::RefCell, time::Duration};

use app_core::session::{SavedTrack, Session};
use app_player::{PlaybackState, Track};
use slint::{ComponentHandle, LogicalSize, Timer, TimerMode};

use crate::{images, models, MainWindow};

/// Smallest size accepted from the file, the window's own minimum.
const MIN_WIDTH: f32 = 980.0;
const MIN_HEIGHT: f32 = 640.0;
const AUTOSAVE: Duration = Duration::from_secs(15);

thread_local! {
    static SESSION: RefCell<Session> = RefCell::new(Session::load());
}

fn with<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    SESSION.with(|s| f(&mut s.borrow_mut()))
}

/// Reopens at the saved size. Call once the native window exists: Slint sizes a new window
/// from its layout, ignoring any size given earlier.
pub fn apply_window_size(ui: &MainWindow) {
    let (width, height, maximized) = with(|s| (s.window_width, s.window_height, s.maximized));
    if width > 0.0 {
        ui.window().set_size(LogicalSize::new(width.max(MIN_WIDTH), height.max(MIN_HEIGHT)));
    }
    if maximized {
        ui.window().set_maximized(true);
        ui.set_is_maximized(true);
    }
}

pub fn saved_track(track: &Track, duration_ms: u32) -> SavedTrack {
    SavedTrack {
        uri: track.uri.clone(),
        name: track.name.clone(),
        artists: track.artists.clone(),
        album: track.album.clone(),
        image_url: track.image_url.clone(),
        duration_ms,
    }
}

pub fn track_of(saved: SavedTrack) -> Track {
    Track { uri: saved.uri, name: saved.name, artists: saved.artists, album: saved.album, image_url: saved.image_url, duration_ms: saved.duration_ms }
}

/// Shows volume and the last song, paused where it stopped.
pub fn restore(ui: &MainWindow) {
    let (volume, muted, track, position) = with(|s| (s.volume, s.muted, s.track.clone(), s.position_ms));
    ui.set_volume(volume);
    ui.set_muted(muted);
    let Some(track) = track else { return };
    ui.set_has_track(true);
    ui.set_track_title(track.name.as_str().into());
    ui.set_track_artist(track.artists.join(", ").into());
    ui.set_now_uri(track.uri.as_str().into());
    ui.set_duration_ms(track.duration_ms as f32);
    ui.set_duration_text(models::format_time(track.duration_ms).into());
    ui.set_position_ms(position as f32);
    ui.set_position_text(models::format_time(position).into());
    models::set_now(&track.uri, &track.image_url);
    images::request(&track.image_url, &ui.as_weak());
    models::refresh_images(ui);
}

/// The song to resume, if it was restored from the last run.
pub fn resume_point() -> Option<(SavedTrack, u32, Option<String>, Vec<String>)> {
    with(|s| s.track.clone().map(|t| (t, s.position_ms, s.context_uri.clone(), s.play_uris.clone())))
}

pub fn queue() -> Vec<SavedTrack> {
    with(|s| s.queue.clone())
}

pub fn queue_add(track: SavedTrack) {
    with(|s| s.queue.push(track));
}

/// Remembers where playback came from, so it can start from the same place.
pub fn set_source(context_uri: Option<String>, play_uris: Vec<String>) {
    with(|s| {
        s.context_uri = context_uri;
        s.play_uris = play_uris;
    });
}

/// Records the playing song; a queued song that started leaves the saved queue.
pub fn on_state(state: &PlaybackState) {
    with(|s| match &state.track {
        Some(track) => {
            if let Some(at) = s.queue.iter().position(|q| q.uri == track.uri) {
                s.queue.remove(at);
            }
            s.track = Some(saved_track(track, state.duration_ms));
            s.position_ms = state.position_ms;
            if let Some(context) = &state.context_uri {
                s.context_uri = Some(context.clone());
            }
        }
        None => s.track = None,
    });
}

/// Writes everything to disk, reading window size and volume from the window.
pub fn save(ui: &MainWindow) {
    let window = ui.window();
    let size = window.size().to_logical(window.scale_factor());
    let maximized = window.is_maximized();
    let snapshot = with(|s| {
        s.maximized = maximized;
        if !maximized && !window.is_fullscreen() && size.width > 0.0 {
            s.window_width = size.width;
            s.window_height = size.height;
        }
        s.volume = ui.get_volume();
        s.muted = ui.get_muted();
        s.position_ms = ui.get_position_ms() as u32;
        s.clone()
    });
    if let Err(e) = snapshot.save() {
        eprintln!("session not saved: {e}");
    }
}

/// Saves regularly, so a crash or power cut loses little.
pub fn start_autosave(ui: &MainWindow) {
    let weak = ui.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, AUTOSAVE, move || {
        if let Some(ui) = weak.upgrade() {
            save(&ui);
        }
    });
    std::mem::forget(timer);
}
