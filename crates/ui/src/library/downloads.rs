//! Songs kept for listening offline: whole local playlists or single songs, always from
//! YouTube. A planner finds each song's video; workers download them a few per run of
//! yt-dlp, which waits a little between songs. One worker runs, or three with a YouTube
//! account and fast downloads on; after a bot check, one again until Lume restarts. A song
//! is stored once, by its video, so the same song in another playlist is not fetched again.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use app_core::{
    api::TrackSummary,
    cache::Store,
    config::{youtube_cookies_file, AppConfig},
    local_library::{ImportedPlaylist, LocalLibrary, SOURCE_COPY, SOURCE_OWN},
    offline,
};
use app_player::{video_for, Fetch, Quality, Ytdlp};
use slint::{Timer, Weak};

use super::{track_of, with, Library};
use crate::{i18n::t, models, MainWindow};

/// Workers with fast downloads on.
const FAST_WORKERS: usize = 3;
/// Songs per run of yt-dlp: enough to save its start-up, few enough to stop quickly.
const BATCH: usize = 6;
/// After YouTube asked for a break, downloads start again this much later.
const RETRY_AFTER_BLOCK: Duration = Duration::from_secs(16 * 60);
/// The key under which single songs are kept, so a playlist never counts them as its own.
const SINGLE_SONGS: &str = "offline-songs";

enum Job {
    Playlist(String),
    Song(TrackSummary),
}

/// One song to download, and the playlist it is fetched for (None: on its own).
#[derive(Clone)]
struct Item {
    track: TrackSummary,
    video: String,
    playlist: Option<String>,
}

/// What the workers share: songs waiting, and the runs going on (to stop them).
struct Queue {
    items: Mutex<VecDeque<Item>>,
    ready: Condvar,
    running: Mutex<Vec<(Arc<AtomicBool>, Vec<Item>)>>,
}

/// What the planner and the workers tell the UI thread.
enum Report {
    /// A playlist's songs already here, and songs in all.
    Total { playlist: String, done: usize, total: usize },
    /// These songs wait their turn.
    Waiting(Vec<String>),
    /// A song is on its way: how much of it is done.
    Song { uri: String, progress: f32 },
    Done { uri: String, video: String, playlist: Option<String> },
    /// A song no longer waits: it failed (`failed`) or its download was stopped.
    Dropped { uri: String, playlist: Option<String>, failed: bool },
    /// YouTube asked to confirm this is not a bot: everything stopped.
    Blocked,
}

static PLANNER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
static QUEUE: OnceLock<Queue> = OnceLock::new();
/// A bot check happened: one song at a time until Lume restarts.
static CAUTIOUS: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Playlists being downloaded: songs done, songs that failed, songs in all.
    static PROGRESS: RefCell<HashMap<String, (usize, usize, usize)>> = RefCell::new(HashMap::new());
    /// Songs on their way, and how much of each is done.
    static CURRENT: RefCell<HashMap<String, f32>> = RefCell::new(HashMap::new());
    /// Songs shown as waiting or downloading, to clear them when downloads stop.
    static ACTIVE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// When YouTube last asked for a break, so it is told once.
    static BLOCK_TOLD: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn queue() -> &'static Queue {
    QUEUE.get_or_init(|| Queue { items: Mutex::default(), ready: Condvar::new(), running: Mutex::default() })
}

/// Starts the planner and the workers the first time something is to be downloaded.
fn send(lib: &Library, job: Job) {
    let planner = PLANNER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let (ui, store) = (lib.ui.clone(), lib.api.store().clone());
        thread::Builder::new().name("downloads-plan".into()).spawn(move || plan(rx, ui, store)).expect("spawn planner");
        for n in 0..FAST_WORKERS {
            let ui = lib.ui.clone();
            thread::Builder::new().name(format!("downloads-{n}")).spawn(move || work(n, ui)).expect("spawn worker");
        }
        Mutex::new(tx)
    });
    let _ = planner.lock().unwrap().send(job);
}

fn config() -> AppConfig {
    AppConfig::load_or_create().unwrap_or_default()
}

fn enabled() -> bool {
    config().offline
}

/// How many workers may run: three only with a YouTube account and fast downloads on.
fn slots() -> usize {
    let fast = config().fast_downloads && youtube_cookies_file().exists() && !CAUTIOUS.load(Ordering::SeqCst);
    if fast { FAST_WORKERS } else { 1 }
}

fn tell(ui: &Weak<MainWindow>, report: Report) {
    let _ = ui.upgrade_in_event_loop(move |ui| apply(&ui, report));
}

/// The song is still wanted: its playlist is still kept offline, or the single song is.
fn wanted(local: &LocalLibrary, item_playlist: &Option<String>, track_uri: &str) -> bool {
    enabled()
        && match item_playlist {
            Some(uri) => local.playlist(uri).is_some_and(|p| p.offline),
            None => local.offline_songs().iter().any(|t| t.uri == track_uri),
        }
}

// ---- the planner: finds each song's video, then queues it ----

fn plan(jobs: mpsc::Receiver<Job>, ui: Weak<MainWindow>, store: Store) {
    let local = LocalLibrary::default();
    for job in jobs {
        if !enabled() {
            continue;
        }
        let tool = Ytdlp::from_config(&config());
        let (playlist, tracks) = match job {
            Job::Playlist(uri) => {
                let Some(p) = local.playlist(&uri).filter(|p| p.offline) else { continue };
                let tracks = covered(&p);
                let pending: Vec<TrackSummary> = tracks.iter().filter(|t| !is_downloaded(t)).cloned().collect();
                tell(&ui, Report::Total { playlist: uri.clone(), done: tracks.len() - pending.len(), total: tracks.len() });
                (Some(uri), pending)
            }
            Job::Song(track) => (None, vec![track]),
        };
        tell(&ui, Report::Waiting(tracks.iter().map(|t| t.uri.clone()).collect()));
        let mut rest = tracks.into_iter();
        while let Some(track) = rest.next() {
            if !wanted(&local, &playlist, &track.uri) {
                tell(&ui, Report::Dropped { uri: track.uri, playlist: playlist.clone(), failed: false });
                continue;
            }
            let found = match offline::video_id(&track.uri) {
                Some(video) => Ok(video.to_string()),
                None => video_for(&tool, &store, &track_of(track.clone())),
            };
            match found {
                // The same song, kept for another playlist, is already here.
                Ok(video) if offline::file_for(&video).is_some() => tell(&ui, Report::Done { uri: track.uri, video, playlist: playlist.clone() }),
                Ok(video) => {
                    let queue = queue();
                    queue.items.lock().unwrap().push_back(Item { track, video, playlist: playlist.clone() });
                    queue.ready.notify_all();
                }
                Err(e) if e.contains("not a bot") => {
                    stop_all_runs();
                    tell(&ui, Report::Blocked);
                    rest.by_ref().for_each(drop);
                }
                Err(e) => {
                    eprintln!("not downloaded: {e}");
                    tell(&ui, Report::Dropped { uri: track.uri, playlist: playlist.clone(), failed: true });
                }
            }
        }
    }
}

// ---- the workers: download a few songs per run ----

/// Waits until this worker may run and songs wait, then takes a few.
fn take(id: usize) -> Vec<Item> {
    let queue = queue();
    let mut items = queue.items.lock().unwrap();
    loop {
        let slots = slots();
        if id < slots && !items.is_empty() {
            let n = share(items.len(), slots);
            return items.drain(..n).collect();
        }
        // The settings may change meanwhile: look again now and then.
        items = queue.ready.wait_timeout(items, Duration::from_secs(1)).unwrap().0;
    }
}

/// Songs one worker takes from `waiting`: up to a run's worth, and a few songs are shared
/// out so each worker gets some.
fn share(waiting: usize, slots: usize) -> usize {
    waiting.div_ceil(slots.max(1)).clamp(1, BATCH)
}

fn work(id: usize, ui: Weak<MainWindow>) {
    let local = LocalLibrary::default();
    loop {
        let batch = take(id);
        let config = config();
        let tool = Ytdlp::from_config(&config);
        let cancel = Arc::new(AtomicBool::new(false));
        queue().running.lock().unwrap().push((Arc::clone(&cancel), batch.clone()));
        let videos: Vec<String> = batch.iter().map(|item| item.video.clone()).collect();
        let done = Mutex::new(HashSet::new());
        let result = tool.download(&videos, Quality::from_name(&config.download_quality), &cancel, |event| match event {
            Fetch::Progress(video, progress) => {
                for item in batch.iter().filter(|item| item.video == video) {
                    tell(&ui, Report::Song { uri: item.track.uri.clone(), progress });
                }
            }
            Fetch::Done(video) => {
                done.lock().unwrap().insert(video.clone());
                for item in batch.iter().filter(|item| item.video == video) {
                    tell(&ui, Report::Done { uri: item.track.uri.clone(), video: video.clone(), playlist: item.playlist.clone() });
                }
            }
        });
        queue().running.lock().unwrap().retain(|(c, _)| !Arc::ptr_eq(c, &cancel));
        let blocked = result.as_ref().is_err_and(|e| e.contains("not a bot"));
        let stopped = cancel.load(Ordering::SeqCst);
        let done = done.into_inner().unwrap();
        for item in batch.into_iter().filter(|item| !done.contains(&item.video) && offline::file_for(&item.video).is_none()) {
            // A run stopped for another playlist: its other songs wait again.
            if stopped && !blocked && wanted(&local, &item.playlist, &item.track.uri) {
                queue().items.lock().unwrap().push_front(item);
                continue;
            }
            tell(&ui, Report::Dropped { uri: item.track.uri, playlist: item.playlist, failed: !stopped && !blocked });
        }
        if blocked {
            CAUTIOUS.store(true, Ordering::SeqCst);
            stop_all_runs();
            tell(&ui, Report::Blocked);
        }
    }
}

/// Stops every run and empties the queue; the songs no longer wait.
fn stop_all_runs() {
    let queue = queue();
    queue.items.lock().unwrap().clear();
    for (cancel, _) in queue.running.lock().unwrap().iter() {
        cancel.store(true, Ordering::SeqCst);
    }
}

/// Stops what is downloaded for one playlist; runs that also had other songs give them back.
fn stop_playlist(uri: &str) {
    let queue = queue();
    queue.items.lock().unwrap().retain(|item| item.playlist.as_deref() != Some(uri));
    for (cancel, items) in queue.running.lock().unwrap().iter() {
        if items.iter().any(|item| item.playlist.as_deref() == Some(uri)) {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// The songs of a playlist that downloading covers: YouTube's, and in the user's own
/// playlists and copies any song, looked up on YouTube.
fn covered(playlist: &ImportedPlaylist) -> Vec<TrackSummary> {
    let any = matches!(playlist.source.as_str(), SOURCE_OWN | SOURCE_COPY);
    let mut seen = HashSet::new();
    playlist.tracks.iter().filter(|t| any || offline::video_id(&t.uri).is_some()).filter(|t| seen.insert(t.uri.clone())).cloned().collect()
}

fn is_downloaded(track: &TrackSummary) -> bool {
    offline::video_id(&track.uri).is_some_and(|video| offline::file_for(video).is_some())
}

// ---- on the UI thread ----

fn apply(ui: &MainWindow, report: Report) {
    match report {
        Report::Total { playlist, done, total } => {
            PROGRESS.with(|p| p.borrow_mut().insert(playlist.clone(), (done, 0, total)));
            refresh_open(ui);
            check_finished(ui, &playlist);
        }
        Report::Waiting(uris) => {
            for uri in uris {
                models::set_download_state(&uri, 1, 0.0);
                ACTIVE.with(|a| a.borrow_mut().insert(uri));
            }
        }
        Report::Song { uri, progress } => {
            models::set_download_state(&uri, 2, progress);
            CURRENT.with(|c| c.borrow_mut().insert(uri, progress));
            refresh_open(ui);
        }
        Report::Done { uri, video, playlist } => {
            settle(&uri);
            models::set_offline(&video, true);
            if offline::video_id(&uri).is_none() {
                became_youtube(&uri, &video);
            }
            ui.set_offline_usage(usage_text().into());
            if let Some(playlist) = playlist {
                count(ui, &playlist, |p| p.0 += 1);
            }
        }
        Report::Dropped { uri, playlist, failed } => {
            settle(&uri);
            match playlist {
                Some(playlist) if failed => count(ui, &playlist, |p| p.1 += 1),
                Some(_) => {}
                None if failed => crate::toast_error(ui, t("Download non riuscito: controlla la connessione e riprova.", "Download failed: check the connection and try again.")),
                None => {}
            }
        }
        Report::Blocked => {
            clear_all();
            tell_block(ui);
        }
    }
}

/// A song no longer waits nor downloads.
fn settle(uri: &str) {
    ACTIVE.with(|a| a.borrow_mut().remove(uri));
    CURRENT.with(|c| c.borrow_mut().remove(uri));
    models::set_download_state(uri, 0, 0.0);
}

/// Every song shown as waiting or downloading goes back to rest.
fn clear_all() {
    let uris: Vec<String> = ACTIVE.with(|a| a.borrow_mut().drain().collect());
    for uri in uris {
        settle(&uri);
    }
    PROGRESS.with(|p| p.borrow_mut().clear());
    with(|lib| {
        if let (Some(ui), Some(playlist)) = (lib.ui.upgrade(), lib.open_local()) {
            show_state(&ui, &playlist);
        }
    });
}

fn count(ui: &MainWindow, playlist: &str, change: impl FnOnce(&mut (usize, usize, usize))) {
    PROGRESS.with(|p| {
        if let Some(progress) = p.borrow_mut().get_mut(playlist) {
            change(progress);
        }
    });
    refresh_open(ui);
    check_finished(ui, playlist);
}

/// Once every song of a playlist is done or failed, it says so.
fn check_finished(ui: &MainWindow, playlist: &str) {
    let finished = PROGRESS.with(|p| p.borrow().get(playlist).copied()).filter(|(done, missing, total)| done + missing >= *total);
    let Some((_, missing, _)) = finished else { return };
    PROGRESS.with(|p| p.borrow_mut().remove(playlist));
    refresh_open(ui);
    ui.set_offline_usage(usage_text().into());
    let Some(name) = LocalLibrary::default().playlist(playlist).filter(|p| p.offline).map(|p| p.name) else { return };
    let text = match missing {
        0 => format!("«{name}» {}", t("è disponibile offline", "is available offline")),
        n => format!("«{name}» {}: {n} {}", t("è disponibile offline", "is available offline"), t("brani non si sono scaricati, riprova più tardi", "songs didn't download, try again later")),
    };
    crate::toast(ui, &text);
}

/// A Spotify song of the user's own playlist was downloaded from YouTube: from now on it is
/// that YouTube song there, so it plays from the file.
fn became_youtube(uri: &str, video: &str) {
    let youtube = format!("youtube:{video}");
    with(|lib| {
        let own = |p: &ImportedPlaylist| matches!(p.source.as_str(), SOURCE_OWN | SOURCE_COPY);
        for mut playlist in lib.likes.local.playlists().into_iter().filter(|p| own(p) && p.tracks.iter().any(|t| t.uri == uri)) {
            for track in playlist.tracks.iter_mut().filter(|t| t.uri == uri) {
                track.uri = youtube.clone();
            }
            lib.likes.local.update(&playlist);
        }
    });
    models::rename_uri(uri, &youtube, video);
}

/// Told once per break, and downloads start again after it, one song at a time.
fn tell_block(ui: &MainWindow) {
    let recent = BLOCK_TOLD.with(|b| b.get()).is_some_and(|at| at.elapsed() < RETRY_AFTER_BLOCK);
    if recent {
        return;
    }
    BLOCK_TOLD.with(|b| b.set(Some(Instant::now())));
    crate::toast_error(ui, t("YouTube ha chiesto una pausa: i download ripartono da soli tra un quarto d'ora.", "YouTube asked for a break: downloads start again by themselves in a quarter of an hour."));
    Timer::single_shot(RETRY_AFTER_BLOCK, || with(resume_all));
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
        Some((done, _, total)) => {
            let current: f32 = CURRENT.with(|c| playlist.tracks.iter().filter_map(|t| c.borrow().get(&t.uri).copied()).sum());
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
            p.borrow_mut().entry(uri.to_string()).or_insert((0, 0, 0));
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
        stop_playlist(&playlist.uri());
        PROGRESS.with(|p| p.borrow_mut().remove(&playlist.uri()));
        for track in &playlist.tracks {
            if ACTIVE.with(|a| a.borrow().contains(&track.uri)) {
                settle(&track.uri);
            }
        }
        crate::toast(&ui, &format!("{} «{}»", t("Download interrotto:", "Download stopped:"), playlist.name));
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
        Some(name) => crate::toast(&ui, &format!("{} «{name}»", t("Resta scaricato in", "Stays downloaded in"))),
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

fn save(change: impl FnOnce(&mut AppConfig)) {
    if let Ok(mut config) = AppConfig::load_or_create() {
        change(&mut config);
        let _ = config.save();
    }
}

/// The switch in Settings. Turning it off keeps what was downloaded.
pub fn set_enabled(lib: &Library, on: bool) {
    let Some(ui) = lib.ui.upgrade() else { return };
    save(|c| c.offline = on);
    ui.set_offline_enabled(on);
    if on {
        resume_all(lib);
    } else {
        stop_all_runs();
        clear_all();
    }
}

/// High quality or space saver; it applies to the songs downloaded from now on.
pub fn set_quality(lib: &Library, saver: bool) {
    save(|c| c.download_quality = if saver { "saver" } else { "high" }.into());
    if let Some(ui) = lib.ui.upgrade() {
        ui.set_download_saver(saver);
    }
}

/// Three songs at a time; it takes effect at the next songs.
pub fn set_fast(lib: &Library, on: bool) {
    save(|c| c.fast_downloads = on);
    if let Some(ui) = lib.ui.upgrade() {
        ui.set_fast_downloads(on);
    }
    queue().ready.notify_all();
}

/// "Delete downloads" in Settings: every file goes, and nothing is kept offline any more.
pub fn delete_all(lib: &Library) {
    let Some(ui) = lib.ui.upgrade() else { return };
    lib.likes.local.forget_offline();
    stop_all_runs();
    clear_all();
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

    #[test]
    fn workers_share_the_songs_that_wait() {
        assert_eq!(share(20, 1), BATCH, "one worker: runs of up to BATCH songs");
        assert_eq!(share(4, 3), 2, "few songs: each worker gets some");
        assert_eq!(share(1, 3), 1);
        assert_eq!(share(40, 3), BATCH);
    }
}
