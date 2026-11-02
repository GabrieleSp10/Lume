//! Loopback HTTP server. Routes:
//!   GET /?code=..&state=..   OAuth redirect target
//!   GET /player?k=<key>      the hidden player page
//!   GET /token?k=<key>       fresh access token for the player page
//!   GET /hls.js              the HLS player library (Apache-2.0, see vendor/hls.js-LICENSE)
//!   GET /media?k=<key>&u=..  YouTube's HLS streams, relayed so the page may read them

use std::{io, sync::{mpsc::Sender, Arc}, thread, time::Duration};

use app_core::auth::{percent_encode, Auth};
use tiny_http::{Header, Request, Response, Server};

use crate::types::Event;

const PLAYER_HTML: &str = include_str!("player.html");
const HLS_JS: &str = include_str!("vendor/hls.light.min.js");
/// Hosts the relay fetches from; anything else is refused, so the page cannot be used to
/// reach other addresses through this server.
const MEDIA_HOSTS: [&str; 2] = [".googlevideo.com", ".youtube.com"];

pub(crate) fn spawn(auth: Arc<Auth>, port: u16, key: String, events: Sender<Event>) -> io::Result<()> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| io::Error::other(format!("cannot listen on 127.0.0.1:{port}: {e}")))?;
    thread::Builder::new().name("loopback-server".into()).spawn(move || {
        let agent = app_core::http::agent(Duration::from_secs(20));
        for request in server.incoming_requests() {
            handle(request, &auth, port, &key, &events, &agent);
        }
    })?;
    Ok(())
}

fn handle(request: Request, auth: &Auth, port: u16, key: &str, events: &Sender<Event>, agent: &ureq::Agent) {
    // Reject requests whose Host header is not our loopback address (DNS rebinding).
    let host_ok = request
        .headers()
        .iter()
        .any(|h| h.field.equiv("Host") && h.value.as_str() == format!("127.0.0.1:{port}"));
    if !host_ok {
        let _ = request.respond(Response::from_string("forbidden").with_status_code(403));
        return;
    }

    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let param = |name: &str| query_param(query, name);
    let key_ok = param("k").as_deref() == Some(key);

    if path == "/media" && key_ok {
        // Segments are big and slow next to tokens: each gets its own thread.
        let (agent, key, target) = (agent.clone(), key.to_string(), param("u").unwrap_or_default());
        thread::spawn(move || {
            let _ = request.respond(relay(&agent, &key, &target));
        });
        return;
    }

    let response = match path {
        "/" => login_callback(auth, events, param("code"), param("state"), param("error")),
        "/hls.js" => Response::from_string(HLS_JS)
            .with_header(header("Content-Type", "text/javascript; charset=utf-8"))
            .with_header(header("Cache-Control", "no-store")),
        "/player" if key_ok => html(PLAYER_HTML.to_string(), 200),
        "/token" if key_ok => match auth.access_token() {
            Ok(token) => json(format!("{{\"token\":\"{token}\"}}"), 200),
            Err(e) => json(format!("{{\"error\":{:?}}}", e.to_string()), 401),
        },
        "/player" | "/token" | "/media" => Response::from_string("forbidden").with_status_code(403),
        _ => Response::from_string("not found").with_status_code(404),
    };
    let _ = request.respond(response);
}

/// Host of an https URL, if it is one.
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    rest.split(['/', '?', '#']).next()?.split(':').next()
}

fn allowed_media(url: &str) -> bool {
    https_host(url).is_some_and(|host| MEDIA_HOSTS.iter().any(|suffix| host.ends_with(suffix)))
}

/// Fetches a stream piece; playlists come back with every address pointing at this relay.
fn relay(agent: &ureq::Agent, key: &str, url: &str) -> Response<io::Cursor<Vec<u8>>> {
    if !allowed_media(url) {
        return Response::from_string("forbidden").with_status_code(403);
    }
    let fetched = agent.get(url).call().map_err(|e| e.to_string()).and_then(|mut resp| {
        let status = resp.status().as_u16();
        let kind = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
        resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec().map(|body| (status, kind, body)).map_err(|e| e.to_string())
    });
    match fetched {
        Ok((200, _, body)) if body.starts_with(b"#EXTM3U") => {
            let text = rewrite_playlist(&String::from_utf8_lossy(&body), url, key);
            Response::from_string(text).with_header(header("Content-Type", "application/vnd.apple.mpegurl"))
        }
        Ok((200, kind, body)) => Response::from_data(body).with_header(header("Content-Type", &kind)),
        Ok((status, ..)) => Response::from_string("upstream error").with_status_code(status),
        Err(e) => Response::from_string(format!("network: {e}")).with_status_code(502),
    }
}

/// The playlist with each address (lines and URI="..." attributes) sent through the relay.
fn rewrite_playlist(text: &str, base: &str, key: &str) -> String {
    let through = |address: &str| format!("/media?k={key}&u={}", percent_encode(&absolute(address, base)));
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else if !line.starts_with('#') {
                through(line.trim())
            } else if let Some(start) = line.find("URI=\"") {
                let value_start = start + 5;
                let value_end = line[value_start..].find('"').map_or(line.len(), |e| value_start + e);
                format!("{}{}{}", &line[..value_start], through(&line[value_start..value_end]), &line[value_end..])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `address` as a full URL, read relative to `base` as a browser would.
fn absolute(address: &str, base: &str) -> String {
    if address.starts_with("https://") || address.starts_with("http://") {
        return address.to_string();
    }
    let origin_end = base.find("://").map_or(0, |i| i + 3);
    let host_end = base[origin_end..].find('/').map_or(base.len(), |i| origin_end + i);
    if address.starts_with('/') {
        return format!("{}{address}", &base[..host_end]);
    }
    let path = base.split(['?', '#']).next().unwrap_or(base);
    let dir_end = path.rfind('/').filter(|i| *i >= host_end).map_or(path.len(), |i| i + 1);
    format!("{}{address}", &path[..dir_end])
}

fn login_callback(
    auth: &Auth,
    events: &Sender<Event>,
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
) -> Response<io::Cursor<Vec<u8>>> {
    let result = match (error, code, state) {
        (Some(e), _, _) => Err(format!("login refused: {e}")),
        (None, Some(code), Some(state)) => auth.complete_login(&code, &state).map_err(|e| e.to_string()),
        _ => Err("incomplete login callback".to_string()),
    };
    let page = match &result {
        Ok(()) => message_page("Accesso completato", "Puoi chiudere questa scheda e tornare all'app."),
        Err(e) => message_page("Accesso non riuscito", &escape(e)),
    };
    let _ = events.send(Event::Login(result));
    html(page, 200)
}

fn message_page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font:16px system-ui;background:#14161c;color:#e8eaf0;display:grid;place-items:center;height:100vh;margin:0\">\
         <div style=\"text-align:center\"><h2>{title}</h2><p>{body}</p></div>"
    )
}

fn html(body: String, status: u16) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn json(body: String, status: u16) -> Response<io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_query_values() {
        assert_eq!(query_param("code=a%2Fb&state=xyz", "code").as_deref(), Some("a/b"));
        assert_eq!(query_param("code=a&state=xyz", "state").as_deref(), Some("xyz"));
        assert_eq!(query_param("code=a", "missing"), None);
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn relays_only_youtube_media() {
        assert!(allowed_media("https://rr1---sn-x.googlevideo.com/videoplayback?id=1"));
        assert!(allowed_media("https://manifest.googlevideo.com/api/manifest/hls_playlist/x/index.m3u8"));
        assert!(!allowed_media("http://rr1---sn-x.googlevideo.com/x"));
        assert!(!allowed_media("https://evil.com/?.googlevideo.com"));
        assert!(!allowed_media("https://googlevideo.com.evil.com/x"));
        assert!(!allowed_media("https://127.0.0.1:8898/token"));
    }

    #[test]
    fn playlists_point_back_at_the_relay() {
        let base = "https://manifest.googlevideo.com/api/manifest/hls_playlist/id/index.m3u8";
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\nseg1.ts\n/abs/seg2.ts\nhttps://rr1.googlevideo.com/seg3.ts\n";
        let out = rewrite_playlist(text, base, "K");
        assert!(out.contains("URI=\"/media?k=K&u=https%3A%2F%2Fmanifest.googlevideo.com%2Fapi%2Fmanifest%2Fhls_playlist%2Fid%2Finit.mp4\""));
        assert!(out.contains("/media?k=K&u=https%3A%2F%2Fmanifest.googlevideo.com%2Fabs%2Fseg2.ts"));
        assert!(out.contains("/media?k=K&u=https%3A%2F%2Frr1.googlevideo.com%2Fseg3.ts"));
        assert!(out.starts_with("#EXTM3U"));
    }
}
