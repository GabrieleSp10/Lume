//! Plays a Spotify track from YouTube: yt-dlp finds the matching video and its audio stream,
//! and the hidden page plays that stream, or the downloaded file when there is one. yt-dlp
//! is installed by the app from its official source on the user's request, never bundled.

use std::{
    collections::HashMap,
    os::windows::process::CommandExt,
    process::Command as Process,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use app_core::{
    cache::Store,
    config::{offline_dir, tools_dir, youtube_cookies_file, AppConfig},
    offline, tools,
};
use serde_json::{json, Value};

use crate::{
    local::{Msg, Sink},
    types::{AudioEvent, Track},
};

/// Keeps a console window from flashing up for each yt-dlp call.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const VIDEOS_KEY: &str = "youtube-videos";
const SEARCH_RESULTS: u32 = 6;
/// Stream links stop working after about six hours.
const STREAM_FRESH: Duration = Duration::from_secs(4 * 3600);
/// After YouTube asks to prove this is not a bot, it is left alone this long: asking again
/// right away keeps the block going.
const BLOCK_PAUSE: Duration = Duration::from_secs(15 * 60);
/// Below this a result is more likely another song than a bad upload of the right one.
const MIN_SCORE: f64 = -40.0;
/// Words that mark a different recording, unless the Spotify title has them too.
const OTHER_VERSION: [&str; 13] = ["live", "cover", "remix", "karaoke", "instrumental", "sped up", "slowed", "nightcore", "reverb", "8d", "acoustic", "reaction", "tutorial"];

/// yt-dlp prints one line like this per step: the video, bytes fetched, then the size or
/// its estimate; and one "LUMEDONE <video>" when a file is complete.
const PROGRESS_TEMPLATE: &str = "download:LUME %(info.id)s %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s";
const DONE_TEMPLATE: &str = "after_move:LUMEDONE %(id)s";
/// Between two songs of one run, at random in this range, as a listener would.
const SLEEP_SECONDS: (&str, &str) = ("2", "5");

/// How downloaded songs are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// The best audio YouTube has (Opus, about 130–160 kbps).
    High,
    /// About half the size (Opus, about 70 kbps).
    Saver,
}

impl Quality {
    pub fn from_name(name: &str) -> Self {
        if name == "saver" { Self::Saver } else { Self::High }
    }

    /// AAC stays as the fallback for videos without Opus.
    fn format(self) -> &'static str {
        match self {
            Self::High => "bestaudio[acodec=opus]/bestaudio[ext=m4a]/bestaudio",
            Self::Saver => "bestaudio[acodec=opus][abr<=90]/worstaudio[acodec=opus]/bestaudio[ext=m4a]/bestaudio",
        }
    }
}

/// What a download run tells while it goes.
pub enum Fetch {
    /// How much of this video is done, from 0 to 1.
    Progress(String, f32),
    /// This video's file is complete.
    Done(String),
}

/// A video and how much of it a progress line says is done, from 0 to 1.
fn progress_of(line: &str) -> Option<(String, f32)> {
    let mut fields = line.strip_prefix("LUME ")?.split_whitespace();
    let video = fields.next().filter(|id| is_video_id(id))?.to_string();
    let mut numbers = fields.map(|f| f.parse::<f64>().ok());
    let done = numbers.next()??;
    let (total, estimate) = (numbers.next().flatten(), numbers.next().flatten());
    let total = total.or(estimate).filter(|t| *t > 0.0)?;
    Some((video, (done / total).clamp(0.0, 1.0) as f32))
}

/// Ends yt-dlp and what it started: the standalone yt-dlp runs as two processes, and
/// stopping only the first would leave the download going.
fn kill_tree(pid: u32) {
    let _ = Process::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).creation_flags(CREATE_NO_WINDOW).output();
}

/// What the page is given instead of a link for a downloaded song.
const OFFLINE_PREFIX: &str = "offline:";
const BLOCKED_MESSAGE: &str = "youtube paused: asked to confirm you're not a bot";

/// One yt-dlp at a time, for playing, copying and downloading alike: bursts are what YouTube
/// reads as a bot.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
/// YouTube refused with a bot check: nothing is asked until then.
static BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Which video plays each song, kept forever.
static VIDEOS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn blocked() -> bool {
    BLOCKED_UNTIL.lock().unwrap().is_some_and(|until| Instant::now() < until)
}

/// Asking again right after a bot check keeps the block going: pause instead.
fn note_block<T>(result: &Result<T, String>) {
    if let Err(e) = result {
        if e.contains("not a bot") {
            *BLOCKED_UNTIL.lock().unwrap() = Some(Instant::now() + BLOCK_PAUSE);
        }
    }
}

fn videos(store: &Store) -> &'static Mutex<HashMap<String, String>> {
    VIDEOS.get_or_init(|| Mutex::new(store.load(VIDEOS_KEY).map(|c| c.value).unwrap_or_default()))
}

fn known_video(store: &Store, track: &Track) -> Option<String> {
    let own = track.uri.strip_prefix(YOUTUBE_URI).filter(|id| is_video_id(id)).map(String::from);
    own.or_else(|| videos(store).lock().unwrap().get(&track.uri).cloned())
}

/// The YouTube video that plays `track`: its own for a YouTube song, else the one found
/// before, else a search. Used to play songs and to copy Spotify playlists.
pub fn video_for(tool: &Ytdlp, store: &Store, track: &Track) -> Result<String, String> {
    if let Some(video) = known_video(store, track) {
        return Ok(video);
    }
    let _one_at_a_time = ONE_AT_A_TIME.lock().unwrap();
    // Another call may have found it while this one waited.
    if let Some(video) = known_video(store, track) {
        return Ok(video);
    }
    if blocked() {
        return Err(BLOCKED_MESSAGE.into());
    }
    let result = tool.find_video(track);
    note_block(&result);
    let video = result?;
    let mut all = videos(store).lock().unwrap();
    all.insert(track.uri.clone(), video.clone());
    store.save(VIDEOS_KEY, &*all);
    Ok(video)
}

/// Sends a command to the hidden page.
pub(crate) type PageSend = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct Ytdlp {
    /// Program to run; empty means "yt-dlp" from the PATH.
    pub path: String,
    /// The YouTube login made in the app; used before any browser's.
    pub cookies_file: String,
}

/// Prefix of the URI of a song that lives on YouTube only.
pub const YOUTUBE_URI: &str = "youtube:";
const SEARCH_LIMIT: u32 = 20;

impl Ytdlp {
    /// A path set in the config wins; then the copy the app installed; then the PATH.
    pub fn from_config(config: &AppConfig) -> Self {
        let file = youtube_cookies_file();
        let own = tools::ytdlp_exe();
        let path = match (config.ytdlp_path.is_empty(), own.exists()) {
            (false, _) => config.ytdlp_path.clone(),
            (true, true) => own.to_string_lossy().into_owned(),
            (true, false) => String::new(),
        };
        Self {
            path,
            cookies_file: if file.exists() { file.to_string_lossy().into_owned() } else { String::new() },
        }
    }

    /// Songs on YouTube that match `query`.
    pub fn search(&self, query: &str) -> Result<Vec<Track>, String> {
        let query = format!("ytsearch{SEARCH_LIMIT}:{query}");
        let text = self.run(&["--flat-playlist", "--dump-single-json", "--", &query])?;
        Ok(tracks_of(&text)?.1)
    }

    /// Title, owner and songs of a YouTube playlist.
    pub fn playlist(&self, url: &str) -> Result<(String, String, Vec<Track>), String> {
        let url = url.trim();
        let host = url.strip_prefix("https://").unwrap_or_default().split('/').next().unwrap_or_default();
        if !["www.youtube.com", "youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"].contains(&host) {
            return Err("not a youtube link".into());
        }
        let text = self.run(&["--flat-playlist", "--dump-single-json", "--", url])?;
        let (results, tracks) = tracks_of(&text)?;
        let text_of = |key: &str| results.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        Ok((text_of("title"), text_of("channel"), tracks))
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = self.command(args).output().map_err(|e| format!("yt-dlp missing: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!("yt-dlp: {}", String::from_utf8_lossy(&output.stderr).trim()))
        }
    }

    fn command(&self, args: &[&str]) -> Process {
        let program = if self.path.is_empty() { "yt-dlp" } else { self.path.as_str() };
        let mut process = Process::new(program);
        process.creation_flags(CREATE_NO_WINDOW);
        // The app's Deno comes first, so yt-dlp finds a JavaScript runtime without any setup.
        if tools::deno_exe().exists() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs = vec![tools_dir()];
            dirs.extend(std::env::split_paths(&path));
            if let Ok(joined) = std::env::join_paths(dirs) {
                process.env("PATH", joined);
            }
        }
        // Without certifi it trusts the Windows store, like the rest of the app does.
        process.args(["--no-config", "--no-warnings", "--socket-timeout", "10", "--compat-options", "no-certifi"]);
        if !self.cookies_file.is_empty() {
            process.args(["--cookies", &self.cookies_file]);
        }
        process.args(args);
        process
    }

    /// The installed version, or None if yt-dlp cannot be run.
    pub fn version(&self) -> Option<String> {
        self.run(&["--version"]).ok().map(|v| v.trim().to_string())
    }

    /// Whether yt-dlp can answer YouTube's JavaScript challenges: it needs a JS runtime and
    /// the yt-dlp-ejs scripts, or YouTube offers no audio to a logged-in account.
    pub fn solves_challenges(&self) -> bool {
        // `-v` without a link prints its setup and stops, without asking YouTube anything.
        let report = match self.run(&["-v"]) {
            Ok(out) | Err(out) => out,
        };
        let runtime = report.lines().find(|l| l.contains("JS runtimes:")).is_some_and(|l| !l.trim_end().ends_with("none"));
        runtime && report.contains("yt_dlp_ejs")
    }

    fn find_video(&self, track: &Track) -> Result<String, String> {
        let query = format!("ytsearch{SEARCH_RESULTS}:{} {} audio", track.artists.join(" "), track.name);
        let text = self.run(&["--flat-playlist", "--dump-single-json", "--", &query])?;
        let results: Value = serde_json::from_str(&text).map_err(|e| format!("yt-dlp: unreadable search: {e}"))?;
        let entries = results.get("entries").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        pick_video(entries, track).ok_or_else(|| format!("no youtube match for {}", track.name))
    }

    /// Downloads the audio of several videos in one run of yt-dlp, as YouTube gives it (no
    /// conversion), into the offline folder. `on` hears progress and each finished file.
    /// Setting `cancel` stops the run; what was fetched stays, and a later try goes on from
    /// there. Songs that fail are left out; a bot check stops the run and pauses YouTube.
    pub fn download(&self, videos: &[String], quality: Quality, cancel: &AtomicBool, on: impl Fn(Fetch)) -> Result<(), String> {
        let videos: Vec<&String> = videos.iter().filter(|v| is_video_id(v) && offline::file_for(v).is_none()).collect();
        if videos.is_empty() {
            return Ok(());
        }
        if blocked() {
            return Err(BLOCKED_MESSAGE.into());
        }
        let dir = offline_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("offline: {e}"))?;
        let template = dir.join("%(id)s.%(ext)s").to_string_lossy().into_owned();
        let urls: Vec<String> = videos.iter().map(|v| format!("https://www.youtube.com/watch?v={v}")).collect();
        let mut args = vec![
            "-f", quality.format(), "--no-playlist", "--ignore-errors", "--newline", "--progress",
            "--progress-template", PROGRESS_TEMPLATE, "--print", DONE_TEMPLATE,
            "--sleep-interval", SLEEP_SECONDS.0, "--max-sleep-interval", SLEEP_SECONDS.1,
            "-o", &template, "--",
        ];
        args.extend(urls.iter().map(String::as_str));
        let result = self.run_with_progress(&args, cancel, |line| {
            if let Some(video) = line.strip_prefix("LUMEDONE ").map(str::trim).filter(|v| is_video_id(v)) {
                on(Fetch::Done(video.to_string()));
            } else if let Some((video, done)) = progress_of(line) {
                on(Fetch::Progress(video, done));
            }
        });
        // One song YouTube refuses does not stop the others; a bot check stops everything.
        let result = match result {
            Err(e) if !e.contains("not a bot") && e != "cancelled" => {
                eprintln!("some songs not downloaded: {e}");
                Ok(())
            }
            other => other,
        };
        note_block(&result);
        result
    }

    /// Runs yt-dlp reading its output lines as they come, so it can be stopped midway.
    fn run_with_progress(&self, args: &[&str], cancel: &AtomicBool, on_line: impl Fn(&str)) -> Result<(), String> {
        use std::io::{BufRead, BufReader, Read};
        let mut child = self
            .command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("yt-dlp missing: {e}"))?;
        // Read apart, so a full error pipe never stalls the progress one.
        let mut stderr = child.stderr.take().expect("piped stderr");
        let errors = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        let stdout = child.stdout.take().expect("piped stdout");
        let (pid, finished) = (child.id(), AtomicBool::new(false));
        thread::scope(|scope| {
            // yt-dlp can be silent for seconds (getting ready, or pausing between songs): a
            // stop must not wait for its next line.
            scope.spawn(|| {
                while !finished.load(Ordering::SeqCst) {
                    if cancel.load(Ordering::SeqCst) {
                        kill_tree(pid);
                        break;
                    }
                    thread::sleep(Duration::from_millis(150));
                }
            });
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                on_line(&line);
            }
            finished.store(true, Ordering::SeqCst);
        });
        let status = child.wait().map_err(|e| format!("yt-dlp: {e}"))?;
        let errors = errors.join().unwrap_or_default();
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        if status.success() { Ok(()) } else { Err(format!("yt-dlp: {}", errors.trim())) }
    }

    fn stream_url(&self, video: &str) -> Result<String, String> {
        let url = format!("https://www.youtube.com/watch?v={video}");
        // Direct audio first; then whole videos, and HLS last (the page relays it), since a
        // logged-in account is sometimes offered nothing else.
        let text = self.run(&["-f", "bestaudio[ext=m4a][protocol^=http]/bestaudio[protocol^=http]/best[protocol^=http]/best", "-g", "--", &url])?;
        text.lines().next().filter(|l| l.starts_with("https://")).map(String::from).ok_or_else(|| "yt-dlp: no stream link".to_string())
    }
}

/// The songs in yt-dlp's answer for a search or a playlist.
fn tracks_of(text: &str) -> Result<(Value, Vec<Track>), String> {
    let results: Value = serde_json::from_str(text).map_err(|e| format!("yt-dlp: unreadable answer: {e}"))?;
    let entries = results.get("entries").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let tracks = entries.iter().filter_map(video_track).collect();
    Ok((results, tracks))
}

/// A YouTube video as a song: the channel stands in for the artist.
fn video_track(entry: &Value) -> Option<Track> {
    let id = entry.get("id")?.as_str().filter(|id| is_video_id(id))?;
    let channel = entry.get("channel").or_else(|| entry.get("uploader")).and_then(Value::as_str).unwrap_or_default();
    let artist = channel.trim_end_matches(" - Topic").trim_end_matches("VEVO").trim();
    Some(Track {
        uri: format!("{YOUTUBE_URI}{id}"),
        name: entry.get("title")?.as_str()?.to_string(),
        artists: if artist.is_empty() { Vec::new() } else { vec![artist.to_string()] },
        album: String::new(),
        // The light frame: lists load it as is, big covers ask for the HD one.
        image_url: format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg"),
        duration_ms: entry.get("duration").and_then(Value::as_f64).map_or(0, |s| (s * 1000.0) as u32),
    })
}

/// The video that best matches `track`, if any is close enough.
fn pick_video(entries: &[Value], track: &Track) -> Option<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str().filter(|id| is_video_id(id))?;
            Some((score(entry, track), id))
        })
        .filter(|(score, _)| *score >= MIN_SCORE)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, id)| id.to_string())
}

fn score(entry: &Value, track: &Track) -> f64 {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default().to_lowercase();
    let (title, channel) = (text("title"), text("channel"));
    let name = track.name.to_lowercase();
    // Spotify titles carry extras a video title rarely has ("Song - 2011 Remaster").
    let short_name = name.split(" - ").next().unwrap_or(&name).split(" (").next().unwrap_or(&name);
    let mut score = if title.contains(short_name) { 30.0 } else { -30.0 };
    if channel.ends_with(" - topic") {
        score += 40.0;
    }
    if title.contains("official audio") {
        score += 20.0;
    }
    if track.artists.iter().any(|a| { let a = a.to_lowercase(); channel.contains(&a) || title.contains(&a) }) {
        score += 15.0;
    }
    if OTHER_VERSION.iter().any(|word| title.contains(word) && !name.contains(word)) {
        score -= 100.0;
    }
    let seconds = entry.get("duration").and_then(Value::as_f64);
    if let (Some(seconds), true) = (seconds, track.duration_ms > 0) {
        score -= (seconds - track.duration_ms as f64 / 1000.0).abs().min(60.0);
    }
    score
}

/// What ends up in a URL is checked, since it comes from a web response.
fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Finds what plays a song: the downloaded file, or a stream link (kept a few hours).
pub(crate) struct Resolver {
    tool: Ytdlp,
    store: Store,
    streams: Mutex<HashMap<String, (Instant, String)>>,
}

impl Resolver {
    pub(crate) fn new(tool: Ytdlp, store: Store) -> Self {
        Self { tool, store, streams: Mutex::default() }
    }

    fn blocked(&self) -> bool {
        blocked()
    }

    fn stream_for(&self, track: &Track) -> Result<String, String> {
        // A downloaded song plays from its file, even without a connection.
        if let Some(video) = known_video(&self.store, track).filter(|video| offline::file_for(video).is_some()) {
            return Ok(format!("{OFFLINE_PREFIX}{video}"));
        }
        let fresh = |streams: &HashMap<String, (Instant, String)>| streams.get(&track.uri).filter(|(at, _)| at.elapsed() < STREAM_FRESH).map(|(_, url)| url.clone());
        if let Some(url) = fresh(&self.streams.lock().unwrap()) {
            return Ok(url);
        }
        let video = video_for(&self.tool, &self.store, track)?;
        let _one_at_a_time = ONE_AT_A_TIME.lock().unwrap();
        // Another load may have resolved it while this one waited.
        if let Some(url) = fresh(&self.streams.lock().unwrap()) {
            return Ok(url);
        }
        if blocked() {
            return Err(BLOCKED_MESSAGE.into());
        }
        let result = self.tool.stream_url(&video);
        note_block(&result);
        let url = result?;
        self.streams.lock().unwrap().insert(track.uri.clone(), (Instant::now(), url.clone()));
        Ok(url)
    }
}

pub(crate) struct YouTubeSink {
    page: PageSend,
    inbox: Sender<Msg>,
    resolver: Arc<Resolver>,
    /// The token of the newest load: a slower, older one must not start.
    latest: Arc<AtomicU64>,
}

impl YouTubeSink {
    pub(crate) fn new(page: PageSend, inbox: Sender<Msg>, resolver: Arc<Resolver>) -> Self {
        Self { page, inbox, resolver, latest: Arc::default() }
    }

    fn page(&self, command: Value) {
        (self.page)(command.to_string());
    }
}

impl Sink for YouTubeSink {
    fn load(&mut self, track: &Track, position_ms: u32, token: u64) {
        self.latest.store(token, Ordering::SeqCst);
        (self.page)(json!({ "cmd": "audio_stop" }).to_string());
        let (page, inbox, resolver, latest, track) =
            (Arc::clone(&self.page), self.inbox.clone(), Arc::clone(&self.resolver), Arc::clone(&self.latest), track.clone());
        thread::spawn(move || {
            let result = resolver.stream_for(&track);
            if latest.load(Ordering::SeqCst) != token {
                return;
            }
            match result {
                Ok(url) => page(json!({ "cmd": "audio_load", "url": url, "position_ms": position_ms, "token": token }).to_string()),
                Err(e) => {
                    let _ = inbox.send(Msg::Audio { token, event: AudioEvent::Failed(e) });
                }
            }
        });
    }

    fn prepare(&mut self, track: &Track) {
        if self.resolver.blocked() {
            return;
        }
        let (resolver, track) = (Arc::clone(&self.resolver), track.clone());
        thread::spawn(move || {
            if let Err(e) = resolver.stream_for(&track) {
                eprintln!("next song not prepared: {e}");
            }
        });
    }

    fn pause(&mut self) {
        self.page(json!({ "cmd": "audio_pause" }));
    }

    fn resume(&mut self) {
        self.page(json!({ "cmd": "audio_resume" }));
    }

    fn seek(&mut self, position_ms: u32) {
        self.page(json!({ "cmd": "audio_seek", "position_ms": position_ms }));
    }

    fn volume(&mut self, value: f32) {
        self.page(json!({ "cmd": "audio_volume", "value": value.clamp(0.0, 1.0) }));
    }

    fn stop(&mut self) {
        self.latest.store(0, Ordering::SeqCst);
        self.page(json!({ "cmd": "audio_stop" }));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn song() -> Track {
        Track { name: "Get Lucky".into(), artists: vec!["Daft Punk".into()], duration_ms: 248_000, ..Default::default() }
    }

    fn video(id: &str, title: &str, channel: &str, duration: f64) -> Value {
        json!({ "id": id, "title": title, "channel": channel, "duration": duration })
    }

    #[test]
    fn reads_download_progress() {
        let of = |line| progress_of(line).map(|(v, p)| (v, p));
        assert_eq!(of("LUME fOT0BUpITw8 500 1000 NA"), Some(("fOT0BUpITw8".into(), 0.5)));
        assert_eq!(of("LUME fOT0BUpITw8 250 NA 1000.0"), Some(("fOT0BUpITw8".into(), 0.25)));
        assert_eq!(of("LUME fOT0BUpITw8 10 NA NA"), None);
        assert_eq!(of("LUME --exec 1 2 3"), None);
        assert_eq!(of("[download] Destination: x.m4a"), None);
    }

    #[test]
    fn qualities_ask_for_opus_first() {
        assert!(Quality::from_name("saver").format().contains("abr<=90"));
        assert_eq!(Quality::from_name("anything"), Quality::High);
        assert!(Quality::High.format().starts_with("bestaudio[acodec=opus]"));
    }

    #[test]
    fn prefers_the_original_recording() {
        let entries = [
            video("aaaaaaaaaaa", "Daft Punk - Get Lucky (Live at Coachella)", "Fan", 250.0),
            video("bbbbbbbbbbb", "Get Lucky", "Daft Punk - Topic", 249.0),
            video("ccccccccccc", "Get Lucky (Full Album Version)", "Daft Punk", 369.0),
        ];
        assert_eq!(pick_video(&entries, &song()).as_deref(), Some("bbbbbbbbbbb"));
    }

    #[test]
    fn keeps_a_version_the_title_asks_for() {
        let mut live = song();
        live.name = "Get Lucky - Live".into();
        let entries = [video("aaaaaaaaaaa", "Daft Punk - Get Lucky Live", "Daft Punk", 248.0)];
        assert_eq!(pick_video(&entries, &live).as_deref(), Some("aaaaaaaaaaa"));
    }

    #[test]
    fn refuses_a_different_song() {
        let entries = [video("aaaaaaaaaaa", "Harder Better Faster Stronger", "Random", 224.0)];
        assert_eq!(pick_video(&entries, &song()), None);
    }

    #[test]
    fn refuses_ids_that_are_not_video_ids() {
        let entries = [video("--exec calc", "Get Lucky", "Daft Punk - Topic", 248.0)];
        assert_eq!(pick_video(&entries, &song()), None);
    }

    #[test]
    fn videos_become_songs() {
        let entry = json!({ "id": "fOT0BUpITw8", "title": "Song", "channel": "Artist - Topic", "duration": 235.0 });
        let track = video_track(&entry).unwrap();
        assert_eq!(track.uri, "youtube:fOT0BUpITw8");
        assert_eq!(track.artists, ["Artist"]);
        assert_eq!(track.duration_ms, 235_000);
        assert!(video_track(&json!({ "id": "bad id", "title": "x" })).is_none());
    }

    #[test]
    fn only_youtube_links_are_imported() {
        let tool = Ytdlp { path: "definitely-not-yt-dlp".into(), ..Default::default() };
        assert_eq!(tool.playlist("https://evil.example/list").unwrap_err(), "not a youtube link");
        assert_eq!(tool.playlist("--exec calc").unwrap_err(), "not a youtube link");
    }

    #[test]
    fn stops_only_the_newest_load() {
        let sent: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = Arc::clone(&sent);
        let page: PageSend = Arc::new(move |c| log.lock().unwrap().push(c));
        let (tx, _rx) = std::sync::mpsc::channel();
        let resolver = Resolver::new(Ytdlp::default(), Store::default());
        let mut sink = YouTubeSink::new(page, tx, Arc::new(resolver));
        sink.stop();
        assert_eq!(sink.latest.load(Ordering::SeqCst), 0);
        assert!(sent.lock().unwrap()[0].contains("audio_stop"));
    }
}
