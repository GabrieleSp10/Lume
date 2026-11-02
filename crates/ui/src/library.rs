//! Spotify library features behind the UI: home data, search, playlists, history, likes,
//! the queue and lyrics. API calls run on short-lived worker threads; results come back to
//! the UI thread through `upgrade_in_event_loop`.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use app_core::{
    api::{AlbumSummary, Api, ApiError, PlaylistSummary, TrackSummary},
    limiter::Usage,
    local_library::{ImportedPlaylist, LocalLibrary, LOCAL_PLAYLIST},
    spotify_embed,
    auth::AuthError,
    config::AppConfig,
    lyrics::{self, Lyrics},
};
use app_player::{Command, PlayRequest, PlaybackEngine, Track, Ytdlp};
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::{i18n::t, images, memory, models, nav, LyricLine, MainWindow, PageState};

/// How many tracks a "play from here" request queues up when there is no context to play.
const PLAY_BATCH: usize = 100;
/// Home shows pages of tiles; the arrows move through the rest.
const QUICK_PICKS: usize = 30;
/// Upper bound for one playlist/library load, far above any normal playlist.
const MAX_TRACKS: usize = 10_000;
const MAX_RECENT_SEARCHES: usize = 6;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(600);
const HISTORY_PAGE: i32 = 3;
const DETAIL_PAGE: i32 = 5;
const LYRICS_PAGE: i32 = 6;
/// Spotify is not asked again for a list seen this recently; the reload button still does.
/// A playlist whose snapshot did not change is not asked again at all.
const DETAIL_FRESH: Duration = Duration::from_secs(6 * 3600);
/// Albums practically never change.
const ALBUM_FRESH: Duration = Duration::from_secs(30 * 24 * 3600);
const HOME_FRESH: Duration = Duration::from_secs(30 * 60);
const HISTORY_FRESH: Duration = Duration::from_secs(300);
const HOME_KEY: &str = "home";
const HISTORY_KEY: &str = "history";
/// Songs kept in the history this app records itself.
const HISTORY_SIZE: usize = 50;
const LIKED_KEY: &str = "liked";
/// Lets Spotify's player take a queued song before the panel asks for the queue.
const QUEUE_SETTLE: Duration = Duration::from_millis(800);

/// Fetches the tracks of a playlist, album or the liked songs (runs on a worker thread).
type DetailLoader = Box<dyn FnOnce(&Api) -> Result<Vec<TrackSummary>, ApiError> + Send>;
/// Opened lists by key; the disk has them too, this avoids reading it again.
type DetailCache = Arc<Mutex<HashMap<String, MemoryList>>>;

#[derive(Clone)]
struct MemoryList {
    read_at: Instant,
    snapshot: String,
    tracks: Vec<TrackSummary>,
}

/// A list as saved on disk.
#[derive(Serialize, Deserialize)]
struct SavedList {
    snapshot: String,
    tracks: Vec<TrackSummary>,
}

/// The Home page as saved on disk.
#[derive(Serialize, Deserialize)]
struct SavedHome {
    playlists: Vec<PlaylistSummary>,
    recent: Vec<TrackSummary>,
}

/// Whether songs are liked, asked of Spotify for its songs while logged in and kept in this
/// app for everything else.
#[derive(Clone)]
struct Likes {
    local: LocalLibrary,
    spotify: Arc<AtomicBool>,
}

impl Likes {
    fn on_spotify(&self, uri: &str) -> bool {
        self.spotify.load(Ordering::SeqCst) && uri.starts_with("spotify:track:")
    }

    fn flags(&self, api: &Api, tracks: &[TrackSummary]) -> Vec<bool> {
        let ask: Vec<String> = tracks.iter().filter(|t| self.on_spotify(&t.uri)).map(|t| t.uri.clone()).collect();
        let answers: HashMap<String, bool> = if ask.is_empty() {
            HashMap::new()
        } else {
            let flags = api.contains(&ask).unwrap_or_default();
            ask.into_iter().zip(flags).collect()
        };
        let local: std::collections::HashSet<String> = self.local.liked().into_iter().map(|t| t.uri).collect();
        tracks
            .iter()
            .map(|t| if self.on_spotify(&t.uri) { answers.get(&t.uri).copied().unwrap_or(false) } else { local.contains(&t.uri) })
            .collect()
    }

    fn set(&self, api: &Api, track: &TrackSummary, liked: bool) -> Result<(), ApiError> {
        if self.on_spotify(&track.uri) {
            api.set_liked(&track.uri, liked)
        } else {
            self.local.set_liked(track, liked);
            Ok(())
        }
    }
}

pub struct Library {
    api: Arc<Api>,
    likes: Likes,
    engine: Arc<dyn PlaybackEngine>,
    ui: Weak<MainWindow>,
    search_timer: Timer,
    search_seq: Arc<AtomicU64>,
    detail_seq: Arc<AtomicU64>,
    /// Playlists and albums already opened in this run.
    detail_cache: DetailCache,
    /// Latest known snapshot of each of the user's playlists, by URI.
    snapshots: Arc<Mutex<HashMap<String, String>>>,
    /// When the history was last read.
    history_at: Cell<Option<Instant>>,
    /// The open detail page, to reload it on "Riprova".
    detail_item: RefCell<Option<crate::Item>>,
    /// Imported playlists whose covers were already asked for in this run.
    covers_asked: RefCell<std::collections::HashSet<String>>,
    /// The detail page is being rebuilt in place (new language): stay on the current page.
    detail_in_place: Cell<bool>,
    /// Playlist/album URI of the open detail page (None for liked songs).
    detail_context: RefCell<Option<String>>,
    now_track: RefCell<Option<Track>>,
    /// URI whose lyrics are loaded (or loading), to avoid refetching.
    lyrics_uri: RefCell<String>,
    /// The song restored from the last run has not been started yet.
    resume_pending: Cell<bool>,
    /// The saved queue goes back in once the resumed song starts.
    restore_queue: Cell<bool>,
}

thread_local! {
    static LIBRARY: RefCell<Option<Rc<Library>>> = const { RefCell::new(None) };
    /// Start times of the loaded synced lyrics, for the position timer.
    static LYRIC_TIMES: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// Makes the library reachable from event handlers running on the UI thread.
pub fn install(lib: Rc<Library>) {
    LIBRARY.with(|l| *l.borrow_mut() = Some(lib));
}

pub fn with(f: impl FnOnce(&Library)) {
    LIBRARY.with(|l| {
        if let Some(lib) = l.borrow().as_ref() {
            f(lib);
        }
    });
}

/// The lyric line being sung (-1 if none) and how far through it we are (0..1). UI thread only.
pub fn lyric_position(position_ms: u32) -> (i32, f32) {
    LYRIC_TIMES.with(|times| {
        let times = times.borrow();
        let Some(line) = lyrics::current_line(&times, position_ms) else { return (-1, 0.0) };
        // The last line has no successor: assume a few seconds.
        let start = times[line];
        let end = times.get(line + 1).copied().unwrap_or(start + 4000);
        let length = end.saturating_sub(start).max(1) as f32;
        (line as i32, ((position_ms - start) as f32 / length).min(1.0))
    })
}

pub fn page_state(kind: i32, title: &str, text: &str, action: &str) -> PageState {
    PageState { kind, title: title.into(), text: text.into(), action: action.into() }
}

fn content() -> PageState {
    PageState::default()
}

fn loading() -> PageState {
    page_state(1, "", "", "")
}

fn empty(title: &str, text: &str) -> PageState {
    page_state(2, title, text, "")
}

fn failed(e: &ApiError) -> PageState {
    let (title, text) = explain(e);
    page_state(3, title, &text, retry())
}

fn retry() -> &'static str {
    t("Riprova", "Try again")
}

/// What to tell the user about a failed request: a title and the way out.
fn explain(e: &ApiError) -> (&'static str, String) {
    eprintln!("spotify request failed: {e}");
    if let ApiError::RateLimited { seconds } = e {
        return (t("Spotify chiede una pausa", "Spotify asks for a pause"), format!("{} {}.", t("Troppe richieste: si riparte tra", "Too many requests: back in"), wait_text(*seconds)));
    }
    let (title, text) = explain_failure(e);
    (title, text.to_string())
}

fn explain_failure(e: &ApiError) -> (&'static str, &'static str) {
    match e {
        ApiError::Auth(AuthError::NotLoggedIn) => (t("Accesso scaduto", "Session expired"), t("Accedi di nuovo a Spotify per continuare.", "Log in to Spotify again to continue.")),
        ApiError::Network(m) | ApiError::Auth(AuthError::Network(m)) if is_dns_problem(m) => (
            t("Spotify non si raggiunge", "Spotify can't be reached"),
            t("Il nome del sito non si trova: se usi una VPN o un filtro DNS, prova a disattivarlo.", "The site's name can't be found: if you use a VPN or a DNS filter, try turning it off."),
        ),
        ApiError::Network(_) | ApiError::Auth(AuthError::Network(_)) => (t("Nessuna connessione", "No connection"), t("Controlla la rete e riprova.", "Check your network and try again.")),
        _ => match e.status() {
            Some(403) => (t("Contenuto non disponibile", "Not available"), t("Spotify permette di aprire solo le tue playlist e quelle collaborative.", "Spotify only lets you open your own and collaborative playlists.")),
            Some(404) => (t("Non trovato", "Not found"), t("Questo contenuto non è più disponibile.", "This content is no longer available.")),
            Some(429) => (t("Troppe richieste", "Too many requests"), t("Aspetta qualche secondo e riprova.", "Wait a few seconds and try again.")),
            _ => (t("Spotify non risponde", "Spotify is not responding"), t("Riprova tra poco.", "Try again shortly.")),
        },
    }
}

fn brief(e: &ApiError) -> String {
    let (title, text) = explain(e);
    format!("{title}. {text}")
}

const STARTING: (&str, &str) = ("Il player si sta avviando: riprova tra un attimo.", "The player is starting: try again in a moment.");

/// Turns a technical message from the hidden player into something a listener can act on.
pub fn engine_message(raw: &str) -> &'static str {
    eprintln!("engine error: {raw}");
    match raw {
        r if r.contains("account_error") => t("Per riprodurre musica serve Spotify Premium.", "Playing music requires Spotify Premium."),
        r if r.contains("authentication_error") => t("Accesso scaduto: accedi di nuovo a Spotify.", "Session expired: log in to Spotify again."),
        r if r.contains("playback_error") => t("Questo brano non si riesce a riprodurre. Provane un altro.", "This song can't be played. Try another one."),
        r if r.contains("initialization_error") => t("Il player non si è avviato. Riapri l'app.", "The player didn't start. Reopen the app."),
        r if r.contains("not ready") || r.contains("not running") => t(STARTING.0, STARTING.1),
        r if r.contains("offline") => t("Il player si è scollegato. Sto riprovando…", "The player disconnected. Retrying…"),
        r if r.contains("-> 403") => t("Spotify non permette questa operazione.", "Spotify doesn't allow this."),
        r if r.contains("-> 404") => t("Nessun player attivo: avvia prima un brano.", "No active player: start a song first."),
        r if r.contains("-> 429") => t("Troppe richieste: aspetta qualche secondo.", "Too many requests: wait a few seconds."),
        r if is_dns_problem(r) => t(
            "Non raggiungo YouTube: il nome del sito non si trova. Se usi una VPN o un filtro DNS, prova a disattivarlo.",
            "Can't reach YouTube: the site's name can't be found. If you use a VPN or a DNS filter, try turning it off.",
        ),
        r if r.starts_with("hls") || r.contains("hls.js") => t("YouTube ha dato un flusso che non si riesce a riprodurre. Provane un altro.", "YouTube gave a stream that can't be played. Try another one."),
        r if r.contains("yt-dlp missing") => t("Per ascoltare da YouTube serve yt-dlp: trovi il link nelle Impostazioni.", "Playing from YouTube needs yt-dlp: the link is in Settings."),
        r if r.contains("cannot open YouTube login") => t("La finestra di accesso a YouTube non si è aperta. Riapri l'app e riprova.", "The YouTube login window didn't open. Reopen the app and try again."),
        r if r.contains("YouTube login not saved") => t("Accesso a YouTube non salvato: controlla lo spazio su disco e riprova.", "YouTube login not saved: check disk space and try again."),
        r if r.contains("not a bot") => t(
            "YouTube ha bloccato le richieste da questo computer. Collega il tuo account YouTube in Impostazioni › Avanzate › Riproduzione, oppure riprova tra un quarto d'ora.",
            "YouTube blocked requests from this computer. Connect your YouTube account in Settings › Advanced › Playback, or try again in fifteen minutes.",
        ),
        r if r.contains("Requested format is not available") => t(
            "YouTube non ha dato l'audio: aggiorna yt-dlp con «pip install -U \"yt-dlp[default]\"» e controlla che Deno sia installato.",
            "YouTube gave no audio: update yt-dlp with «pip install -U \"yt-dlp[default]\"» and check that Deno is installed.",
        ),
        r if r.contains("confirm your age") => t("Questo video è vietato ai minori: serve l'account YouTube collegato.", "This video is age-restricted: it needs your YouTube account connected."),
        r if r.contains("cookies") || r.contains("Could not copy") => t(
            "L'accesso a YouTube non è più valido: scollega l'account e collegalo di nuovo.",
            "The YouTube login is no longer valid: disconnect the account and connect it again.",
        ),
        r if r.contains("Sign in") => t("YouTube chiede l'accesso: collega il tuo account YouTube nelle Impostazioni.", "YouTube asks you to log in: connect your YouTube account in Settings."),
        r if r.contains("yt-dlp") => t("YouTube non ha dato l'audio: aggiorna yt-dlp e riprova.", "YouTube gave no audio: update yt-dlp and try again."),
        r if r.contains("no youtube match") => t("Questo brano non si trova su YouTube.", "This song can't be found on YouTube."),
        r if r.contains("librespot") => t("librespot non riesce a riprodurre con questo account.", "librespot can't play with this account."),
        _ => t("La riproduzione non è riuscita. Riprova.", "Playback failed. Try again."),
    }
}

/// A name lookup failed: the network may be up while a VPN or DNS filter hides the site.
pub fn is_dns_problem(message: &str) -> bool {
    const SIGNS: [&str; 6] = ["11001", "getaddrinfo", "Host sconosciuto", "No such host", "Name or service not known", "Failed to resolve"];
    SIGNS.iter().any(|sign| message.contains(sign))
}

/// Tells that another engine took over, and for how long if Spotify said so.
pub fn switch_message(to: app_player::EngineKind, blocked_seconds: u64) -> String {
    if to == app_player::EngineKind::Spotify {
        return t("Riproduco con Spotify: YouTube non risponde.", "Playing with Spotify: YouTube isn't responding.").to_string();
    }
    let lead = t("Riproduco da YouTube", "Playing from YouTube");
    match blocked_seconds {
        0 => format!("{lead}: {}", t("il motore scelto non funziona.", "the chosen engine isn't working.")),
        s => format!("{lead}: {} {}.", t("Spotify è in pausa ancora per", "Spotify is paused for"), wait_text(s)),
    }
}

/// A wait in the unit that reads best.
pub fn wait_text(seconds: u64) -> String {
    match seconds {
        0..=90 => format!("{seconds} {}", t("secondi", "seconds")),
        91..=5400 => format!("{} {}", seconds.div_ceil(60), t("minuti", "minutes")),
        _ => format!("{} {}", seconds.div_ceil(3600), t("ore", "hours")),
    }
}

/// What Settings says about Spotify's limits; empty when there are none.
pub fn usage_note(usage: &Usage) -> String {
    let wait = wait_text(usage.blocked_seconds);
    if usage.playback_blocked {
        format!("{} {wait}.", t("Spotify non fa partire nuovi brani per altre", "Spotify won't start new songs for another"))
    } else if usage.blocked_endpoints > 0 {
        format!(
            "{} {wait}: {}",
            t("Spotify limita cronologia, preferiti o cuori per altre", "Spotify limits history, likes or hearts for another"),
            t("la musica non si ferma.", "the music keeps playing."),
        )
    } else {
        String::new()
    }
}

/// Runs `work` on a worker thread, then `then` on the UI thread with its result.
fn spawn_job<T: Send + 'static>(
    api: Arc<Api>,
    ui: Weak<MainWindow>,
    work: impl FnOnce(&Api) -> T + Send + 'static,
    then: impl FnOnce(&MainWindow, T) + Send + 'static,
) {
    thread::spawn(move || {
        let result = work(&api);
        let _ = ui.upgrade_in_event_loop(move |ui| then(&ui, result));
    });
}

fn source_name(p: &ImportedPlaylist) -> &'static str {
    if p.source == "youtube" { "YouTube" } else { "Spotify" }
}

fn imported_summary(p: &ImportedPlaylist) -> PlaylistSummary {
    PlaylistSummary {
        id: p.id.clone(),
        uri: p.uri(),
        name: p.name.clone(),
        // The source has its own label on the playlist page; the card keeps just the owner.
        owner: if p.owner.is_empty() { source_name(p).to_string() } else { p.owner.clone() },
        image_url: p.image_url.clone(),
        track_count: Some(p.tracks.len() as u32),
        snapshot_id: String::new(),
    }
}

fn import_message(e: &str) -> String {
    let text = match e {
        e if e.contains("not a spotify") => t("Il link non è una playlist o un album di Spotify.", "The link isn't a Spotify playlist or album."),
        e if e.contains("not a youtube") => t("Il link non è una playlist di YouTube.", "The link isn't a YouTube playlist."),
        e if e.contains("no songs") => t("In questa playlist non ci sono brani.", "This playlist has no songs."),
        e if e.contains("spotify embed") => t("Playlist non trovata: deve essere pubblica.", "Playlist not found: it must be public."),
        e if e.starts_with("network") => t("Nessuna connessione: controlla la rete.", "No connection: check your network."),
        e => engine_message(e),
    };
    format!("{} {text}", t("Importazione non riuscita.", "Import failed."))
}

/// "100 of 270 songs" when only part of the playlist came in.
fn imported_count(p: &ImportedPlaylist) -> String {
    let read = p.tracks.len() as u32;
    match (read, p.total) {
        (1, total) if total <= 1 => t("1 brano", "1 song").to_string(),
        (read, total) if total > read => format!("{read} {} {total} {}", t("di", "of"), t("brani", "songs")),
        (read, _) => format!("{read} {}", t("brani", "songs")),
    }
}

/// Reads a playlist from a link; runs on a worker thread. With a Spotify login, the Web API
/// fills in what the embed page leaves out (it works for the user's own playlists).
fn import_from(source: i32, link: &str, api: &Api, spotify: bool) -> Result<ImportedPlaylist, String> {
    if source == 0 {
        let mut playlist = spotify_embed::import(link)?;
        let id = spotify_embed::parse_link(link).filter(|(kind, _)| *kind == "playlist").map(|(_, id)| id);
        if let (true, Some(id), true) = (spotify, id, playlist.total > playlist.tracks.len() as u32) {
            match api.playlist_tracks(&id, MAX_TRACKS) {
                Ok(all) if all.len() > playlist.tracks.len() => playlist.tracks = all,
                Ok(_) => {}
                Err(e) => eprintln!("full playlist not read: {e}"),
            }
        }
        return Ok(playlist);
    }
    let (name, owner, tracks) = Ytdlp::from_config(&AppConfig::load_or_create().unwrap_or_default()).playlist(link)?;
    if tracks.is_empty() {
        return Err("no songs".into());
    }
    let list: String = link.split("list=").nth(1).unwrap_or(link).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    Ok(ImportedPlaylist {
        id: format!("youtube-{list}"),
        name,
        owner,
        image_url: tracks[0].image_url.clone(),
        source: "youtube".into(),
        total: tracks.len() as u32,
        tracks: tracks.into_iter().map(summary_of).collect(),
    })
}

/// Home when there is no Spotify account and nothing yet to show.
fn home_start() -> PageState {
    page_state(
        2,
        t("Inizia dalla tua musica", "Start with your music"),
        t("Importa una playlist da Spotify o da YouTube, oppure cerca un brano.", "Import a playlist from Spotify or YouTube, or search for a song."),
        t("Importa una playlist", "Import a playlist"),
    )
}

/// Puts the song first in the history this app keeps, which needs no quota.
fn push_history(history: &mut Vec<TrackSummary>, track: TrackSummary) {
    history.retain(|t| t.uri != track.uri);
    history.insert(0, track);
    history.truncate(HISTORY_SIZE);
}

fn detail_key(item: &crate::Item) -> String {
    if item.kind == 3 { LIKED_KEY.to_string() } else { item.uri.to_string() }
}

fn summary_of(track: Track) -> TrackSummary {
    TrackSummary {
        uri: track.uri,
        name: track.name,
        artists: track.artists,
        album: track.album,
        image_url: track.image_url,
        duration_ms: track.duration_ms,
        explicit: false,
    }
}

fn track_of(summary: TrackSummary) -> Track {
    Track {
        uri: summary.uri,
        name: summary.name,
        artists: summary.artists,
        album: summary.album,
        image_url: summary.image_url,
        duration_ms: summary.duration_ms,
    }
}

fn track_items(tracks: &[TrackSummary], flags: &[bool], ui: &Weak<MainWindow>) -> Vec<crate::Item> {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| models::track_item(t, flags.get(i).copied().unwrap_or(false), ui))
        .collect()
}

fn history_empty() -> PageState {
    empty(t("Ancora nessun ascolto", "Nothing played yet"), t("I brani che ascolti compariranno qui.", "The songs you play will show up here."))
}

pub fn lyrics_idle() -> PageState {
    empty(t("Nessun brano in riproduzione", "Nothing playing"), t("Avvia un brano per vederne il testo.", "Start a song to see its lyrics."))
}

fn hearts_job(api: Arc<Api>, likes: Likes, ui: Weak<MainWindow>, detail_seq: Arc<AtomicU64>, tracks: Vec<TrackSummary>, seq: u64) {
    spawn_job(
        api,
        ui,
        move |api| {
            let flags = likes.flags(api, &tracks);
            (tracks.into_iter().map(|t| t.uri).collect::<Vec<_>>(), flags)
        },
        move |_, (uris, flags)| {
            if detail_seq.load(Ordering::SeqCst) == seq {
                models::apply_likes(&models::get().detail_tracks, &uris, &flags);
            }
        },
    );
}

fn show_detail(ui: &MainWindow, tracks: &[TrackSummary], subtitle: &str, ui_weak: &Weak<MainWindow>) {
    models::get().detail_tracks.set_vec(track_items(tracks, &[], ui_weak));
    // One cover on every row says nothing: the rows then go without.
    ui.set_detail_uniform_art(tracks.len() > 1 && tracks.iter().all(|t| t.image_url == tracks[0].image_url));
    ui.set_detail_state(match (tracks.is_empty(), ui.get_detail_liked_songs()) {
        (true, true) => empty(t("Ancora nessun preferito", "No favorites yet"), t("Premi il cuore accanto a un brano per ritrovarlo qui.", "Press the heart next to a song to find it here.")),
        (true, false) => empty(t("Qui non c'è nulla da ascoltare", "Nothing to play here"), t("Questa raccolta non contiene brani riproducibili.", "This collection has no playable songs.")),
        _ => content(),
    });
    let count = match tracks.len() {
        1 => t("1 brano", "1 song").to_string(),
        n => format!("{n} {}", t("brani", "songs")),
    };
    let count = if ui.get_detail_count().is_empty() { count } else { ui.get_detail_count().to_string() };
    ui.set_detail_subtitle(if subtitle.is_empty() { count } else { format!("{subtitle} · {count}") }.into());
}

fn search_start() -> PageState {
    empty(t("Cerca la tua musica", "Search your music"), t("Scrivi il titolo di un brano, un album o una playlist.", "Type the name of a song, an album or a playlist."))
}

/// Shows the saved searches and resets the page; call at startup.
pub fn init_search_page(ui: &MainWindow, recent: &[String]) {
    ui.set_search_state(search_start());
    show_recent(ui, recent);
}

fn show_recent(ui: &MainWindow, recent: &[String]) {
    let rows: Vec<SharedString> = recent.iter().map(|s| s.as_str().into()).collect();
    ui.set_recent_searches(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// Puts `query` first in the saved searches, without duplicates.
fn push_recent(recent: &mut Vec<String>, query: &str) {
    recent.retain(|q| !q.eq_ignore_ascii_case(query));
    recent.insert(0, query.to_string());
    recent.truncate(MAX_RECENT_SEARCHES);
}

fn remember_search(ui: &MainWindow, query: &str) {
    let Ok(mut config) = AppConfig::load_or_create() else { return };
    push_recent(&mut config.recent_searches, query);
    let _ = config.save();
    show_recent(ui, &config.recent_searches);
}

impl Library {
    pub fn new(api: Arc<Api>, engine: Arc<dyn PlaybackEngine>, ui: Weak<MainWindow>) -> Self {
        Self {
            api,
            likes: Likes { local: LocalLibrary::default(), spotify: Arc::default() },
            engine,
            ui,
            search_timer: Timer::default(),
            search_seq: Arc::default(),
            detail_seq: Arc::default(),
            detail_cache: Arc::default(),
            snapshots: Arc::default(),
            history_at: Cell::new(None),
            detail_item: RefCell::default(),
            detail_in_place: Cell::new(false),
            covers_asked: RefCell::default(),
            detail_context: RefCell::default(),
            now_track: RefCell::default(),
            lyrics_uri: RefCell::default(),
            resume_pending: Cell::new(false),
            restore_queue: Cell::new(false),
        }
    }

    fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Api) -> T + Send + 'static,
        then: impl FnOnce(&MainWindow, T) + Send + 'static,
    ) {
        spawn_job(Arc::clone(&self.api), self.ui.clone(), work, then);
    }

    // ---- home ----

    pub fn refresh_home(&self) {
        self.load_home(false);
    }

    /// Logged in to Spotify (true) or using this app's own library (false).
    pub fn set_spotify(&self, on: bool) {
        self.likes.spotify.store(on, Ordering::SeqCst);
    }

    fn spotify(&self) -> bool {
        self.likes.spotify.load(Ordering::SeqCst)
    }

    /// Home without Spotify: imported playlists and what played here.
    fn local_home(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        show_playlists(&[], &self.likes.local, &self.ui);
        let mut recent = self.played_here();
        recent.truncate(QUICK_PICKS);
        let flags = self.likes.flags(&self.api, &recent);
        models::get().quick_picks.set_vec(track_items(&recent, &flags, &self.ui));
        let nothing = recent.is_empty() && self.likes.local.playlists().is_empty();
        ui.set_home_state(if nothing { home_start() } else { content() });
    }

    /// Shows the saved Home at once, and asks Spotify only if it is old or `force` is set.
    fn load_home(&self, force: bool) {
        if !self.spotify() {
            self.local_home();
            return;
        }
        let m = models::get();
        let saved = self.api.store().load::<SavedHome>(HOME_KEY);
        let fresh = saved.as_ref().is_some_and(|s| s.age < HOME_FRESH);
        if let (Some(saved), Some(ui)) = (saved, self.ui.upgrade()) {
            self.remember_snapshots(&saved.value.playlists);
            show_playlists(&saved.value.playlists, &self.likes.local, &self.ui);
            let recent = if saved.value.recent.is_empty() { self.played_here() } else { saved.value.recent };
            m.quick_picks.set_vec(track_items(&recent, &[], &self.ui));
            ui.set_home_state(content());
            let likes = self.likes.clone();
            self.run(
                move |api| {
                    let flags = likes.flags(api, &recent);
                    (recent.into_iter().map(|t| t.uri).collect::<Vec<_>>(), flags)
                },
                |_, (uris, flags)| models::apply_likes(&models::get().quick_picks, &uris, &flags),
            );
        }
        if fresh && !force {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            if m.quick_picks.row_count() == 0 {
                ui.set_home_state(loading());
            }
        }
        let ui_weak = self.ui.clone();
        let snapshots = Arc::clone(&self.snapshots);
        let likes = self.likes.clone();
        let local = self.likes.local.clone();
        self.run(
            move |api| {
                let (playlists, mut recent) = thread::scope(|scope| {
                    let playlists = scope.spawn(|| api.playlists(200));
                    let recent = api.recently_played();
                    (playlists.join().expect("playlist worker panicked"), recent)
                });
                if recent.as_ref().map_or(true, |r| r.is_empty()) {
                    // New or quiet account: fall back to the latest liked songs.
                    if let Ok(liked) = api.liked_tracks(QUICK_PICKS) {
                        recent = Ok(liked);
                    }
                }
                // Spotify's list can be blocked for hours: what played here still counts.
                let played_here = || api.store().load::<Vec<TrackSummary>>(HISTORY_KEY).map(|c| c.value).filter(|l| !l.is_empty());
                let recent = match recent {
                    Err(_) if played_here().is_some() => Ok(played_here().unwrap_or_default()),
                    other => other,
                };
                let recent = recent.map(|mut r| {
                    r.truncate(QUICK_PICKS);
                    let flags = likes.flags(api, &r);
                    (r, flags)
                });
                if let Ok(playlists) = &playlists {
                    // Saved even when the songs failed: the playlists need no asking again.
                    let recent = match &recent {
                        Ok((recent, _)) => recent.clone(),
                        Err(_) => api.store().load::<SavedHome>(HOME_KEY).map(|c| c.value.recent).unwrap_or_default(),
                    };
                    api.store().save(HOME_KEY, &SavedHome { playlists: playlists.clone(), recent });
                }
                (playlists, recent)
            },
            move |ui, (playlists, recent)| {
                let m = models::get();
                let mut problem = None;
                match playlists {
                    Ok(list) => {
                        snapshots.lock().unwrap().extend(list.iter().map(|p| (p.uri.clone(), p.snapshot_id.clone())));
                        show_playlists(&list, &local, &ui_weak);
                    }
                    Err(e) => problem = Some(e),
                }
                match recent {
                    Ok((tracks, flags)) => m.quick_picks.set_vec(track_items(&tracks, &flags, &ui_weak)),
                    Err(e) => problem = Some(e),
                }
                let nothing_shown = m.quick_picks.row_count() == 0 && m.playlists.row_count() == 0;
                match problem {
                    Some(e) if nothing_shown => ui.set_home_state(failed(&e)),
                    Some(e) => {
                        ui.set_home_state(content());
                        // Saved data is on screen: only say so if the user asked for new data.
                        if force {
                            crate::toast(ui, &brief(&e));
                        }
                    }
                    None => ui.set_home_state(content()),
                }
            },
        );
    }

    fn played_here(&self) -> Vec<TrackSummary> {
        self.api.store().load::<Vec<TrackSummary>>(HISTORY_KEY).map(|c| c.value).unwrap_or_default()
    }

    fn record_played(&self, track: &Track) {
        let mut history = self.played_here();
        push_history(&mut history, summary_of(track.clone()));
        self.api.store().save(HISTORY_KEY, &history);
    }

    fn remember_snapshots(&self, playlists: &[PlaylistSummary]) {
        self.snapshots.lock().unwrap().extend(playlists.iter().map(|p| (p.uri.clone(), p.snapshot_id.clone())));
    }

    /// The title bar's reload button and every "Riprova".
    pub fn refresh(&self, page: i32) {
        match page {
            1 => {
                if let Some(ui) = self.ui.upgrade() {
                    self.search_submit(&ui.get_search_text());
                }
            }
            DETAIL_PAGE => {
                self.detail_cache.lock().unwrap().clear();
                if let Some(item) = self.detail_item.borrow().as_ref() {
                    self.api.store().remove(&detail_key(item));
                }
                self.reload_detail();
            }
            LYRICS_PAGE => {
                self.lyrics_uri.borrow_mut().clear();
                self.load_lyrics();
            }
            HISTORY_PAGE => {
                self.history_at.set(None);
                self.load_history();
            }
            _ => self.load_home(true),
        }
    }

    pub fn navigated(&self, page: i32) {
        match page {
            HISTORY_PAGE => self.load_history(),
            LYRICS_PAGE => self.load_lyrics(),
            _ => {}
        }
    }

    fn load_history(&self) {
        if !self.spotify() {
            let Some(ui) = self.ui.upgrade() else { return };
            let tracks = self.played_here();
            let flags = self.likes.flags(&self.api, &tracks);
            models::get().history.set_vec(track_items(&tracks, &flags, &self.ui));
            ui.set_history_state(if tracks.is_empty() { history_empty() } else { content() });
            return;
        }
        if self.history_at.get().is_some_and(|at| at.elapsed() < HISTORY_FRESH) {
            return;
        }
        self.history_at.set(Some(Instant::now()));
        if let Some(ui) = self.ui.upgrade() {
            if models::get().history.row_count() == 0 {
                ui.set_history_state(loading());
            }
        }
        let ui_weak = self.ui.clone();
        let likes = self.likes.clone();
        self.run(
            move |api| {
                let tracks = api.recently_played().or_else(|e| match api.store().load::<Vec<TrackSummary>>(HISTORY_KEY) {
                    Some(saved) => Ok(saved.value),
                    None => Err(e),
                });
                tracks.map(|t| {
                    let flags = likes.flags(api, &t);
                    (t, flags)
                })
            },
            move |ui, result| match result {
                Ok((tracks, flags)) => {
                    models::get().history.set_vec(track_items(&tracks, &flags, &ui_weak));
                    ui.set_history_state(if tracks.is_empty() { history_empty() } else { content() });
                }
                Err(e) => ui.set_history_state(failed(&e)),
            },
        );
    }

    // ---- search ----

    /// Called on every keystroke; waits for a pause before searching.
    pub fn search_edited(&self, text: &str) {
        let (api, likes, ui, seq, query) = (Arc::clone(&self.api), self.likes.clone(), self.ui.clone(), Arc::clone(&self.search_seq), text.to_string());
        self.search_timer.start(TimerMode::SingleShot, SEARCH_DEBOUNCE, move || {
            run_search(Arc::clone(&api), likes.clone(), ui.clone(), Arc::clone(&seq), query.clone());
        });
    }

    pub fn search_submit(&self, text: &str) {
        self.search_timer.stop();
        if let (Some(ui), query) = (self.ui.upgrade(), text.trim()) {
            if query.chars().count() >= 2 {
                remember_search(&ui, query);
            }
        }
        run_search(Arc::clone(&self.api), self.likes.clone(), self.ui.clone(), Arc::clone(&self.search_seq), text.to_string());
    }

    // ---- detail pages ----

    pub fn open_item(&self, source: i32, index: i32) {
        let m = models::get();
        let model = match source {
            0 => &m.playlists,
            1 => &m.results_albums,
            2 => &m.results_playlists,
            _ => return,
        };
        let Some(item) = model.row_data(index as usize) else { return };
        self.open_detail(item);
    }

    /// Rebuilds the last detail page without going to it.
    fn reload_detail(&self) {
        let item = self.detail_item.borrow().clone();
        if let Some(item) = item {
            self.detail_in_place.set(true);
            self.open_detail(item);
            self.detail_in_place.set(false);
        }
    }

    fn open_detail(&self, item: crate::Item) {
        *self.detail_item.borrow_mut() = Some(item.clone());
        let id = item.id.to_string();
        if item.kind == 3 && !self.spotify() {
            *self.detail_context.borrow_mut() = None;
            self.begin_detail(&item, "Playlist");
            self.show_local(self.likes.local.liked());
            return;
        }
        if item.uri.starts_with(LOCAL_PLAYLIST) {
            *self.detail_context.borrow_mut() = None;
            let playlist = self.likes.local.playlist(&item.uri).unwrap_or_default();
            self.begin_detail(&item, &imported_summary(&playlist).owner);
            if let Some(ui) = self.ui.upgrade() {
                ui.set_detail_local(true);
                ui.set_detail_source(format!("{} {}", t("Importata da", "Imported from"), source_name(&playlist)).into());
                ui.set_detail_count(imported_count(&playlist).into());
                if playlist.total > playlist.tracks.len() as u32 {
                    ui.set_detail_note(
                        t(
                            "Da un link Spotify arrivano solo i primi 100 brani. Collega il tuo account Spotify in Impostazioni › Avanzate e importala di nuovo per averli tutti.",
                            "A Spotify link brings only the first 100 songs. Connect your Spotify account in Settings › Advanced and import it again to get them all.",
                        )
                        .into(),
                    );
                }
            }
            let one_cover = playlist.tracks.len() > 1 && playlist.tracks.iter().all(|t| t.image_url == playlist.image_url);
            if playlist.source == "spotify" && one_cover {
                self.fill_covers(playlist.clone());
            }
            self.show_local(playlist.tracks);
            return;
        }
        match item.kind {
            3 => {
                *self.detail_context.borrow_mut() = None;
                self.begin_detail(&item, "Playlist");
                self.load_detail(LIKED_KEY.into(), "Playlist".into(), Box::new(|api| api.liked_tracks(MAX_TRACKS)));
            }
            2 => {
                *self.detail_context.borrow_mut() = Some(item.uri.to_string());
                self.begin_detail(&item, &item.subtitle);
                let album = AlbumSummary {
                    id: id.clone(),
                    name: item.title.to_string(),
                    image_url: item.image_url.to_string(),
                    ..Default::default()
                };
                self.load_detail(item.uri.to_string(), item.subtitle.to_string(), Box::new(move |api| api.album_tracks(&id, &album)));
            }
            _ => {
                *self.detail_context.borrow_mut() = Some(item.uri.to_string());
                // The card subtitle is "owner · N brani"; the page shows its own count.
                let owner = item.subtitle.split(" · ").next().unwrap_or_default().to_string();
                self.begin_detail(&item, &owner);
                self.load_detail(item.uri.to_string(), owner, Box::new(move |api| api.playlist_tracks(&id, MAX_TRACKS)));
            }
        }
    }

    fn begin_detail(&self, item: &crate::Item, subtitle: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        models::get().detail_tracks.set_vec(Vec::new());
        ui.set_detail_title(item.title.clone());
        ui.set_detail_subtitle(subtitle.into());
        ui.set_detail_state(loading());
        ui.set_detail_has_art(false);
        ui.set_detail_liked_songs(item.kind == 3);
        ui.set_detail_local(false);
        ui.set_detail_source("".into());
        ui.set_detail_note("".into());
        ui.set_detail_count("".into());
        models::set_detail_art_url(&item.image_url);
        images::request(&item.image_url, &self.ui);
        models::refresh_images(&ui);
        if !self.detail_in_place.get() {
            nav::go(&ui, DETAIL_PAGE);
        }
    }

    /// Shows a list kept in this app; it needs no request.
    fn show_local(&self, tracks: Vec<TrackSummary>) {
        let seq = self.detail_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(ui) = self.ui.upgrade() else { return };
        let subtitle = ui.get_detail_subtitle().to_string();
        show_detail(&ui, &tracks, &subtitle, &self.ui);
        self.load_hearts(tracks, seq);
    }

    /// The list as last read, from memory or the disk.
    fn saved_list(&self, key: &str) -> Option<MemoryList> {
        if let Some(list) = self.detail_cache.lock().unwrap().get(key) {
            return Some(list.clone());
        }
        let saved = self.api.store().load::<SavedList>(key)?;
        let read_at = Instant::now().checked_sub(saved.age)?;
        let list = MemoryList { read_at, snapshot: saved.value.snapshot, tracks: saved.value.tracks };
        self.detail_cache.lock().unwrap().insert(key.to_string(), list.clone());
        Some(list)
    }

    /// Shows the list right away if it was seen before, then refreshes it quietly if it may
    /// have changed. Hearts come from a second request, since checking likes on a long
    /// playlist takes several calls.
    fn load_detail(&self, key: String, subtitle: String, load: DetailLoader) {
        let seq = self.detail_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let cached = self.saved_list(&key);
        let snapshot = self.snapshots.lock().unwrap().get(&key).cloned().unwrap_or_default();
        let lifetime = if key.starts_with("spotify:album:") { ALBUM_FRESH } else { DETAIL_FRESH };
        let fresh = cached.as_ref().is_some_and(|list| {
            list.read_at.elapsed() < lifetime || (!snapshot.is_empty() && list.snapshot == snapshot)
        });
        let shown = cached.map(|list| list.tracks);
        if let (Some(tracks), Some(ui)) = (&shown, self.ui.upgrade()) {
            show_detail(&ui, tracks, &subtitle, &self.ui);
            if fresh {
                self.load_hearts(tracks.clone(), seq);
                return;
            }
        }
        let (detail_seq, api, ui_weak, cache, likes) =
            (Arc::clone(&self.detail_seq), Arc::clone(&self.api), self.ui.clone(), Arc::clone(&self.detail_cache), self.likes.clone());
        self.run(load, move |ui, result| {
            if detail_seq.load(Ordering::SeqCst) != seq {
                return; // another playlist was opened meanwhile
            }
            match result {
                Ok(tracks) => {
                    if shown.as_ref() != Some(&tracks) {
                        show_detail(ui, &tracks, &subtitle, &ui_weak);
                    }
                    let saved = SavedList { snapshot: snapshot.clone(), tracks: tracks.clone() };
                    let store = api.store().clone();
                    let saved_key = key.clone();
                    thread::spawn(move || store.save(&saved_key, &saved));
                    cache.lock().unwrap().insert(key, MemoryList { read_at: Instant::now(), snapshot, tracks: tracks.clone() });
                    hearts_job(api, likes, ui_weak, detail_seq, tracks, seq);
                }
                Err(e) if shown.is_some() => eprintln!("playlist not refreshed: {e}"),
                Err(e) => ui.set_detail_state(failed(&e)),
            }
        });
    }

    fn load_hearts(&self, tracks: Vec<TrackSummary>, seq: u64) {
        hearts_job(Arc::clone(&self.api), self.likes.clone(), self.ui.clone(), Arc::clone(&self.detail_seq), tracks, seq);
    }

    // ---- imported playlists ----

    pub fn import_playlist(&self, source: i32, link: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let link = link.trim().to_string();
        if link.is_empty() {
            crate::toast(&ui, t("Incolla prima il link della playlist.", "Paste the playlist link first."));
            return;
        }
        ui.set_import_busy(true);
        let (local, ui_weak, api, spotify) = (self.likes.local.clone(), self.ui.clone(), Arc::clone(&self.api), self.spotify());
        thread::spawn(move || {
            let result = import_from(source, &link, &api, spotify);
            if let Ok(playlist) = &result {
                local.add(playlist.clone());
            }
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                ui.set_import_busy(false);
                match result {
                    Ok(p) => {
                        ui.set_import_open(false);
                        ui.set_import_url("".into());
                        crate::toast(&ui, &format!("{} «{}»: {}", t("Importata", "Imported"), p.name, imported_count(&p)));
                        with(|lib| lib.refresh_home());
                    }
                    Err(e) => {
                        eprintln!("import failed: {e}");
                        crate::toast_error(&ui, &import_message(&e));
                    }
                }
            });
        });
    }

    /// Fetches each song's own cover for a playlist imported before covers were read, then
    /// shows them if the playlist is still open.
    fn fill_covers(&self, mut playlist: ImportedPlaylist) {
        // When the endpoint is out of reach, opening the playlist again must not ask again.
        if !self.covers_asked.borrow_mut().insert(playlist.id.clone()) {
            return;
        }
        let (local, ui_weak) = (self.likes.local.clone(), self.ui.clone());
        thread::spawn(move || {
            spotify_embed::add_covers(&mut playlist.tracks);
            local.add(playlist.clone());
            let _ = ui_weak.upgrade_in_event_loop(move |_| {
                with(|lib| {
                    let open = lib.detail_item.borrow().as_ref().is_some_and(|item| item.uri == playlist.uri().as_str());
                    if open {
                        lib.reload_detail();
                    }
                    lib.refresh_home();
                });
            });
        });
    }

    pub fn remove_local_playlist(&self) {
        let Some(item) = self.detail_item.borrow().clone() else { return };
        let Some(playlist) = self.likes.local.playlist(&item.uri) else { return };
        self.likes.local.remove(&item.uri);
        if let Some(ui) = self.ui.upgrade() {
            let local = self.likes.local.clone();
            crate::toast_with_undo(&ui, &format!("{} «{}»", t("Rimossa", "Removed"), item.title), move || {
                local.add(playlist);
                with(|lib| lib.refresh_home());
            });
            nav::go(&ui, 2);
        }
        self.refresh_home();
    }

    // ---- playback ----

    /// Starts a song; the play button shows it is on its way until sound comes.
    fn start(&self, request: PlayRequest) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_track_loading(true);
        }
        self.engine.send(Command::Play(request));
    }

    fn engine_ready(&self) -> bool {
        let ready = self.engine.ready();
        if let (false, Some(ui)) = (ready, self.ui.upgrade()) {
            crate::toast(&ui, t(STARTING.0, STARTING.1));
        }
        ready
    }

    /// Plays the tapped track. In a playlist or album the whole context plays from there
    /// (so shuffle and "next" cover all of it); elsewhere the following tracks of the list.
    pub fn play_track(&self, source: i32, index: i32) {
        if !self.engine_ready() {
            return;
        }
        let m = models::get();
        let Some(model) = m.track_source(source) else { return };
        let index = index.max(0) as usize;
        if model.row_data(index).is_none() {
            return;
        }
        self.resume_pending.set(false);
        if source == 2 {
            if let Some(context) = self.detail_context.borrow().clone() {
                memory::set_source(Some(context.clone()), Vec::new());
                let tracks = model.iter().map(|row| models::row_track(&row)).collect();
                self.start(PlayRequest { tracks, start: Some(index), context: Some(context), position_ms: 0 });
                return;
            }
        }
        let tracks: Vec<Track> = (index..model.row_count())
            .take(PLAY_BATCH)
            .filter_map(|i| model.row_data(i))
            .filter(|row| !row.uri.is_empty())
            .map(|row| models::row_track(&row))
            .collect();
        if !tracks.is_empty() {
            memory::set_source(None, tracks.iter().map(|t| t.uri.clone()).collect());
            self.start(PlayRequest { tracks, start: Some(0), ..Default::default() });
        }
    }

    pub fn play_detail(&self) {
        match self.detail_context.borrow().clone() {
            Some(context) => {
                if self.engine_ready() {
                    self.resume_pending.set(false);
                    memory::set_source(Some(context.clone()), Vec::new());
                    let tracks = models::get().detail_tracks.iter().map(|row| models::row_track(&row)).collect();
                    self.start(PlayRequest { tracks, start: None, context: Some(context), position_ms: 0 });
                }
            }
            None => self.play_track(2, 0),
        }
    }

    /// Marks the song shown at startup as waiting to be resumed.
    pub fn restore_session(&self) {
        let Some((track, ..)) = memory::resume_point() else { return };
        self.resume_pending.set(true);
        *self.now_track.borrow_mut() = Some(memory::track_of(track));
    }

    /// Starts the restored song from where it stopped. False if there is nothing to resume.
    pub fn resume(&self) -> bool {
        if !self.resume_pending.get() {
            return false;
        }
        if !self.engine_ready() {
            return true;
        }
        self.resume_pending.set(false);
        let Some((saved, position, context, uris)) = memory::resume_point() else { return false };
        self.restore_queue.set(true);
        let track = memory::track_of(saved);
        self.record_played(&track);
        self.start(match context {
            Some(context) => {
                // The saved list gives engines that keep their own queue the whole context.
                let mut tracks: Vec<Track> = self.saved_list(&context).map(|l| l.tracks.into_iter().map(track_of).collect()).unwrap_or_default();
                let start = tracks.iter().position(|t| t.uri == track.uri).unwrap_or_else(|| {
                    tracks = vec![track.clone()];
                    0
                });
                PlayRequest { tracks, start: Some(start), context: Some(context), position_ms: position }
            }
            None => {
                let from = uris.iter().position(|u| *u == track.uri).unwrap_or(0);
                // Only the song itself is known by name: the rest are enough for Spotify.
                let rest = uris.iter().skip(from + 1).map(|uri| Track { uri: uri.clone(), ..Default::default() });
                let tracks = std::iter::once(track.clone()).chain(rest).collect();
                PlayRequest { tracks, start: Some(0), context: None, position_ms: position }
            }
        });
        true
    }

    /// Runs once the player reports a song: finishes a resume.
    pub fn on_state_changed(&self) {
        if self.restore_queue.take() {
            for track in memory::queue() {
                self.engine.send(Command::Queue(memory::track_of(track)));
            }
        }
    }

    /// The play button and the space bar: with nothing loaded yet, start the first quick pick.
    pub fn play_pause(&self) {
        if self.resume() {
            return;
        }
        if !models::now_uri().is_empty() {
            self.engine.send(Command::Toggle);
        } else if models::get().quick_picks.row_count() > 0 {
            self.play_track(0, 0);
        } else if let Some(ui) = self.ui.upgrade() {
            crate::toast(&ui, t("Scegli un brano da ascoltare.", "Pick a song to play."));
        }
    }

    pub fn seek_ms(&self, ms: i32) {
        if ms >= 0 {
            self.engine.send(Command::SeekMs(ms as u32));
        }
    }

    // ---- likes ----

    pub fn like_item(&self, source: i32, index: i32) {
        let m = models::get();
        let Some(row) = m.track_source(source).and_then(|model| model.row_data(index as usize)) else { return };
        self.set_liked(summary_of(models::row_track(&row)), !row.liked);
    }

    pub fn like_current(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let track = self.now_track.borrow().clone();
        if let Some(track) = track.filter(|t| !t.uri.is_empty()) {
            self.set_liked(summary_of(track), !ui.get_now_liked());
        }
    }

    fn set_liked(&self, track: TrackSummary, liked: bool) {
        let uri = track.uri.clone();
        if let Some(ui) = self.ui.upgrade() {
            models::set_liked_everywhere(&ui, &uri, liked);
        }
        let likes = self.likes.clone();
        self.detail_cache.lock().unwrap().remove(LIKED_KEY);
        self.api.store().remove(LIKED_KEY);
        self.run(
            move |api| likes.set(api, &track, liked),
            move |ui, result| {
                if let Err(e) = result {
                    // Roll the heart back and say why.
                    models::set_liked_everywhere(ui, &uri, !liked);
                    crate::toast(ui, &format!("{} {}", t("Preferito non aggiornato.", "Favorite not updated."), explain(&e).1));
                }
            },
        );
    }

    // ---- queue ----

    pub fn enqueue_item(&self, source: i32, index: i32) {
        let m = models::get();
        let Some(row) = m.track_source(source).and_then(|model| model.row_data(index as usize)) else { return };
        let track = models::row_track(&row);
        let title = track.name.clone();
        let saved = memory::saved_track(&track, track.duration_ms);
        let added = format!("{}: {title}", t("Aggiunto alla coda", "Added to queue"));
        if self.resume_pending.get() {
            // Nothing is playing yet: it joins the queue when the song resumes.
            memory::queue_add(saved);
            if let Some(ui) = self.ui.upgrade() {
                crate::toast(&ui, &added);
            }
            self.queue_toggled();
            return;
        }
        if !self.engine_ready() {
            return;
        }
        if models::now_uri().is_empty() {
            // Spotify only queues behind something that is playing: start this one instead.
            self.start(PlayRequest { tracks: vec![track], start: Some(0), ..Default::default() });
            if let Some(ui) = self.ui.upgrade() {
                crate::toast(&ui, &format!("{} «{title}»", t("Avvio", "Playing")));
            }
            return;
        }
        memory::queue_add(saved);
        self.engine.send(Command::Queue(track));
        if let Some(ui) = self.ui.upgrade() {
            crate::toast(&ui, &added);
        }
        if self.engine.local_queue().is_some() {
            self.queue_toggled();
        } else {
            let (api, likes, ui_weak) = (Arc::clone(&self.api), self.likes.clone(), self.ui.clone());
            Timer::single_shot(QUEUE_SETTLE, move || {
                if ui_weak.upgrade().is_some_and(|ui| ui.get_queue_open()) {
                    refresh_queue(api, likes, ui_weak);
                }
            });
        }
    }

    pub fn queue_toggled(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !ui.get_queue_open() {
            return;
        }
        if self.resume_pending.get() {
            // Spotify has no player yet: show what will resume.
            let tracks: Vec<TrackSummary> = memory::resume_point()
                .map(|(track, ..)| track)
                .into_iter()
                .chain(memory::queue())
                .map(|s| summary_of(memory::track_of(s)))
                .collect();
            models::get().queue.set_vec(track_items(&tracks, &[], &self.ui));
            ui.set_queue_state(content());
            return;
        }
        if let Some(tracks) = self.engine.local_queue() {
            let tracks: Vec<TrackSummary> = tracks.into_iter().map(summary_of).collect();
            models::get().queue.set_vec(track_items(&tracks, &[], &self.ui));
            ui.set_queue_state(content());
            return;
        }
        if !self.spotify() {
            models::get().queue.set_vec(Vec::new());
            ui.set_queue_state(empty(t("La coda è vuota", "The queue is empty"), t("Avvia un brano per vedere cosa viene dopo.", "Start a song to see what comes next.")));
            return;
        }
        refresh_queue(Arc::clone(&self.api), self.likes.clone(), self.ui.clone());
    }

    // ---- lyrics ----

    fn load_lyrics(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(track) = self.now_track.borrow().clone() else {
            ui.set_lyrics(ModelRc::default());
            ui.set_lyrics_state(lyrics_idle());
            return;
        };
        if *self.lyrics_uri.borrow() == track.uri {
            return; // already loaded or loading
        }
        *self.lyrics_uri.borrow_mut() = track.uri.clone();
        LYRIC_TIMES.with(|t| t.borrow_mut().clear());
        ui.set_lyrics(ModelRc::default());
        ui.set_lyrics_current(-1);
        ui.set_lyrics_state(loading());

        let artist = track.artists.first().cloned().unwrap_or_default();
        let duration = ui.get_duration_ms() as u32;
        let uri = track.uri.clone();
        let ui_weak = self.ui.clone();
        thread::spawn(move || {
            let result = lyrics::fetch(&track.uri, &track.name, &artist, &track.album, duration);
            let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                if models::now_uri() != uri {
                    return; // the song changed while we were fetching
                }
                match result {
                    Ok(Lyrics::Lines(lines)) => {
                        let synced = lines.iter().all(|l| l.time_ms.is_some());
                        LYRIC_TIMES.with(|t| *t.borrow_mut() = if synced { lines.iter().filter_map(|l| l.time_ms).collect() } else { Vec::new() });
                        let rows: Vec<LyricLine> = lines
                            .iter()
                            .map(|l| LyricLine { text: l.text.as_str().into(), time_ms: l.time_ms.map_or(-1, |t| t as i32) })
                            .collect();
                        ui.set_lyrics(ModelRc::from(Rc::new(VecModel::from(rows))));
                        ui.set_lyrics_state(content());
                    }
                    Ok(Lyrics::Instrumental) => ui.set_lyrics_state(empty(t("Brano strumentale", "Instrumental"), t("Qui non c'è nulla da cantare.", "Nothing to sing here."))),
                    Ok(Lyrics::NotFound) => ui.set_lyrics_state(empty(t("Testo non trovato", "Lyrics not found"), t("Non abbiamo il testo di questo brano.", "We don't have the lyrics for this song."))),
                    Err(e) => {
                        eprintln!("lyrics failed: {e}");
                        let text = if e == lyrics::BUSY {
                            t("Il sito dei testi chiede una pausa: riprova tra un minuto.", "The lyrics site asks for a pause: try again in a minute.")
                        } else if is_dns_problem(&e) {
                            t("Non raggiungo il sito dei testi: se usi una VPN o un filtro DNS, prova a disattivarlo.", "Can't reach the lyrics site: if you use a VPN or a DNS filter, try turning it off.")
                        } else {
                            t("Controlla la rete e riprova.", "Check your network and try again.")
                        };
                        ui.set_lyrics_state(page_state(3, t("Testo non disponibile", "Lyrics unavailable"), text, retry()));
                    }
                }
            });
        });
    }

    /// Called when the playing track changes.
    pub fn on_track_changed(&self, track: &Track) {
        let Some(ui) = self.ui.upgrade() else { return };
        *self.now_track.borrow_mut() = Some(track.clone());
        self.record_played(track);
        models::set_now(&track.uri, &track.image_url);
        ui.set_now_uri(track.uri.as_str().into());
        images::request(&track.image_url, &self.ui);
        ui.set_now_has_art(false);
        ui.set_now_liked(false);
        models::refresh_images(&ui);

        let uri = track.uri.clone();
        let (likes, summary) = (self.likes.clone(), summary_of(track.clone()));
        self.run(
            move |api| likes.flags(api, &[summary]).first().copied().unwrap_or(false),
            move |ui, liked| {
                if models::now_uri() == uri {
                    ui.set_now_liked(liked);
                }
            },
        );
        self.queue_toggled();
        if ui.get_page() == LYRICS_PAGE {
            self.load_lyrics();
        }
    }

    /// Rebuilds every text made in Rust after a language change.
    pub fn relocalize(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let search = ui.get_search_text();
        if search.trim().chars().count() >= 2 {
            run_search(Arc::clone(&self.api), self.likes.clone(), self.ui.clone(), Arc::clone(&self.search_seq), search.to_string());
        } else {
            ui.set_search_state(search_start());
        }
        self.lyrics_uri.borrow_mut().clear();
        self.load_lyrics();
        self.load_history();
        self.queue_toggled();
        self.reload_detail();
        self.refresh_home();
    }

    /// Called when playback ends with nothing loaded.
    pub fn on_track_cleared(&self) {
        *self.now_track.borrow_mut() = None;
        self.lyrics_uri.borrow_mut().clear();
        models::set_now("", "");
        if let Some(ui) = self.ui.upgrade() {
            ui.set_now_uri("".into());
            ui.set_now_has_art(false);
            ui.set_now_liked(false);
        }
    }
}

/// The account's playlists, then the imported ones.
fn show_playlists(list: &[PlaylistSummary], local: &LocalLibrary, ui: &Weak<MainWindow>) {
    let mut items = vec![models::liked_card()];
    items.extend(list.iter().map(|p| models::playlist_item(p, ui)));
    items.extend(local.playlists().iter().map(|p| {
        let mut item = models::playlist_item(&imported_summary(p), ui);
        item.subtitle = format!("{} · {}", imported_summary(p).owner, imported_count(p)).into();
        item
    }));
    models::get().playlists.set_vec(items);
}

fn run_search(api: Arc<Api>, likes: Likes, ui: Weak<MainWindow>, seq: Arc<AtomicU64>, query: String) {
    let id = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let query = query.trim().to_string();
    let Some(window) = ui.upgrade() else { return };
    let m = models::get();
    if query.chars().count() < 2 {
        for model in [&m.results_tracks, &m.results_albums, &m.results_playlists] {
            model.set_vec(Vec::new());
        }
        window.set_search_state(search_start());
        return;
    }
    window.set_search_state(loading());
    let ui_weak = ui.clone();
    let shown_query = query.clone();
    spawn_job(
        api,
        ui,
        move |api| {
            // Without Spotify, songs are searched on YouTube.
            let results = if likes.spotify.load(Ordering::SeqCst) {
                api.search(&query).map_err(|e| brief(&e))
            } else {
                Ytdlp::from_config(&AppConfig::load_or_create().unwrap_or_default())
                    .search(&query)
                    .map(|tracks| app_core::api::SearchResults { tracks: tracks.into_iter().map(summary_of).collect(), ..Default::default() })
                    .map_err(|e| engine_message(&e).to_string())
            };
            results.map(|r| {
                let flags = likes.flags(api, &r.tracks);
                (r, flags)
            })
        },
        move |ui, result| {
            if seq.load(Ordering::SeqCst) != id {
                return; // a newer search superseded this one
            }
            let m = models::get();
            match result {
                Ok((r, flags)) => {
                    let none = r.tracks.is_empty() && r.albums.is_empty() && r.playlists.is_empty();
                    m.results_tracks.set_vec(track_items(&r.tracks, &flags, &ui_weak));
                    m.results_albums.set_vec(r.albums.iter().map(|a| models::album_item(a, &ui_weak)).collect::<Vec<_>>());
                    m.results_playlists.set_vec(r.playlists.iter().map(|p| models::playlist_item(p, &ui_weak)).collect::<Vec<_>>());
                    ui.set_search_state(if none {
                        empty(
                            &format!("{} «{shown_query}»", t("Nessun risultato per", "No results for")),
                            t("Controlla come è scritto o prova con meno parole.", "Check the spelling or try fewer words."),
                        )
                    } else {
                        content()
                    });
                }
                Err(text) => ui.set_search_state(page_state(3, t("Ricerca non riuscita", "Search failed"), &text, retry())),
            }
        },
    );
}

fn refresh_queue(api: Arc<Api>, likes: Likes, ui: Weak<MainWindow>) {
    if let Some(window) = ui.upgrade() {
        if models::get().queue.row_count() == 0 {
            window.set_queue_state(loading());
        }
    }
    let ui_weak = ui.clone();
    spawn_job(
        api,
        ui,
        move |api| {
            api.queue().map(|q| {
                let mut tracks: Vec<TrackSummary> = q.current.into_iter().collect();
                tracks.extend(q.upcoming);
                let flags = likes.flags(api, &tracks);
                (tracks, flags)
            })
        },
        move |ui, result| match result {
            Ok((tracks, flags)) => {
                models::get().queue.set_vec(track_items(&tracks, &flags, &ui_weak));
                ui.set_queue_state(if tracks.is_empty() {
                    empty(t("La coda è vuota", "The queue is empty"), t("Aggiungi un brano con l'icona accanto al titolo.", "Add a song with the icon next to its title."))
                } else {
                    content()
                });
            }
            Err(e) => {
                let (title, text) = explain(&e);
                ui.set_queue_state(page_state(3, title, &text, ""));
            }
        },
    );
}

/// Connects every library callback of the window to `lib`.
pub fn wire(ui: &MainWindow, lib: &Rc<Library>) {
    let l = Rc::clone(lib);
    ui.on_search_edited(move |t| l.search_edited(&t));
    let l = Rc::clone(lib);
    ui.on_search_submit(move |t| l.search_submit(&t));
    let l = Rc::clone(lib);
    ui.on_open_item(move |s, i| l.open_item(s, i));
    let l = Rc::clone(lib);
    ui.on_play_track(move |s, i| l.play_track(s, i));
    let l = Rc::clone(lib);
    ui.on_like_item(move |s, i| l.like_item(s, i));
    let l = Rc::clone(lib);
    ui.on_enqueue_item(move |s, i| l.enqueue_item(s, i));
    let l = Rc::clone(lib);
    ui.on_play_detail(move || l.play_detail());
    let l = Rc::clone(lib);
    ui.on_play_pause(move || l.play_pause());
    let l = Rc::clone(lib);
    ui.on_like_current(move || l.like_current());
    let l = Rc::clone(lib);
    ui.on_queue_toggled(move || l.queue_toggled());
    let l = Rc::clone(lib);
    ui.on_refresh(move |p| l.refresh(p));
    let l = Rc::clone(lib);
    ui.on_seek_ms(move |ms| l.seek_ms(ms));
    let l = Rc::clone(lib);
    ui.on_import_playlist(move |source, link| l.import_playlist(source, &link));
    let l = Rc::clone(lib);
    ui.on_remove_local_playlist(move || l.remove_local_playlist());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_become_plain_italian() {
        assert_eq!(engine_message("account_error: premium required"), "Per riprodurre musica serve Spotify Premium.");
        assert_eq!(engine_message("engine not ready"), "Il player si sta avviando: riprova tra un attimo.");
        assert_eq!(engine_message("API PUT /me/player/play -> 403 Forbidden"), "Spotify non permette questa operazione.");
        assert_eq!(engine_message("something unknown"), "La riproduzione non è riuscita. Riprova.");
        assert!(engine_message("yt-dlp missing: not found").contains("serve yt-dlp"));
        assert!(engine_message("yt-dlp: ERROR: Sign in to confirm").contains("accesso"));
        assert!(engine_message("yt-dlp: ERROR: Sign in to confirm you're not a bot").contains("bloccato"));
        assert!(engine_message("yt-dlp: ERROR: Sign in to confirm your age").contains("minori"));
        assert!(engine_message("yt-dlp: ERROR: page needs reload").contains("aggiorna"));
    }

    #[test]
    fn dns_failures_are_told_apart() {
        assert!(is_dns_problem("io: Host sconosciuto. (os error 11001)"));
        assert!(is_dns_problem("Failed to resolve 'api.spotify.com' ([Errno 11001] getaddrinfo failed)"));
        assert!(!is_dns_problem("status 404"));
        assert!(engine_message("yt-dlp: ERROR: getaddrinfo failed").contains("VPN"));
    }

    #[test]
    fn import_errors_say_what_to_do() {
        assert!(import_message("not a spotify playlist link").contains("Spotify"));
        assert!(import_message("spotify embed: status 404").contains("pubblica"));
        assert!(import_message("yt-dlp missing: x").contains("yt-dlp"));
    }

    #[test]
    fn imported_playlists_say_where_they_come_from() {
        let p = ImportedPlaylist { id: "youtube-x".into(), name: "Mix".into(), owner: "Me".into(), source: "youtube".into(), ..Default::default() };
        let s = imported_summary(&p);
        assert_eq!(s.uri, "local:playlist:youtube-x");
        assert_eq!(s.owner, "Me");
        assert_eq!(s.track_count, Some(0));
        let mut cut = ImportedPlaylist { total: 270, ..Default::default() };
        cut.tracks = vec![TrackSummary::default(); 100];
        assert_eq!(imported_count(&cut), "100 di 270 brani");
        cut.total = 100;
        assert_eq!(imported_count(&cut), "100 brani");
    }

    #[test]
    fn waits_read_naturally() {
        assert_eq!(wait_text(45), "45 secondi");
        assert_eq!(wait_text(600), "10 minuti");
        assert_eq!(wait_text(7200), "2 ore");
        assert!(switch_message(app_player::EngineKind::YouTube, 600).contains("10 minuti"));
        assert!(switch_message(app_player::EngineKind::Spotify, 0).contains("Spotify"));
    }

    #[test]
    fn local_history_stays_unique_and_bounded() {
        let track = |uri: &str| TrackSummary { uri: uri.to_string(), ..Default::default() };
        let mut history = vec![track("a"), track("b")];
        push_history(&mut history, track("b"));
        assert_eq!(history.iter().map(|t| t.uri.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        for i in 0..HISTORY_SIZE {
            push_history(&mut history, track(&i.to_string()));
        }
        assert_eq!(history.len(), HISTORY_SIZE);
    }

    #[test]
    fn the_limit_note_says_whether_music_stops() {
        let quiet = Usage::default();
        assert_eq!(usage_note(&quiet), "");
        let partial = Usage { blocked_endpoints: 4, blocked_seconds: 59_000, ..Default::default() };
        assert!(usage_note(&partial).contains("la musica non si ferma"));
        let total = Usage { blocked_endpoints: 1, blocked_seconds: 600, playback_blocked: true, ..Default::default() };
        assert!(usage_note(&total).contains("non fa partire"));
    }

    #[test]
    fn recent_searches_stay_unique_and_bounded() {
        let mut recent = vec!["Ghali".to_string(), "Lazza".to_string()];
        push_recent(&mut recent, "ghali");
        assert_eq!(recent, ["ghali", "Lazza"]);
        for q in ["a1", "a2", "a3", "a4", "a5"] {
            push_recent(&mut recent, q);
        }
        assert_eq!(recent.len(), MAX_RECENT_SEARCHES);
        assert_eq!(recent[0], "a5");
    }
}
