//! Hidden WebView2 hosting `player.html`. It runs its own tao event loop on a
//! dedicated thread so the UI toolkit keeps the main thread. The same loop opens the
//! YouTube login window, since both share the WebView2 profile.

use std::{
    fs,
    sync::{
        mpsc::{self, Sender},
        Arc,
    },
    thread,
};

use app_core::config::{webview_dir, youtube_cookies_file};
use tao::{
    dpi::LogicalSize,
    event::{Event as TaoEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows},
    window::{Window, WindowBuilder},
};
use wry::{PageLoadEvent, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows};

use crate::types::{parse_page_message, Event, PageMessage};

/// Receives what the page posts.
pub(crate) type PageHandler = Arc<dyn Fn(PageMessage) + Send + Sync>;

/// Google's sign-in page, coming back to YouTube once the user is in.
const YOUTUBE_LOGIN: &str = "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F";
/// Lets the hidden page start audio without a user gesture.
const BROWSER_ARGS: &str = "--autoplay-policy=no-user-gesture-required";
/// Cookies that only exist once someone is logged in.
const LOGGED_IN_COOKIES: [&str; 3] = ["SID", "__Secure-3PSID", "LOGIN_INFO"];

pub(crate) enum Request {
    /// A JSON command for the page's `__cmd`.
    Script(String),
    OpenYouTubeLogin,
    /// The login page reached YouTube: see whether the user is in.
    CheckYouTubeLogin,
    YouTubeLogout,
}

pub(crate) struct WebViewEngine {
    proxy: EventLoopProxy<Request>,
}

impl WebViewEngine {
    pub(crate) fn start(port: u16, key: String, events: Sender<Event>, on_message: PageHandler) -> Self {
        let (proxy_tx, proxy_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("webview-engine".into())
            .spawn(move || run(port, key, events, on_message, proxy_tx))
            .expect("spawn webview thread");
        Self { proxy: proxy_rx.recv().expect("webview thread failed to start") }
    }

    /// Hands `command`, a JSON object, to the page's `__cmd`.
    pub(crate) fn send(&self, command: String) {
        let _ = self.proxy.send_event(Request::Script(command));
    }

    pub(crate) fn request(&self, request: Request) {
        let _ = self.proxy.send_event(request);
    }
}

fn is_google(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    ["youtube.com", "google.com"].iter().any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

/// The cookies in the Netscape format yt-dlp reads with `--cookies`.
fn netscape(cookies: &[wry::cookie::Cookie<'static>]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let Some(domain) = c.domain().filter(|d| is_google(d)) else { continue };
        let domain = if domain.starts_with('.') { domain.to_string() } else { format!(".{domain}") };
        let expires = c.expires_datetime().map_or(0, |t| t.unix_timestamp().max(0));
        let secure = if c.secure().unwrap_or(false) { "TRUE" } else { "FALSE" };
        out.push_str(&format!("{domain}\tTRUE\t{}\t{secure}\t{expires}\t{}\t{}\n", c.path().unwrap_or("/"), c.name(), c.value()));
    }
    out
}

fn logged_in(cookies: &[wry::cookie::Cookie<'static>]) -> bool {
    cookies.iter().any(|c| LOGGED_IN_COOKIES.contains(&c.name()) && c.domain().is_some_and(is_google))
}

fn run(port: u16, key: String, events: Sender<Event>, on_message: PageHandler, proxy_tx: mpsc::SyncSender<EventLoopProxy<Request>>) {
    let event_loop = EventLoopBuilder::<Request>::with_user_event().with_any_thread(true).build();
    let proxy = event_loop.create_proxy();
    let _ = proxy_tx.send(event_loop.create_proxy());

    // winit (the UI) also registers "Window Class", tao's default; sharing it would route
    // this window through winit's window procedure and crash the process.
    let window = WindowBuilder::new()
        .with_title("Playback engine")
        .with_visible(false)
        .with_window_classname("SpotifyAppPlaybackEngine")
        .build(&event_loop);
    let window = match window {
        Ok(w) => w,
        Err(e) => {
            let _ = events.send(Event::Error(format!("cannot create engine window: {e}")));
            return;
        }
    };

    let mut context = WebContext::new(Some(webview_dir()));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(format!("http://127.0.0.1:{port}/player?k={key}"))
        .with_additional_browser_args(BROWSER_ARGS)
        .with_ipc_handler(move |req| {
            if let Some(message) = parse_page_message(req.body()) {
                on_message(message);
            }
        })
        .build(&window);
    let webview = match webview {
        Ok(w) => w,
        Err(e) => {
            let _ = events.send(Event::Error(format!("cannot start WebView2: {e}")));
            return;
        }
    };

    let mut login: Option<(Window, WebView)> = None;
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::Wait;
        let _keep_alive = &window;
        match event {
            TaoEvent::UserEvent(Request::Script(cmd)) => {
                let _ = webview.evaluate_script(&format!("window.__cmd({cmd})"));
            }
            TaoEvent::UserEvent(Request::OpenYouTubeLogin) => {
                if let Some((w, _)) = &login {
                    w.set_focus();
                    return;
                }
                let built = WindowBuilder::new()
                    .with_title("YouTube")
                    .with_inner_size(LogicalSize::new(520.0, 720.0))
                    .with_window_classname("LumeYouTubeLogin")
                    .build(target);
                let Ok(w) = built else { return };
                let check = proxy.clone();
                let view = WebViewBuilder::new_with_web_context(&mut context)
                    .with_url(YOUTUBE_LOGIN)
                    // One profile, one set of arguments: WebView2 refuses a second webview otherwise.
                    .with_additional_browser_args(BROWSER_ARGS)
                    .with_on_page_load_handler(move |load, url| {
                        if matches!(load, PageLoadEvent::Finished) && url.starts_with("https://www.youtube.com") {
                            let _ = check.send_event(Request::CheckYouTubeLogin);
                        }
                    })
                    .build(&w);
                match view {
                    Ok(view) => {
                        // Opened from the app's window: it must not hide behind it.
                        w.set_focus();
                        login = Some((w, view));
                    }
                    Err(e) => {
                        let _ = events.send(Event::Error(format!("cannot open YouTube login: {e}")));
                    }
                }
            }
            TaoEvent::UserEvent(Request::CheckYouTubeLogin) => {
                let Some((_, view)) = &login else { return };
                let Ok(cookies) = view.cookies() else { return };
                if !logged_in(&cookies) {
                    return;
                }
                let file = youtube_cookies_file();
                let saved = file.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::write(&file, netscape(&cookies)));
                if let Err(e) = saved {
                    let _ = events.send(Event::Error(format!("YouTube login not saved: {e}")));
                    return;
                }
                login = None;
                let _ = events.send(Event::YouTubeAccount(true));
            }
            TaoEvent::UserEvent(Request::YouTubeLogout) => {
                // Forget the login both in the app's file and in the browser profile.
                let _ = fs::remove_file(youtube_cookies_file());
                if let Ok(cookies) = webview.cookies() {
                    for c in cookies.iter().filter(|c| c.domain().is_some_and(is_google)) {
                        let _ = webview.delete_cookie(c);
                    }
                }
                let _ = events.send(Event::YouTubeAccount(false));
            }
            TaoEvent::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if login.as_ref().is_some_and(|(w, _)| w.id() == window_id) {
                    login = None;
                }
            }
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use wry::cookie::Cookie;

    use super::*;

    #[test]
    fn writes_only_youtube_and_google_cookies() {
        let mut sid = Cookie::new("SID", "abc");
        sid.set_domain(".youtube.com");
        sid.set_path("/");
        sid.set_secure(true);
        let mut other = Cookie::new("x", "y");
        other.set_domain("example.com");
        let text = netscape(&[sid.clone(), other.clone()]);
        assert!(text.starts_with("# Netscape HTTP Cookie File\n"));
        assert!(text.contains(".youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc"));
        assert!(!text.contains("example.com"));
        assert!(logged_in(&[sid]));
        assert!(!logged_in(&[other]));
        assert!(!is_google("notyoutube.com"));
    }
}
