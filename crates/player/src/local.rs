//! Playback that does not need Spotify's servers to run the queue. Spotify's player keeps its
//! own queue, position and shuffle; YouTube and librespot only play one track at a time, so
//! this module does that work for both. Each of them is just a [`Sink`].

use std::{
    collections::VecDeque,
    sync::{
        mpsc::{Receiver, Sender},
        Arc, Mutex,
    },
    time::Instant,
};

use crate::types::{AudioEvent, Command, EngineKind, Event, PlaybackState, PlayRequest, Track};

/// Pressing "previous" later than this restarts the song instead.
const RESTART_AFTER_MS: u32 = 3000;
/// Unplayable songs skipped in a row before giving up, so a broken engine does not spin.
const MAX_SKIPS: u32 = 3;
/// How many upcoming songs the queue panel shows.
const QUEUE_PANEL: usize = 50;

/// Something that can play one track. Results come back as [`Msg::Audio`] with the token
/// given to `load`, so a late answer about the previous song is recognised and ignored.
pub(crate) trait Sink: Send {
    fn load(&mut self, track: &Track, position_ms: u32, token: u64);
    /// A hint that `track` is likely next, to start it faster.
    fn prepare(&mut self, _track: &Track) {}
    fn pause(&mut self);
    fn resume(&mut self);
    fn seek(&mut self, position_ms: u32);
    fn volume(&mut self, value: f32);
    fn stop(&mut self);
}

/// Hands the queue, from the playing song on, to another engine. True if it took it.
pub(crate) type Rescue = Box<dyn Fn(Vec<Track>) -> bool + Send>;

/// A failure that concerns this song only, not the engine.
fn song_only(reason: &str) -> bool {
    const SONG_PROBLEMS: [&str; 4] = ["no youtube match", "confirm your age", "Video unavailable", "Private video"];
    SONG_PROBLEMS.iter().any(|p| reason.contains(p))
}

/// YouTube refuses this computer for now: every further try only makes it last longer.
fn engine_blocked(reason: &str) -> bool {
    reason.contains("not a bot")
}

pub(crate) enum Msg {
    Command(Command),
    Audio { token: u64, event: AudioEvent },
    Configure { primary: Box<dyn Sink>, fallback: Option<Box<dyn Sink>> },
    /// Another engine took over: go silent.
    Stop,
}

/// The tracks to play, in play order, plus songs added by hand that go first.
#[derive(Default)]
pub(crate) struct Queue {
    tracks: Vec<Track>,
    order: Vec<usize>,
    at: usize,
    manual: VecDeque<Track>,
    current: Option<Track>,
    shuffle: bool,
    /// 0 = off, 1 = context, 2 = track.
    repeat: u8,
}

impl Queue {
    fn load(&mut self, tracks: Vec<Track>, start: usize) -> Option<Track> {
        self.order = (0..tracks.len()).collect();
        self.tracks = tracks;
        self.at = start.min(self.order.len().saturating_sub(1));
        if self.shuffle {
            self.shuffle_rest();
        }
        self.current = self.track_at(self.at);
        self.current.clone()
    }

    fn track_at(&self, at: usize) -> Option<Track> {
        self.order.get(at).and_then(|&i| self.tracks.get(i)).cloned()
    }

    /// Keeps what already played in place and mixes what is left.
    fn shuffle_rest(&mut self) {
        for i in (self.at + 2..self.order.len()).rev() {
            let j = rand::random_range(self.at + 1..=i);
            self.order.swap(i, j);
        }
    }

    fn set_shuffle(&mut self, on: bool) {
        if on == self.shuffle {
            return;
        }
        self.shuffle = on;
        if on {
            self.shuffle_rest();
        } else if let Some(&playing) = self.order.get(self.at) {
            self.order = (0..self.tracks.len()).collect();
            self.at = playing;
        }
    }

    /// The song after this one. `by_user` means the next button, which skips even a
    /// song on repeat.
    fn next(&mut self, by_user: bool) -> Option<Track> {
        if !by_user && self.repeat == 2 {
            return self.current.clone();
        }
        if let Some(track) = self.manual.pop_front() {
            self.current = Some(track);
        } else if self.at + 1 < self.order.len() {
            self.at += 1;
            self.current = self.track_at(self.at);
        } else if self.repeat == 1 && !self.order.is_empty() {
            self.at = 0;
            self.current = self.track_at(0);
        } else {
            return None;
        }
        self.current.clone()
    }

    fn prev(&mut self) -> Option<Track> {
        self.at = self.at.checked_sub(1)?;
        self.current = self.track_at(self.at);
        self.current.clone()
    }

    fn peek_next(&self) -> Option<Track> {
        match self.repeat {
            2 => None,
            _ => self.manual.front().cloned().or_else(|| self.track_at(self.at + 1)),
        }
    }

    fn panel(&self) -> Vec<Track> {
        let rest = (self.at + 1..self.order.len()).filter_map(|at| self.track_at(at));
        self.current.iter().cloned().chain(self.manual.iter().cloned()).chain(rest).take(QUEUE_PANEL).collect()
    }
}

pub(crate) struct Player {
    sink: Box<dyn Sink>,
    fallback: Option<Box<dyn Sink>>,
    rescue: Option<Rescue>,
    /// Engines chosen in Settings, waiting for the next song so this one is not cut off.
    chosen: Option<(Box<dyn Sink>, Option<Box<dyn Sink>>)>,
    queue: Queue,
    events: Sender<Event>,
    /// What the queue panel shows.
    panel: Arc<Mutex<Vec<Track>>>,
    token: u64,
    /// Waiting for the sink to start the current song.
    loading: bool,
    paused: bool,
    position_ms: u32,
    position_at: Instant,
    duration_ms: u32,
    seek_when_started: Option<u32>,
    /// The next song was handed to the sink to get ready.
    prepared: bool,
    skips: u32,
    volume: f32,
}

impl Player {
    pub(crate) fn new(sink: Box<dyn Sink>, fallback: Option<Box<dyn Sink>>, events: Sender<Event>, panel: Arc<Mutex<Vec<Track>>>) -> Self {
        Self {
            sink,
            fallback,
            rescue: None,
            chosen: None,
            queue: Queue::default(),
            events,
            panel,
            token: 0,
            loading: false,
            paused: true,
            position_ms: 0,
            position_at: Instant::now(),
            duration_ms: 0,
            seek_when_started: None,
            prepared: false,
            skips: 0,
            volume: 0.7,
        }
    }

    pub(crate) fn with_rescue(mut self, rescue: Rescue) -> Self {
        self.rescue = Some(rescue);
        self
    }

    pub(crate) fn run(mut self, inbox: Receiver<Msg>) {
        for msg in inbox {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Command(cmd) => self.command(cmd),
            Msg::Audio { token, event } if token == self.token => self.audio(event),
            Msg::Audio { .. } => {}
            Msg::Configure { primary, fallback } => self.chosen = Some((primary, fallback)),
            Msg::Stop => {
                self.token += 1;
                self.loading = false;
                self.paused = true;
                self.sink.stop();
            }
        }
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Play(PlayRequest { tracks, start, position_ms, .. }) => {
                let wanted = start.unwrap_or(0);
                let wanted_uri = tracks.get(wanted).map(|t| t.uri.clone());
                // Tracks known by URI alone (from a saved session) cannot be searched for.
                let tracks: Vec<Track> = tracks.into_iter().filter(|t| !t.name.is_empty()).collect();
                let start = tracks.iter().position(|t| Some(&t.uri) == wanted_uri.as_ref()).unwrap_or(0);
                self.skips = 0;
                if let Some(track) = self.queue.load(tracks, start) {
                    self.start(track);
                    self.seek_when_started = (position_ms > 0).then_some(position_ms);
                }
            }
            Command::Pause => self.set_paused(true),
            Command::Resume => self.set_paused(false),
            Command::Toggle => self.set_paused(!self.paused),
            Command::Next => {
                self.skips = 0;
                self.advance(true);
            }
            Command::Prev => {
                if self.position() > RESTART_AFTER_MS {
                    self.seek(0);
                } else if let Some(track) = self.queue.prev() {
                    self.start(track);
                } else {
                    self.seek(0);
                }
            }
            Command::SeekMs(ms) => self.seek(ms),
            Command::Volume(value) => {
                self.volume = value;
                self.sink.volume(value);
            }
            Command::Shuffle(on) => {
                self.queue.set_shuffle(on);
                self.publish();
            }
            Command::Repeat(mode) => {
                self.queue.repeat = mode;
                self.publish();
            }
            Command::Queue(track) => {
                if self.queue.current.is_none() {
                    if let Some(track) = self.queue.load(vec![track], 0) {
                        self.start(track);
                    }
                } else {
                    self.queue.manual.push_back(track);
                    self.update_panel();
                }
            }
        }
    }

    fn audio(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Playing { position_ms, duration_ms } => {
                self.loading = false;
                self.paused = false;
                self.skips = 0;
                // Only a song that plays makes it worth asking for the next one.
                if !std::mem::replace(&mut self.prepared, true) {
                    if let Some(next) = self.queue.peek_next() {
                        self.sink.prepare(&next);
                    }
                }
                if self.duration_ms == 0 {
                    self.duration_ms = duration_ms;
                }
                self.set_position(position_ms);
                if let Some(ms) = self.seek_when_started.take() {
                    self.sink.seek(ms);
                    self.set_position(ms);
                }
                self.publish();
            }
            AudioEvent::Paused { position_ms } => {
                self.paused = true;
                self.set_position(position_ms);
                self.publish();
            }
            AudioEvent::Ended => self.advance(false),
            AudioEvent::Failed(reason) => self.failed(reason),
        }
    }

    fn failed(&mut self, reason: String) {
        eprintln!("local playback failed: {reason}");
        if !song_only(&reason) {
            if let Some(fallback) = self.fallback.take() {
                // The chosen engine does not work here: the backup takes over for good.
                self.sink.stop();
                self.sink = fallback;
                self.sink.volume(self.volume);
                let _ = self.events.send(Event::Switched { to: EngineKind::YouTube, blocked_seconds: 0 });
                if let Some(track) = self.queue.current.clone() {
                    self.start(track);
                }
                return;
            }
            if self.hand_over() {
                return;
            }
        }
        if engine_blocked(&reason) {
            self.give_up(reason);
            return;
        }
        self.skips += 1;
        match self.queue.next(false).filter(|_| self.skips < MAX_SKIPS) {
            Some(track) => self.start(track),
            None => self.give_up(reason),
        }
    }

    /// Gives the queue to another engine, which starts the song from the beginning.
    fn hand_over(&mut self) -> bool {
        let Some(rescue) = &self.rescue else { return false };
        let rest = (self.queue.at + 1..self.queue.order.len()).filter_map(|at| self.queue.track_at(at));
        let tracks: Vec<Track> = self.queue.current.iter().cloned().chain(self.queue.manual.iter().cloned()).chain(rest).collect();
        if tracks.is_empty() || !rescue(tracks) {
            return false;
        }
        self.token += 1;
        self.loading = false;
        self.paused = true;
        self.sink.stop();
        true
    }

    fn give_up(&mut self, reason: String) {
        self.loading = false;
        self.paused = true;
        self.token += 1;
        self.publish();
        let _ = self.events.send(Event::Error(reason));
    }

    fn advance(&mut self, by_user: bool) {
        match self.queue.next(by_user) {
            Some(track) => self.start(track),
            None => {
                self.paused = true;
                self.loading = false;
                self.set_position(self.duration_ms);
                self.publish();
            }
        }
    }

    fn start(&mut self, track: Track) {
        if let Some((primary, fallback)) = self.chosen.take() {
            self.sink.stop();
            self.sink = primary;
            self.fallback = fallback;
            self.sink.volume(self.volume);
        }
        self.token += 1;
        self.loading = true;
        self.paused = true;
        self.seek_when_started = None;
        self.duration_ms = track.duration_ms;
        self.set_position(0);
        self.publish();
        self.sink.load(&track, 0, self.token);
        self.prepared = false;
    }

    fn set_paused(&mut self, paused: bool) {
        if self.queue.current.is_none() || self.loading || self.paused == paused {
            return;
        }
        if paused {
            self.set_position(self.position());
            self.sink.pause();
        } else {
            self.sink.resume();
        }
        self.paused = paused;
        self.publish();
    }

    fn seek(&mut self, ms: u32) {
        if self.loading {
            self.seek_when_started = Some(ms);
        } else {
            self.sink.seek(ms);
        }
        self.set_position(ms);
        self.publish();
    }

    fn set_position(&mut self, ms: u32) {
        self.position_ms = ms;
        self.position_at = Instant::now();
    }

    fn position(&self) -> u32 {
        let moved = if self.paused { 0 } else { self.position_at.elapsed().as_millis() as u32 };
        self.position_ms + moved
    }

    fn update_panel(&self) {
        *self.panel.lock().unwrap() = self.queue.panel();
    }

    fn publish(&self) {
        self.update_panel();
        let _ = self.events.send(Event::State(PlaybackState {
            paused: self.paused,
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            shuffle: self.queue.shuffle,
            repeat_mode: self.queue.repeat,
            track: self.queue.current.clone(),
            context_uri: None,
            loading: self.loading,
            updated_at: self.position_at,
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn track(n: &str) -> Track {
        Track { uri: format!("spotify:track:{n}"), name: n.to_string(), duration_ms: 1000, ..Default::default() }
    }

    fn names(q: &Queue) -> Vec<String> {
        q.panel().into_iter().map(|t| t.name).collect()
    }

    #[test]
    fn queue_plays_hand_added_songs_first_and_stops_at_the_end() {
        let mut q = Queue::default();
        q.load(vec![track("a"), track("b")], 0);
        q.manual.push_back(track("x"));
        assert_eq!(names(&q), ["a", "x", "b"]);
        assert_eq!(q.next(false).unwrap().name, "x");
        assert_eq!(q.next(false).unwrap().name, "b");
        assert!(q.next(false).is_none());
    }

    #[test]
    fn repeat_modes() {
        let mut q = Queue::default();
        q.load(vec![track("a"), track("b")], 1);
        q.repeat = 1;
        assert_eq!(q.next(false).unwrap().name, "a");
        q.repeat = 2;
        assert_eq!(q.next(false).unwrap().name, "a");
        assert_eq!(q.next(true).unwrap().name, "b");
    }

    #[test]
    fn shuffle_keeps_the_playing_song_and_every_track() {
        let mut q = Queue::default();
        q.load((0..30).map(|i| track(&i.to_string())).collect(), 5);
        q.set_shuffle(true);
        assert_eq!(q.current.as_ref().unwrap().name, "5");
        assert_eq!(q.order[..6], [0, 1, 2, 3, 4, 5]);
        let mut sorted = q.order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..30).collect::<Vec<_>>());
        q.set_shuffle(false);
        assert_eq!(q.next(false).unwrap().name, "6");
    }

    #[test]
    fn prev_stops_at_the_first_song() {
        let mut q = Queue::default();
        q.load(vec![track("a"), track("b")], 1);
        assert_eq!(q.prev().unwrap().name, "a");
        assert!(q.prev().is_none());
    }

    /// Records calls, in a form the test can read.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    struct Fake(&'static str, Log);

    impl Sink for Fake {
        fn load(&mut self, track: &Track, _: u32, token: u64) {
            self.1 .0.lock().unwrap().push(format!("{} load {} #{token}", self.0, track.name));
        }
        fn prepare(&mut self, track: &Track) {
            self.1 .0.lock().unwrap().push(format!("{} prepare {}", self.0, track.name));
        }
        fn pause(&mut self) {
            self.1 .0.lock().unwrap().push(format!("{} pause", self.0));
        }
        fn resume(&mut self) {
            self.1 .0.lock().unwrap().push(format!("{} resume", self.0));
        }
        fn seek(&mut self, ms: u32) {
            self.1 .0.lock().unwrap().push(format!("{} seek {ms}", self.0));
        }
        fn volume(&mut self, _: f32) {}
        fn stop(&mut self) {
            self.1 .0.lock().unwrap().push(format!("{} stop", self.0));
        }
    }

    fn player(fallback: bool) -> (Player, Log, Receiver<Event>) {
        let (log, (tx, rx)) = (Log::default(), mpsc::channel());
        let backup: Option<Box<dyn Sink>> = fallback.then(|| Box::new(Fake("backup", log.clone())) as Box<dyn Sink>);
        (Player::new(Box::new(Fake("main", log.clone())), backup, tx, Arc::default()), log, rx)
    }

    fn play(p: &mut Player, names: &[&str]) {
        let tracks = names.iter().map(|n| track(n)).collect();
        p.handle(Msg::Command(Command::Play(PlayRequest { tracks, start: Some(0), ..Default::default() })));
    }

    fn audio(p: &mut Player, event: AudioEvent) {
        p.handle(Msg::Audio { token: p.token, event });
    }

    #[test]
    fn a_song_that_ends_starts_the_next() {
        let (mut p, log, _rx) = player(false);
        play(&mut p, &["a", "b"]);
        assert_eq!(log.take(), ["main load a #1"]);
        audio(&mut p, AudioEvent::Playing { position_ms: 0, duration_ms: 1000 });
        assert_eq!(log.take(), ["main prepare b"]);
        audio(&mut p, AudioEvent::Ended);
        assert_eq!(log.take(), ["main load b #2"]);
    }

    #[test]
    fn answers_about_the_previous_song_are_ignored() {
        let (mut p, log, _rx) = player(false);
        play(&mut p, &["a", "b"]);
        let old = p.token;
        p.handle(Msg::Command(Command::Next));
        log.take();
        p.handle(Msg::Audio { token: old, event: AudioEvent::Ended });
        assert!(log.take().is_empty());
        assert_eq!(p.queue.current.as_ref().unwrap().name, "b");
    }

    #[test]
    fn a_seek_while_loading_waits_for_the_start() {
        let (mut p, log, _rx) = player(false);
        play(&mut p, &["a"]);
        log.take();
        p.handle(Msg::Command(Command::SeekMs(500)));
        assert!(log.take().is_empty());
        audio(&mut p, AudioEvent::Playing { position_ms: 0, duration_ms: 1000 });
        assert_eq!(log.take(), ["main seek 500"]);
    }

    #[test]
    fn a_resumed_song_continues_where_it_stopped() {
        let (mut p, log, _rx) = player(false);
        let request = PlayRequest { tracks: vec![track("a")], start: Some(0), position_ms: 60_000, ..Default::default() };
        p.handle(Msg::Command(Command::Play(request)));
        log.take();
        audio(&mut p, AudioEvent::Playing { position_ms: 0, duration_ms: 1000 });
        assert_eq!(log.take(), ["main seek 60000"]);
    }

    #[test]
    fn a_failing_engine_hands_over_to_the_backup() {
        let (mut p, log, rx) = player(true);
        play(&mut p, &["a"]);
        log.take();
        audio(&mut p, AudioEvent::Failed("audio key refused".into()));
        assert_eq!(log.take(), ["main stop", "backup load a #2"]);
        assert!(rx.try_iter().any(|e| matches!(e, Event::Switched { to: EngineKind::YouTube, .. })));
    }

    #[test]
    fn a_broken_engine_hands_the_queue_over_from_the_playing_song() {
        let (p, log, _rx) = player(false);
        let taken: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&taken);
        let mut p = p.with_rescue(Box::new(move |tracks| {
            *seen.lock().unwrap() = tracks.into_iter().map(|t| t.name).collect();
            true
        }));
        play(&mut p, &["a", "b", "c"]);
        p.handle(Msg::Command(Command::Next));
        log.take();
        audio(&mut p, AudioEvent::Failed("yt-dlp missing: not found".into()));
        assert_eq!(*taken.lock().unwrap(), ["b", "c"]);
        assert_eq!(log.take(), ["main stop"]);
        assert!(p.paused);
    }

    #[test]
    fn a_youtube_block_stops_instead_of_skipping() {
        let (mut p, log, rx) = player(false);
        play(&mut p, &["a", "b", "c"]);
        log.take();
        audio(&mut p, AudioEvent::Failed("yt-dlp: ERROR: Sign in to confirm you're not a bot".into()));
        assert!(log.take().iter().all(|l| !l.contains("load")));
        assert!(rx.try_iter().any(|e| matches!(e, Event::Error(m) if m.contains("not a bot"))));
    }

    #[test]
    fn an_age_restricted_video_is_skipped() {
        let (mut p, log, _rx) = player(false);
        play(&mut p, &["a", "b"]);
        log.take();
        audio(&mut p, AudioEvent::Failed("yt-dlp: ERROR: Sign in to confirm your age".into()));
        assert_eq!(log.take(), ["main load b #2"]);
    }

    #[test]
    fn a_song_missing_on_youtube_is_skipped_not_handed_over() {
        let (p, log, _rx) = player(false);
        let mut p = p.with_rescue(Box::new(|_| panic!("must not hand over")));
        play(&mut p, &["a", "b"]);
        log.take();
        audio(&mut p, AudioEvent::Failed("no youtube match for a".into()));
        assert_eq!(log.take(), ["main load b #2"]);
    }

    #[test]
    fn failures_skip_a_few_songs_then_stop_and_tell() {
        let (mut p, log, rx) = player(false);
        play(&mut p, &["a", "b", "c", "d", "e"]);
        for _ in 0..MAX_SKIPS {
            audio(&mut p, AudioEvent::Failed("no youtube match".into()));
        }
        let loads = log.take().iter().filter(|l| l.contains("load")).count();
        assert_eq!(loads, MAX_SKIPS as usize);
        assert!(rx.try_iter().any(|e| matches!(e, Event::Error(m) if m == "no youtube match")));
        assert!(p.paused);
    }

    #[test]
    fn a_new_engine_waits_for_the_next_song() {
        let (mut p, log, _rx) = player(false);
        play(&mut p, &["a", "b"]);
        log.take();
        p.handle(Msg::Configure { primary: Box::new(Fake("new", log.clone())), fallback: None });
        assert!(log.take().is_empty());
        p.handle(Msg::Command(Command::Next));
        assert_eq!(log.take(), ["main stop", "new load b #2"]);
    }

    #[test]
    fn tracks_without_a_name_are_left_out_and_the_start_is_kept() {
        let (mut p, log, _rx) = player(false);
        let unnamed = Track { uri: "spotify:track:z".into(), ..Default::default() };
        let tracks = vec![unnamed, track("a"), track("b")];
        p.handle(Msg::Command(Command::Play(PlayRequest { tracks, start: Some(2), ..Default::default() })));
        assert_eq!(log.take()[0], "main load b #1");
    }
}
