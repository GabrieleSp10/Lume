//! Reads a public Spotify playlist or album from its embed page, which needs no login and
//! spends no Web API quota. The page lists at most 100 songs, with names and lengths but
//! one cover for all; the public page tells how many there really are.

use std::time::Duration;

use serde_json::Value;

use crate::{api::TrackSummary, local_library::ImportedPlaylist};

const EMBED: &str = "https://open.spotify.com/embed";
const SCRIPT_START: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;

/// Kind ("playlist" or "album") and id of a Spotify link or URI.
pub fn parse_link(link: &str) -> Option<(&'static str, String)> {
    let link = link.trim();
    let path = link.strip_prefix("https://open.spotify.com/").or_else(|| link.strip_prefix("spotify:"))?;
    // Links can carry a language segment first ("intl-it/album/...").
    let mut parts = path.split(['/', ':']);
    let kind = parts.find_map(|part| match part {
        "playlist" => Some("playlist"),
        "album" => Some("album"),
        _ => None,
    })?;
    let id: String = parts.next()?.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (id.len() == 22).then_some((kind, id))
}

pub fn import(link: &str) -> Result<ImportedPlaylist, String> {
    let (kind, id) = parse_link(link).ok_or("not a spotify playlist link")?;
    let agent = crate::http::agent(Duration::from_secs(15));
    let mut resp = agent.get(format!("{EMBED}/{kind}/{id}")).header("User-Agent", "Mozilla/5.0").call().map_err(|e| format!("network: {e}"))?;
    if resp.status().as_u16() != 200 {
        return Err(format!("spotify embed: status {}", resp.status().as_u16()));
    }
    let page = resp.body_mut().read_to_string().map_err(|e| format!("network: {e}"))?;
    let mut playlist = parse_page(&page)?;
    playlist.id = format!("spotify-{id}");
    // Best effort: without it the count shown is just the songs read.
    let total = agent
        .get(format!("https://open.spotify.com/{kind}/{id}"))
        .header("User-Agent", "Mozilla/5.0")
        .call()
        .ok()
        .and_then(|mut r| r.body_mut().read_to_string().ok())
        .and_then(|page| song_count(&page));
    playlist.total = total.unwrap_or(0).max(playlist.tracks.len() as u32);
    add_covers(&mut playlist.tracks);
    Ok(playlist)
}

/// Workers fetching covers at once: quick, without hammering the endpoint.
const COVER_WORKERS: usize = 8;

/// Gives every song its own cover in place of the playlist's, where Spotify has one. The
/// embed page lists one cover for all; the public oEmbed endpoint knows each song's.
pub fn add_covers(tracks: &mut [TrackSummary]) {
    let agent = crate::http::agent(Duration::from_secs(10));
    let chunk = tracks.len().div_ceil(COVER_WORKERS).max(1);
    std::thread::scope(|scope| {
        for part in tracks.chunks_mut(chunk) {
            let agent = &agent;
            scope.spawn(move || {
                for track in part {
                    if let Some(url) = cover_of(agent, &track.uri) {
                        track.image_url = url;
                    }
                }
            });
        }
    });
}

fn cover_of(agent: &ureq::Agent, uri: &str) -> Option<String> {
    let id = uri.strip_prefix("spotify:track:").filter(|id| id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric()))?;
    let mut resp = agent.get(format!("https://open.spotify.com/oembed?url=spotify:track:{id}")).call().ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    v.get("thumbnail_url")?.as_str().filter(|u| u.starts_with("https://")).map(String::from)
}

/// The song count a Spotify page states in its `music:song_count` tag.
fn song_count(page: &str) -> Option<u32> {
    let tag = page.find(r#"name="music:song_count""#)?;
    let rest = &page[tag..];
    let start = rest.find("content=\"")? + 9;
    rest[start..].split('"').next()?.parse().ok()
}

fn parse_page(page: &str) -> Result<ImportedPlaylist, String> {
    let start = page.find(SCRIPT_START).ok_or("spotify embed: no data")? + SCRIPT_START.len();
    let end = page[start..].find("</script>").ok_or("spotify embed: no data")? + start;
    let data: Value = serde_json::from_str(&page[start..end]).map_err(|e| format!("spotify embed: {e}"))?;
    let entity = data.pointer("/props/pageProps/state/data/entity").ok_or("spotify embed: private or missing playlist")?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let image_url = entity.pointer("/coverArt/sources/0/url").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = text(entity, "name");
    let tracks = entity
        .get("trackList")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    let uri = text(t, "uri");
                    uri.starts_with("spotify:track:").then(|| TrackSummary {
                        uri,
                        name: text(t, "title"),
                        // Artists come joined by a comma and a non-breaking space.
                        artists: text(t, "subtitle").split(',').map(|a| a.trim_matches(|c: char| c.is_whitespace()).to_string()).filter(|a| !a.is_empty()).collect(),
                        album: if text(entity, "type") == "album" { name.clone() } else { String::new() },
                        image_url: image_url.clone(),
                        duration_ms: t.get("duration").and_then(Value::as_u64).unwrap_or(0) as u32,
                        explicit: t.get("isExplicit").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if tracks.is_empty() {
        return Err("spotify embed: no songs".into());
    }
    Ok(ImportedPlaylist { name, owner: text(entity, "subtitle"), image_url, source: "spotify".into(), tracks, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_links_and_uris() {
        let id = "37i9dQZF1DXcBWIGoYBM5M";
        assert_eq!(parse_link(&format!("https://open.spotify.com/playlist/{id}?si=abc")), Some(("playlist", id.to_string())));
        assert_eq!(parse_link(&format!("https://open.spotify.com/intl-it/album/{id}")), Some(("album", id.to_string())));
        assert_eq!(parse_link(&format!("spotify:playlist:{id}")), Some(("playlist", id.to_string())));
        assert_eq!(parse_link(&format!("https://open.spotify.com/track/{id}")), None);
        assert_eq!(parse_link("https://evil.example/playlist/37i9dQZF1DXcBWIGoYBM5M"), None);
        assert_eq!(parse_link("https://open.spotify.com/playlist/short"), None);
    }

    #[test]
    fn reads_the_embed_page() {
        let page = format!(
            "<html>{SCRIPT_START}{}</script></html>",
            r#"{"props":{"pageProps":{"state":{"data":{"entity":{"type":"playlist","name":"Mix","subtitle":"Me",
            "coverArt":{"sources":[{"url":"https://i.scdn.co/image/x"}]},
            "trackList":[{"uri":"spotify:track:a","title":"Song","subtitle":"Ghali, Salmo","duration":1000,"isExplicit":true},
                         {"uri":"spotify:episode:b","title":"Pod","subtitle":"Host","duration":5}]}}}}}}"#
        );
        let p = parse_page(&page).unwrap();
        assert_eq!(p.name, "Mix");
        assert_eq!(p.tracks.len(), 1);
        assert_eq!(p.tracks[0].artists, ["Ghali", "Salmo"]);
        assert_eq!(p.tracks[0].image_url, "https://i.scdn.co/image/x");
        assert!(parse_page("<html>nothing</html>").is_err());
    }

    #[test]
    fn reads_the_real_song_count() {
        assert_eq!(song_count(r#"<meta name="music:song_count" content="270"/>"#), Some(270));
        assert_eq!(song_count("<meta name=\"other\" content=\"1\"/>"), None);
    }
}
