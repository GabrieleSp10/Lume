//! Song lyrics from free public sources: LRCLIB (https://lrclib.net) for time-synced lines,
//! lyrics.ovh as a plain-text fallback. Spotify's own lyrics are not available through any
//! public API. Only the track title, artist, album and duration are sent.

use std::{fs, path::PathBuf, thread, time::Duration};

use serde_json::Value;

use crate::auth::percent_encode;

const BASE: &str = "https://lrclib.net/api";
const USER_AGENT: &str = "Lume/0.1 (open-source desktop player)";
/// Waits before each try of a request.
const RETRY_PAUSES_MS: [u64; 3] = [0, 250, 700];
/// How far a record's length may be from the song's for its timings to be trusted.
const MAX_LENGTH_GAP_S: f64 = 4.0;

#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    /// Start time; `None` for plain (unsynced) lyrics.
    pub time_ms: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lyrics {
    Lines(Vec<LyricLine>),
    Instrumental,
    NotFound,
}

/// Finds the lyrics of a song, best source first: the local cache, then time-synced lines
/// from LRCLIB, then plain text from LRCLIB or lyrics.ovh. `key` identifies the song in the
/// cache. Err means every source failed, so "not found" would be untrue.
pub fn fetch(key: &str, title: &str, artist: &str, album: &str, duration_ms: u32) -> Result<Lyrics, String> {
    let cached = load_cached(key);
    if cached.as_ref().is_some_and(|l| is_synced(l)) {
        return Ok(Lyrics::Lines(cached.unwrap_or_default()));
    }
    if cooling_down() {
        return cached.map(Lyrics::Lines).ok_or_else(|| BUSY.to_string());
    }

    let clean = without_artist(&clean_title(title), artist);
    let agent = crate::http::agent(Duration::from_secs(6));
    // lyrics.ovh runs alongside: when LRCLIB is struggling it should not cost extra waiting.
    let (from_lrclib, from_ovh) = thread::scope(|scope| {
        let ovh = scope.spawn(|| lyrics_ovh(&agent, &clean, artist));
        let lrclib = lrclib(&agent, title, &clean, artist, album, duration_ms);
        (lrclib, ovh.join().unwrap_or_else(|_| Err("lyrics.ovh worker failed".into())))
    });

    let result = match (from_lrclib, from_ovh) {
        (Ok(Lyrics::Lines(lines)), _) if is_synced(&lines) => Ok(Lyrics::Lines(lines)),
        (Ok(Lyrics::Instrumental), _) => Ok(Lyrics::Instrumental),
        (Ok(Lyrics::Lines(lines)), _) => Ok(Lyrics::Lines(lines)),
        (_, Ok(Some(lines))) => Ok(Lyrics::Lines(lines)),
        (Ok(Lyrics::NotFound), Ok(None)) => Ok(Lyrics::NotFound),
        (Err(e), _) | (_, Err(e)) => Err(e),
    };
    match result {
        Ok(Lyrics::Lines(lines)) => {
            save_cached(key, &lines);
            Ok(Lyrics::Lines(lines))
        }
        // Nothing new: an older plain copy still beats an empty page.
        other => cached.map_or(other, |lines| Ok(Lyrics::Lines(lines))),
    }
}

/// What an error says when LRCLIB asked to slow down.
pub const BUSY: &str = "lyrics busy";
/// After a 429 LRCLIB is left alone this long: asking again would only extend the limit.
const COOLDOWN: Duration = Duration::from_secs(60);
static COOLING_UNTIL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn cooling_down() -> bool {
    COOLING_UNTIL.lock().unwrap().is_some_and(|until| std::time::Instant::now() < until)
}

/// YouTube titles often repeat the artist first ("Artist - Song"); lyric databases do not.
fn without_artist(title: &str, artist: &str) -> String {
    let lower = title.to_lowercase();
    let prefix = format!("{} - ", artist.to_lowercase());
    match lower.starts_with(&prefix) && !artist.is_empty() {
        true => title[prefix.len()..].trim().to_string(),
        false => title.to_string(),
    }
}

fn is_synced(lines: &[LyricLine]) -> bool {
    lines.iter().any(|l| l.time_ms.is_some())
}

/// Title without what lyric databases do not index: "(feat. X)", "- 2022 Remaster", "[Live]"…
pub fn clean_title(title: &str) -> String {
    const NOISE: [&str; 16] = [
        "feat", "ft.", "with ", "remaster", "live", "version", "edit", "mix", "rmx", "official", "video", "audio", "visual", "lyric", "testo", "prod",
    ];
    let noisy = |part: &str| {
        let part = part.to_lowercase();
        NOISE.iter().any(|n| part.contains(n))
    };
    let mut out = title.to_string();
    for (open, close) in [('(', ')'), ('[', ']')] {
        while let (Some(a), Some(b)) = (out.find(open), out.find(close)) {
            if b < a || !noisy(&out[a..b]) {
                break;
            }
            out.replace_range(a..=b, "");
        }
    }
    if let Some((head, tail)) = out.rsplit_once(" - ") {
        if noisy(tail) {
            out = head.to_string();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// GET with quick retries: LRCLIB often answers 503 for a moment. Ok(None) means 404.
fn get_json(agent: &ureq::Agent, url: &str) -> Result<Option<Value>, String> {
    let mut failure = String::new();
    for pause_ms in RETRY_PAUSES_MS {
        thread::sleep(Duration::from_millis(pause_ms));
        match agent.get(url).header("User-Agent", USER_AGENT).call() {
            Ok(mut resp) => match resp.status().as_u16() {
                200 => return resp.body_mut().read_json::<Value>().map(Some).map_err(|e| e.to_string()),
                404 => return Ok(None),
                // Retrying at once is what keeps the limit going: stop and wait instead.
                429 => {
                    *COOLING_UNTIL.lock().unwrap() = Some(std::time::Instant::now() + COOLDOWN);
                    return Err(BUSY.to_string());
                }
                s => failure = format!("{url} returned {s}"),
            },
            Err(e) => failure = e.to_string(),
        }
    }
    Err(failure)
}

/// Tries the ways of asking LRCLIB in turn, stopping at the first synced result. Several are
/// needed: one form can keep failing or come back empty while another finds the song.
fn lrclib(agent: &ureq::Agent, title: &str, clean: &str, artist: &str, album: &str, duration_ms: u32) -> Result<Lyrics, String> {
    let (t, c, a) = (percent_encode(title), percent_encode(clean), percent_encode(artist));
    let urls = [
        format!("{BASE}/get?track_name={t}&artist_name={a}&album_name={}&duration={}", percent_encode(album), duration_ms / 1000),
        format!("{BASE}/search?track_name={c}&artist_name={a}"),
        format!("{BASE}/search?q={}", percent_encode(&format!("{artist} {clean}"))),
    ];
    let mut best = Lyrics::NotFound;
    let mut failure = None;
    let mut answered = false;
    for url in &urls {
        match get_json(agent, url) {
            Ok(body) => {
                answered = true;
                let records = match body {
                    Some(Value::Array(list)) => list,
                    Some(record) => vec![record],
                    None => Vec::new(),
                };
                match choose(&records, artist, duration_ms) {
                    Lyrics::Lines(lines) if is_synced(&lines) => return Ok(Lyrics::Lines(lines)),
                    Lyrics::NotFound => {}
                    other => best = other,
                }
            }
            Err(e) if e == BUSY => {
                failure = Some(e);
                break;
            }
            Err(e) => failure = Some(e),
        }
    }
    match (answered, failure) {
        (false, Some(e)) => Err(e),
        _ => Ok(best),
    }
}

/// The best record for this song: right artist, and synced only if its length matches,
/// because timings from another version would drift. Otherwise its words alone.
fn choose(records: &[Value], artist: &str, duration_ms: u32) -> Lyrics {
    let wanted = artist.to_lowercase();
    let same_artist = |v: &&Value| {
        let name = v.get("artistName").and_then(Value::as_str).unwrap_or_default().to_lowercase();
        name.is_empty() || name.contains(&wanted) || wanted.contains(&name)
    };
    let same_length = |v: &&Value| {
        let seconds = v.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
        seconds == 0.0 || duration_ms == 0 || (seconds - duration_ms as f64 / 1000.0).abs() <= MAX_LENGTH_GAP_S
    };
    let matching: Vec<&Value> = records.iter().filter(same_artist).collect();
    let synced = |v: &&&Value| !text_of(v, "syncedLyrics").is_empty();
    if let Some(record) = matching.iter().filter(synced).find(|v| same_length(v)) {
        return from_record(record);
    }
    if matching.iter().any(|v| v.get("instrumental").and_then(Value::as_bool) == Some(true)) {
        return Lyrics::Instrumental;
    }
    if let Some(record) = matching.iter().find(|v| !text_of(v, "plainLyrics").is_empty()) {
        return Lyrics::Lines(plain_lines(text_of(record, "plainLyrics")));
    }
    match matching.iter().find(synced) {
        Some(record) => Lyrics::Lines(parse_synced(text_of(record, "syncedLyrics")).into_iter().map(|l| LyricLine { time_ms: None, ..l }).collect()),
        None => Lyrics::NotFound,
    }
}

fn text_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or_default()
}

fn plain_lines(text: &str) -> Vec<LyricLine> {
    text.lines().map(|l| LyricLine { time_ms: None, text: l.trim().to_string() }).collect()
}

fn from_record(v: &Value) -> Lyrics {
    if v.get("instrumental").and_then(Value::as_bool) == Some(true) {
        return Lyrics::Instrumental;
    }
    let lines = parse_synced(text_of(v, "syncedLyrics"));
    if !lines.is_empty() {
        return Lyrics::Lines(lines);
    }
    match text_of(v, "plainLyrics") {
        "" => Lyrics::NotFound,
        plain => Lyrics::Lines(plain_lines(plain)),
    }
}

/// Plain lyrics from lyrics.ovh. Ok(None) means it does not have the song.
fn lyrics_ovh(agent: &ureq::Agent, title: &str, artist: &str) -> Result<Option<Vec<LyricLine>>, String> {
    let url = format!("https://api.lyrics.ovh/v1/{}/{}", percent_encode(artist), percent_encode(title));
    let Some(body) = get_json(agent, &url)? else { return Ok(None) };
    let text = clean_ovh(text_of(&body, "lyrics"));
    Ok((!text.is_empty()).then(|| plain_lines(&text)))
}

/// Drops the site's French header line and the double blank lines it leaves between verses.
fn clean_ovh(text: &str) -> String {
    let text = text.replace('\r', "");
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.first().is_some_and(|l| l.starts_with("Paroles de la chanson")) {
        lines.remove(0);
    }
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        if !line.trim().is_empty() || out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

fn cache_file(key: &str) -> PathBuf {
    let name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    crate::config::lyrics_dir().join(format!("{name}.lrc"))
}

/// Lines are stored as LRC when synced, plain text otherwise.
fn load_cached(key: &str) -> Option<Vec<LyricLine>> {
    let text = fs::read_to_string(cache_file(key)).ok()?;
    let synced = parse_synced(&text);
    let lines = if synced.is_empty() { plain_lines(&text) } else { synced };
    (!lines.is_empty()).then_some(lines)
}

fn save_cached(key: &str, lines: &[LyricLine]) {
    let text: Vec<String> = lines
        .iter()
        .map(|l| match l.time_ms {
            Some(ms) => format!("[{:02}:{:02}.{:02}]{}", ms / 60_000, ms / 1000 % 60, ms % 1000 / 10, l.text),
            None => l.text.clone(),
        })
        .collect();
    let path = cache_file(key);
    let saved = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::write(&path, text.join("\n")));
    if let Err(e) = saved {
        eprintln!("lyrics not cached: {e}");
    }
}

/// Parses LRC lines like `[01:02.34] text`. Lines without a timestamp are skipped.
pub fn parse_synced(lrc: &str) -> Vec<LyricLine> {
    let mut out: Vec<LyricLine> = lrc
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix('[')?;
            let (stamp, text) = rest.split_once(']')?;
            let (min, sec) = stamp.split_once(':')?;
            let ms = min.parse::<u32>().ok()? * 60_000 + (sec.parse::<f64>().ok()? * 1000.0).round() as u32;
            Some(LyricLine { time_ms: Some(ms), text: text.trim().to_string() })
        })
        .collect();
    out.sort_by_key(|l| l.time_ms);
    out
}

/// Index of the line being sung at `position_ms` (the last one that has started).
pub fn current_line(times: &[u32], position_ms: u32) -> Option<usize> {
    match times.partition_point(|&t| t <= position_ms) {
        0 => None,
        n => Some(n - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc_and_skips_metadata() {
        let lines = parse_synced("[ar: Someone]\n[00:12.50] First\n[01:02.00]Second\n[00:05.00] Intro\nno stamp");
        let times: Vec<_> = lines.iter().map(|l| l.time_ms).collect();
        assert_eq!(times, [Some(5_000), Some(12_500), Some(62_000)]);
        assert_eq!(lines[1].text, "First");
    }

    #[test]
    fn titles_lose_only_the_noise() {
        assert_eq!(clean_title("Cacao (feat. Pyrex)"), "Cacao");
        assert_eq!(clean_title("Never Gonna Give You Up - 2022 Remaster"), "Never Gonna Give You Up");
        assert_eq!(clean_title("Song [Live at Wembley]"), "Song");
        assert_eq!(clean_title("Stand (Up) - Radio Edit"), "Stand (Up)");
        assert_eq!(clean_title("Sturm und drang"), "Sturm und drang");
    }

    #[test]
    fn picks_right_artist_and_length() {
        let other = serde_json::json!({"artistName": "Someone Else", "duration": 200.0, "syncedLyrics": "[00:01.00]wrong"});
        let far = serde_json::json!({"artistName": "Ghali", "duration": 300.0, "syncedLyrics": "[00:01.00]long cut"});
        let good = serde_json::json!({"artistName": "Ghali", "duration": 201.0, "syncedLyrics": "[00:01.00]right"});
        match choose(&[other.clone(), far.clone(), good], "Ghali", 200_000) {
            Lyrics::Lines(l) => assert_eq!(l[0].text, "right"),
            x => panic!("{x:?}"),
        }
        // Only another version: keep the words, drop the timings that would drift.
        match choose(&[far], "Ghali", 200_000) {
            Lyrics::Lines(l) => assert_eq!((l[0].time_ms, l[0].text.as_str()), (None, "long cut")),
            x => panic!("{x:?}"),
        }
        assert_eq!(choose(&[other], "Ghali", 200_000), Lyrics::NotFound);
    }

    #[test]
    fn cleans_lyrics_ovh_text() {
        let raw = "Paroles de la chanson X par Y\r\nFirst\r\n\r\n\r\n\r\nSecond\r\n";
        assert_eq!(clean_ovh(raw), "First\n\nSecond");
    }

    #[test]
    fn finds_current_line() {
        let t = [1_000, 5_000, 9_000];
        assert_eq!(current_line(&t, 500), None);
        assert_eq!(current_line(&t, 1_000), Some(0));
        assert_eq!(current_line(&t, 7_000), Some(1));
        assert_eq!(current_line(&t, 99_000), Some(2));
    }

    #[test]
    fn record_shapes() {
        assert_eq!(from_record(&serde_json::json!({"instrumental": true})), Lyrics::Instrumental);
        let plain = from_record(&serde_json::json!({"plainLyrics": "a\nb", "syncedLyrics": ""}));
        assert!(matches!(plain, Lyrics::Lines(l) if l.len() == 2 && l[0].time_ms.is_none()));
    }

    #[test]
    fn youtube_titles_lose_the_artist_and_the_video_words() {
        let title = clean_title("Kid Yugi - Feeling The Music (Visual)");
        assert_eq!(without_artist(&title, "KID YUGI"), "Feeling The Music");
        assert_eq!(without_artist("Song", "Artist"), "Song");
        assert_eq!(clean_title("Song (Official Video)"), "Song");
        assert_eq!(clean_title("Ilva (Fume scure rmx) (feat. Fido Guido)"), "Ilva");
    }
}
