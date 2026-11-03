//! Songs kept for listening offline: whole local playlists or single songs, always from
//! YouTube. One worker downloads one song at a time with a pause between them, the pace of
//! someone listening, and stops when YouTube asks to confirm this is not a bot. A song is
//! stored once, by its video, so the same song in another playlist is not fetched again.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use app_core::{
    api::TrackSummary,
    cache::Store,
    config::AppConfig,
    local_library::{ImportedPlaylist, LocalLibrary},
    offline,
};
use app_player::{cancel_download, video_for, Ytdlp};
use slint::{Timer, Weak};

use super::{track_of, with, Library};
use crate::{i18n::t, models, MainWindow};

/// Between two downloads.
const PAUSE: Duration = Duration::from_secs(3);
/// After YouTube asked for a break, downloads start again this much later.
const RETRY_AFTER_BLOCK: Duration = Duration::from_secs(16 * 60);
/// The key under which single songs are kept, so a playlist never counts them as its own.
const SINGLE_SONGS: &str = "offline-songs";

enum Job {
    Playlist(String),
    Song(TrackSummary),
}

/// What the worker tells the UI thread.
enum Report {
    /// These songs wait their turn.
    Waiting(Vec<String>),
    /// A song is on its way: how much of it is done.
    Song { uri: String, progress: f32 },
    /// A song is downloaded, as this video.
    Done { uri: String, video: String },
    /// A song could not be downloaded, or no longer waits.
    Dropped(Vec<String>),
    /// A playlist's songs done, songs in all.
    Playlist { uri: String, done: usize, total: usize },
    Finished { uri: String, name: String, missing: usize, blocked: bool },
    SongFailed { error: String },
}

static WORKER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();

thread_local! {
    /// Playlists being downloaded: songs done and songs in all.
    static PROGRESS: RefCell<HashMap<String, (usize, usize)>> = RefCell::new(HashMap::new());
    /// The song downloading now and how much of it is done.
    static CURRENT: RefCell<Option<(String, f32)>> = const { RefCell::new(None) };
    /// Songs waiting their turn.
    static WAITING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// When YouTube last asked for a break, so it is told once.
    static BLOCK_TOLD: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn send(lib: &Library, job: Job) {
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let (ui, store) = (lib.ui.clone(), lib.api.store().clone());
        thread::Builder::new().name("downloads".into()).spawn(move || work(rx, ui, store)).expect("spawn downloads");
        Mutex::new(tx)
    });
    let _ = worker.lock().unwrap().send(job);
}

fn enabled() -> bool {
    AppConfig::load_or_create().is_ok_and(|c| c.offline)
}

fn tell(ui: &Weak<MainWindow>, report: Report) {
    let _ = ui.upgrade_in_event_loop(move |ui| apply(&ui, report));
}

// ---- the worker ----

fn work(jobs: mpsc::Receiver<Job>, ui: Weak<MainWindow>, store: Store) {
    let local = LocalLibrary::default();
    for job in jobs {
        if !enabled() {
            continue;
        }
        let tool = Ytdlp::from_config(&AppConfig::load_or_create().unwrap_or_default());
        match job {
            Job::Song(track) => {
                let result = fetch(&tool, &store, &track, &ui);
                match result {
                    Ok((video, _)) => tell(&ui, Report::Done { uri: track.uri, video }),
                    Err(error) => {
                        tell(&ui, Report::Dropped(vec![track.uri]));
                        if error != "cancelled" {
                            tell(&ui, Report::SongFailed { error });
                        }
                    }
                }
                thread::sleep(PAUSE);
            }
            Job::Playlist(uri) => download_playlist(&local, &tool, &store, &uri, &ui),
        }
    }
}

/// Downloads one song, finding its video first when it is not a YouTube one. Returns the
/// video, and whether anything was fetched (the same song may be here for another playlist).
fn fetch(tool: &Ytdlp, store: &Store, track: &TrackSummary, ui: &Weak<MainWindow>) -> Result<(String, bool), String> {
    let video = match offline::video_id(&track.uri) {
        Some(video) => video.to_string(),
        None => video_for(tool, store, &track_of(track.clone()))?,
    };
    // The same song, kept for another playlist, is already here.
    if offline::file_for(&video).is_some() {
        return Ok((video, false));
    }
    tell(ui, Report::Song { uri: track.uri.clone(), progress: 0.0 });
    let (uri, last) = (track.uri.clone(), Cell::new(-1i32));
    tool.download_audio(&video, |done| {
        // A step per percent is plenty for the screen.
        let percent = (done * 100.0) as i32;
        if percent != last.replace(percent) {
            tell(ui, Report::Song { uri: uri.clone(), progress: done });
        }
    })?;
    Ok((video, true))
}

/// The songs of a playlist that downloading covers: YouTube's, and in the user's own
/// playlists and copies any song, looked up on YouTube.
fn covered(playlist: &ImportedPlaylist) -> Vec<TrackSummary> {
    let any = matches!(playlist.source.as_str(), app_core::local_library::SOURCE_OWN | app_core::local_library::SOURCE_COPY);
    let mut seen = HashSet::new();
    playlist.tracks.iter().filter(|t| any || offline::video_id(&t.uri).is_some()).filter(|t| seen.insert(t.uri.clone())).cloned().collect()
}

fn is_downloaded(track: &TrackSummary) -> bool {
    offline::video_id(&track.uri).is_some_and(|video| offline::file_for(video).is_some())
}

fn download_playlist(local: &LocalLibrary, tool: &Ytdlp, store: &Store, uri: &str, ui: &Weak<MainWindow>) {
    let Some(playlist) = local.playlist(uri).filter(|p| p.offline) else { return };
    let tracks = covered(&playlist);
    let total = tracks.len();
    let pending: Vec<TrackSummary> = tracks.into_iter().filter(|t| !is_downloaded(t)).collect();
    let mut done = total - pending.len();
    tell(ui, Report::Waiting(pending.iter().map(|t| t.uri.clone()).collect()));
    tell(ui, Report::Playlist { uri: uri.to_string(), done, total });
    let (mut missing, mut blocked) = (0, false);
    let mut left = pending.iter().map(|t| t.uri.clone()).collect::<Vec<_>>();
    for track in &pending {
        // Stopped, turned off or no longer kept while this ran.
        if !enabled() || !local.playlist(uri).is_some_and(|p| p.offline) {
            break;
        }
        left.retain(|u| *u != track.uri);
        let mut fetched = true;
        match fetch(tool, store, track, ui) {
            Ok((video, fresh)) => {
                fetched = fresh;
                done += 1;
                tell(ui, Report::Done { uri: track.uri.clone(), video });
                tell(ui, Report::Playlist { uri: uri.to_string(), done, total });
            }
            Err(e) if e == "cancelled" => break,
            Err(e) if e.contains("not a bot") => {
                tell(ui, Report::Dropped(vec![track.uri.clone()]));
                blocked = true;
                break;
            }
            Err(e) => {
                eprintln!("not downloaded: {e}");
                tell(ui, Report::Dropped(vec![track.uri.clone()]));
                missing += 1;
            }
        }
        if fetched {
            thread::sleep(PAUSE);
        }
    }
    tell(ui, Report::Dropped(left));
    tell(ui, Report::Finished { uri: uri.to_string(), name: playlist.name, missing, blocked });
}

// ---- on the UI thread ----

fn apply(ui: &MainWindow, report: Report) {
    match report {
        Report::Waiting(uris) => {
            for uri in uris {
                models::set_download_state(&uri, 1, 0.0);
                WAITING.with(|w| w.borrow_mut().insert(uri));
            }
        }
        Report::Song { uri, progress } => {
            WAITING.with(|w| w.borrow_mut().remove(&uri));
            models::set_download_state(&uri, 2, progress);
            CURRENT.with(|c| *c.borrow_mut() = Some((uri, progress)));
            refresh_open(ui);
        }
        Report::Done { uri, video } => {
            WAITING.with(|w| w.borrow_mut().remove(&uri));
            CURRENT.with(|c| c.borrow_mut().take_if(|(u, _)| *u == uri));
            models::set_download_state(&uri, 0, 0.0);
            models::set_offline(&video, true);
            if offline::video_id(&uri).is_none() {
                became_youtube(&uri, &video);
            }
            ui.set_offline_usage(usage_text().into());
        }
        Report::Dropped(uris) => {
            for uri in uris {
                WAITING.with(|w| w.borrow_mut().remove(&uri));
                CURRENT.with(|c| c.borrow_mut().take_if(|(u, _)| *u == uri));
                models::set_download_state(&uri, 0, 0.0);
            }
        }
        Report::Playlist { uri, done, total } => {
            PROGRESS.with(|p| p.borrow_mut().insert(uri, (done, total)));
            refresh_open(ui);
        }
        Report::Finished { uri, name, missing, blocked } => finished(ui, &uri, &name, missing, blocked),
        Report::SongFailed { error } => failed(ui, &error),
    }
}

/// A Spotify song of the user's own playlist was downloaded from YouTube: from now on it is
/// that YouTube song there, so it plays from the file.
fn became_youtube(uri: &str, video: &str) {
    let youtube = format!("youtube:{video}");
    with(|lib| {
        let own = |p: &ImportedPlaylist| matches!(p.source.as_str(), app_core::local_library::SOURCE_OWN | app_core::local_library::SOURCE_COPY);
        for mut playlist in lib.likes.local.playlists().into_iter().filter(|p| own(p) && p.tracks.iter().any(|t| t.uri == uri)) {
            for track in playlist.tracks.iter_mut().filter(|t| t.uri == uri) {
                track.uri = youtube.clone();
            }
            lib.likes.local.update(&playlist);
        }
    });
    models::rename_uri(uri, &youtube, video);
}

fn failed(ui: &MainWindow, error: &str) {
    if error.contains("not a bot") {
        tell_block(ui);
    } else {
        eprintln!("not downloaded: {error}");
        crate::toast_error(ui, t("Download non riuscito: controlla la connessione e riprova.", "Download failed: check the connection and try again."));
    }
}

/// Told once per break, and downloads start again after it.
fn tell_block(ui: &MainWindow) {
    let recent = BLOCK_TOLD.with(|b| b.get()).is_some_and(|at| at.elapsed() < RETRY_AFTER_BLOCK);
    if recent {
        return;
    }
    BLOCK_TOLD.with(|b| b.set(Some(Instant::now())));
    crate::toast_error(ui, t("YouTube ha chiesto una pausa: i download ripartono da soli tra un quarto d'ora.", "YouTube asked for a break: downloads start again by themselves in a quarter of an hour."));
    Timer::single_shot(RETRY_AFTER_BLOCK, || with(resume_all));
}

fn finished(ui: &MainWindow, uri: &str, name: &str, missing: usize, blocked: bool) {
    PROGRESS.with(|p| p.borrow_mut().remove(uri));
    refresh_open(ui);
    ui.set_offline_usage(usage_text().into());
    if blocked {
        tell_block(ui);
        return;
    }
    // Stopped by the user: nothing to tell.
    let kept = LocalLibrary::default().playlist(uri).is_some_and(|p| p.offline);
    if !kept {
        return;
    }
    let text = match missing {
        0 => format!("«{name}» {}", t("è disponibile offline", "is available offline")),
        n => format!("«{name}» {}: {n} {}", t("è disponibile offline", "is available offline"), t("brani non si sono scaricati, riprova più tardi", "songs didn't download, try again later")),
    };
    crate::toast(ui, &text);
}

/// The download button of the open playlist.
fn refresh_open(ui: &MainWindow) {
    with(|lib| {
        if let Some(playlist) = lib.open_local() {
            show_state(ui, &playlist);
        }
    });
}

/// What the download button of a local playlist shows.
pub(super) fn show_state(ui: &MainWindow, playlist: &ImportedPlaylist) {
    ui.set_detail_downloadable(playlist.downloadable());
    match PROGRESS.with(|p| p.borrow().get(&playlist.uri()).copied()) {
        Some((done, total)) => {
            let current = CURRENT.with(|c| c.borrow().as_ref().filter(|(uri, _)| playlist.tracks.iter().any(|t| t.uri == *uri)).map(|(_, p)| *p)).unwrap_or(0.0);
            let progress = if total == 0 { 0.0 } else { ((done as f32 + current) / total as f32).min(1.0) };
            let text = if total == 0 {
                t("In attesa…", "Waiting…").to_string()
            } else {
                format!("{done} {} {total} · {}%", t("di", "of"), (progress * 100.0) as u32)
            };
            ui.set_detail_download_state(1);
            ui.set_detail_download_progress(progress);
            ui.set_detail_download_text(text.into());
        }
        None => ui.set_detail_download_state(if playlist.offline { 2 } else { 0 }),
    }
}

pub(super) fn queue_playlist(lib: &Library, uri: &str) {
    if enabled() {
        PROGRESS.with(|p| {
            p.borrow_mut().entry(uri.to_string()).or_insert((0, 0));
        });
        send(lib, Job::Playlist(uri.to_string()));
    }
}

/// The playlist's download button: starts, stops (keeping what is done) or removes.
pub fn toggle_open(lib: &Library) {
    let (Some(ui), Some(mut playlist)) = (lib.ui.upgrade(), lib.open_local()) else { return };
    if !playlist.downloadable() {
        return;
    }
    let running = PROGRESS.with(|p| p.borrow().contains_key(&playlist.uri()));
    if running {
        // Stop: what is downloaded stays; pressing again fetches only what is missing.
        playlist.offline = false;
        lib.likes.local.update(&playlist);
        let current = CURRENT.with(|c| c.borrow().as_ref().map(|(uri, _)| uri.clone()));
        if current.is_some_and(|uri| playlist.tracks.iter().any(|t| t.uri == uri)) {
            cancel_download();
        }
        PROGRESS.with(|p| p.borrow_mut().remove(&playlist.uri()));
        crate::toast(&ui, &format!("{} «{}»: {}", t("Download interrotto", "Download stopped"), playlist.name, t("i brani scaricati restano", "the downloaded songs stay")));
    } else if playlist.offline {
        playlist.offline = false;
        lib.likes.local.update(&playlist);
        release(lib, &playlist, &playlist.tracks);
        crate::toast(&ui, &format!("{} «{}»", t("Download rimossi da", "Downloads removed from"), playlist.name));
    } else {
        playlist.offline = true;
        lib.likes.local.update(&playlist);
        queue_playlist(lib, &playlist.uri());
    }
    show_state(&ui, &playlist);
}

/// "Download" in a song's menu.
pub(super) fn keep_songs(lib: &Library, tracks: &[TrackSummary]) {
    let Some(ui) = lib.ui.upgrade() else { return };
    let mut queued = 0;
    for track in tracks.iter().filter(|t| !is_downloaded(t)) {
        lib.likes.local.set_offline_song(track, true);
        models::set_download_state(&track.uri, 1, 0.0);
        WAITING.with(|w| w.borrow_mut().insert(track.uri.clone()));
        send(lib, Job::Song(track.clone()));
        queued += 1;
    }
    let text = match (queued, tracks.first()) {
        (0, _) => t("Già scaricato", "Already downloaded").to_string(),
        (1, Some(track)) => format!("{} «{}»", t("In download:", "Downloading:"), track.name),
        (n, _) => format!("{n} {}", t("brani in download", "songs downloading")),
    };
    crate::toast(&ui, &text);
}

/// "Remove download" in a song's menu; a file stays if a playlist kept offline has it.
pub(super) fn forget_songs(lib: &Library, tracks: &[TrackSummary]) {
    let Some(ui) = lib.ui.upgrade() else { return };
    let mut kept_by = None;
    for track in tracks {
        lib.likes.local.set_offline_song(track, false);
        match lib.likes.local.kept_offline_by(&track.uri, SINGLE_SONGS) {
            Some(name) => kept_by = Some(name),
            None => delete(&ui, track),
        }
    }
    match kept_by {
        Some(name) => crate::toast(&ui, &format!("{} «{name}», {}", t("Resta scaricato quello che fa parte di", "What's part of"), t("disponibile offline", "stays downloaded, as it is available offline"))),
        None => crate::toast(&ui, t("Download rimosso", "Download removed")),
    }
}

/// Deletes the files of `tracks` that nothing else keeps, now that `playlist` lets them go.
pub(super) fn release(lib: &Library, playlist: &ImportedPlaylist, tracks: &[TrackSummary]) {
    let Some(ui) = lib.ui.upgrade() else { return };
    for track in tracks {
        if lib.likes.local.kept_offline_by(&track.uri, &playlist.id).is_none() {
            delete(&ui, track);
        }
    }
}

fn delete(ui: &MainWindow, track: &TrackSummary) {
    if let Some(video) = offline::video_id(&track.uri) {
        offline::remove(&[video]);
        models::set_offline(video, offline::file_for(video).is_some());
    }
    ui.set_offline_usage(usage_text().into());
}

/// Picks up whatever is kept offline but not downloaded yet: at start, after turning
/// downloads on, after a break.
pub fn resume_all(lib: &Library) {
    if !enabled() {
        return;
    }
    for playlist in lib.likes.local.playlists().iter().filter(|p| p.offline && p.downloadable()) {
        if covered(playlist).iter().any(|t| !is_downloaded(t)) {
            queue_playlist(lib, &playlist.uri());
        }
    }
    let songs: Vec<TrackSummary> = lib.likes.local.offline_songs().into_iter().filter(|t| !is_downloaded(t)).collect();
    for track in songs {
        send(lib, Job::Song(track));
    }
}

/// The switch in Settings. Turning it off keeps what was downloaded.
pub fn set_enabled(lib: &Library, on: bool) {
    let Some(ui) = lib.ui.upgrade() else { return };
    if let Ok(mut config) = AppConfig::load_or_create() {
        config.offline = on;
        let _ = config.save();
    }
    ui.set_offline_enabled(on);
    if on {
        resume_all(lib);
    } else {
        cancel_download();
    }
}

/// "Delete downloads" in Settings: every file goes, and nothing is kept offline any more.
pub fn delete_all(lib: &Library) {
    let Some(ui) = lib.ui.upgrade() else { return };
    lib.likes.local.forget_offline();
    cancel_download();
    offline::remove_all();
    models::reload_offline();
    ui.set_offline_usage(usage_text().into());
    if let Some(playlist) = lib.open_local() {
        show_state(&ui, &playlist);
    }
    crate::toast(&ui, t("Download eliminati", "Downloads deleted"));
}

/// "Space used: 120 MB", or nothing while nothing is downloaded.
pub fn usage_text() -> String {
    match offline::size() {
        0 => String::new(),
        bytes => format!("{} {}", t("Spazio usato:", "Space used:"), size_text(bytes)),
    }
}

fn size_text(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{:.0} MB", mb.max(1.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_text(4_304_597), "4 MB");
        assert_eq!(size_text(100), "1 MB");
        assert_eq!(size_text(3 * 1_073_741_824), "3.0 GB");
    }

    #[test]
    fn own_playlists_cover_every_song_imports_only_youtube_ones() {
        let song = |uri: &str| TrackSummary { uri: uri.into(), ..Default::default() };
        let mut p = ImportedPlaylist {
            source: "own".into(),
            tracks: vec![song("youtube:aaaaaaaaaaa"), song("spotify:track:x"), song("youtube:aaaaaaaaaaa")],
            ..Default::default()
        };
        assert_eq!(covered(&p).len(), 2, "each song once");
        p.source = "youtube".into();
        assert_eq!(covered(&p).len(), 1);
    }
}
