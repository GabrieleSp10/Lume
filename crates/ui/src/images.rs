//! Background cover-art loader. Workers read the disk cache or download, then decode to a
//! pixel buffer (Send); the UI thread turns buffers into `slint::Image`s via
//! `models::refresh_images`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use app_core::config::covers_dir;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer, Weak};

use crate::MainWindow;

const WORKERS: usize = 8;
/// Track rows show covers around 50px: a small copy keeps long playlists light.
pub const THUMBNAIL: u32 = 112;
/// A cover that failed to load is asked again after this.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// Big covers: playlist cards and the lyrics page.
pub const FULL: u32 = 640;

type Job = (String, u32, Weak<MainWindow>);

struct Hub {
    cache: Mutex<HashMap<String, SharedPixelBuffer<Rgba8Pixel>>>,
    /// URLs queued or loaded.
    queued: Mutex<HashSet<String>>,
    /// When a URL last failed: it is asked again only after a pause, so a broken one does
    /// not loop and a passing network problem does not leave the cover missing for good.
    failed: Mutex<HashMap<String, Instant>>,
    tx: mpsc::Sender<Job>,
}

static HUB: OnceLock<Hub> = OnceLock::new();
static REFRESH_QUEUED: AtomicBool = AtomicBool::new(false);

fn hub() -> &'static Hub {
    HUB.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let agent = app_core::http::agent(Duration::from_secs(15));
        for n in 0..WORKERS {
            let rx = Arc::clone(&rx);
            let agent = agent.clone();
            thread::Builder::new()
                .name(format!("cover-loader-{n}"))
                .spawn(move || loop {
                    let job = rx.lock().unwrap().recv();
                    let Ok((url, max_side, ui)) = job else { break };
                    match fetch(&agent, &url, max_side) {
                        Ok(buffer) => {
                            hub().cache.lock().unwrap().insert(key(&url, max_side), buffer);
                            // One UI refresh covers every cover that lands before it runs, so a
                            // playlist of hundreds of tracks does not trigger hundreds of passes.
                            if !REFRESH_QUEUED.swap(true, Ordering::AcqRel) {
                                let _ = ui.upgrade_in_event_loop(|ui| {
                                    REFRESH_QUEUED.store(false, Ordering::Release);
                                    crate::models::refresh_images(&ui);
                                });
                            }
                        }
                        Err(e) => {
                            eprintln!("cover {url}: {e}");
                            hub().queued.lock().unwrap().remove(&key(&url, max_side));
                            hub().failed.lock().unwrap().insert(url, Instant::now());
                        }
                    }
                })
                .expect("spawn cover loader");
        }
        Hub { cache: Mutex::default(), queued: Mutex::default(), failed: Mutex::default(), tx }
    })
}

/// The image file for `url`, from the disk cache when possible: covers never change. An
/// address without a scheme is a file of the app's own (a chosen or composed cover).
fn file_bytes(agent: &ureq::Agent, url: &str, max_side: u32) -> Result<Vec<u8>, String> {
    if !url.contains("://") {
        return fs::read(url).map_err(|e| e.to_string());
    }
    // The whole address names the file: YouTube covers all end in the same "mqdefault.jpg".
    // Small copies for lists are kept apart from the sharp ones for big covers.
    let small = max_side <= THUMBNAIL;
    let name = if small { format!("{}-s", cover_name(url)) } else { cover_name(url) };
    let path = covers_dir().join(name);
    if let Ok(bytes) = fs::read(&path) {
        return Ok(bytes);
    }
    let mut last = String::new();
    let candidates = if small { vec![url.to_string()] } else { sharper(url) };
    for candidate in candidates {
        match download(agent, &candidate) {
            Ok(bytes) => {
                if fs::create_dir_all(covers_dir()).and_then(|_| fs::write(&path, &bytes)).is_err() {
                    eprintln!("cover not cached: {url}");
                }
                return Ok(bytes);
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = agent.get(url).call().map_err(|e| e.to_string())?;
    // Error pages are not covers: they must not end up in the cache.
    if resp.status().as_u16() != 200 {
        return Err(format!("status {}", resp.status().as_u16()));
    }
    resp.body_mut().read_to_vec().map_err(|e| e.to_string())
}

/// Sharper versions of a cover to try first, then the address itself. YouTube keeps a
/// full HD frame for most videos and a smaller one for all; Spotify serves 640px covers
/// under the same id as the 300px ones.
fn sharper(url: &str) -> Vec<String> {
    const YOUTUBE_SIZES: [&str; 3] = ["/maxresdefault.jpg", "/hqdefault.jpg", "/mqdefault.jpg"];
    if let Some(base) = YOUTUBE_SIZES.iter().find_map(|size| url.strip_suffix(size)).filter(|_| url.contains("i.ytimg.com/vi/")) {
        return YOUTUBE_SIZES.iter().map(|size| format!("{base}{size}")).collect();
    }
    if url.contains("/image/ab67616d00001e02") {
        return vec![url.replace("/image/ab67616d00001e02", "/image/ab67616d0000b273"), url.to_string()];
    }
    vec![url.to_string()]
}

fn fetch(agent: &ureq::Agent, url: &str, max_side: u32) -> Result<SharedPixelBuffer<Rgba8Pixel>, String> {
    let bytes = file_bytes(agent, url, max_side)?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let img = if img.width().max(img.height()) > max_side { img.thumbnail(max_side, max_side) } else { img };
    let rgba = img.to_rgba8();
    Ok(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

/// Side of a composed playlist cover.
const MOSAIC: u32 = 640;

/// Writes a playlist cover made of four song covers in a square, each cut to a square from
/// its middle, as YouTube frames are wider than tall. Runs on a worker thread.
pub fn compose_mosaic(covers: &[String], out: &Path) -> Result<(), String> {
    let agent = app_core::http::agent(Duration::from_secs(15));
    let half = MOSAIC / 2;
    let mut canvas = image::RgbaImage::new(MOSAIC, MOSAIC);
    for (i, url) in covers.iter().take(4).enumerate() {
        let img = image::load_from_memory(&file_bytes(&agent, url, FULL)?).map_err(|e| e.to_string())?;
        let side = img.width().min(img.height());
        let square = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
        let tile = square.resize_exact(half, half, image::imageops::FilterType::Triangle).to_rgba8();
        image::imageops::replace(&mut canvas, &tile, i64::from(i as u32 % 2 * half), i64::from(i as u32 / 2 * half));
    }
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    canvas.save(out).map_err(|e| e.to_string())
}

/// Queues a big cover unless the URL is empty or already known.
pub fn request(url: &str, ui: &Weak<MainWindow>) {
    request_sized(url, FULL, ui);
}

/// Like `request`, decoding to at most `max_side` pixels.
pub fn request_sized(url: &str, max_side: u32, ui: &Weak<MainWindow>) {
    if url.is_empty() {
        return;
    }
    let hub = hub();
    if hub.failed.lock().unwrap().get(url).is_some_and(|at| at.elapsed() < RETRY_AFTER) {
        return;
    }
    if hub.queued.lock().unwrap().insert(key(url, max_side)) {
        let _ = hub.tx.send((url.to_string(), max_side, ui.clone()));
    }
}

/// The decoded cover, if it has been downloaded already. UI thread only.
/// The sharpest copy of a cover loaded so far: a big slot shows the thumbnail while the
/// full copy is on its way, then the full one.
pub fn get(url: &str) -> Option<Image> {
    let cache = hub().cache.lock().unwrap();
    let buffer = cache.get(&key(url, FULL)).or_else(|| cache.get(&key(url, THUMBNAIL)))?.clone();
    Some(Image::from_rgba8(buffer))
}

fn key(url: &str, max_side: u32) -> String {
    format!("{max_side}|{url}")
}

/// File name of a cached cover: the letters and digits of its address, kept short.
fn cover_name(url: &str) -> String {
    let name: String = url.split_once("://").map_or(url, |(_, rest)| rest).chars().filter(char::is_ascii_alphanumeric).collect();
    name[name.len().saturating_sub(80)..].to_string()
}

#[cfg(test)]
mod tests {
    use super::cover_name;

    #[test]
    fn covers_with_the_same_last_segment_get_their_own_file() {
        let a = cover_name("https://i.ytimg.com/vi/aaaaaaaaaaa/mqdefault.jpg");
        let b = cover_name("https://i.ytimg.com/vi/bbbbbbbbbbb/mqdefault.jpg");
        assert_ne!(a, b);
        assert!(cover_name(&format!("https://x/{}", "y".repeat(300))).len() <= 80);
    }

    #[test]
    fn asks_for_the_sharpest_cover_first() {
        let yt = super::sharper("https://i.ytimg.com/vi/abc/mqdefault.jpg");
        assert_eq!(yt[0], "https://i.ytimg.com/vi/abc/maxresdefault.jpg");
        assert_eq!(yt.len(), 3);
        let sp = super::sharper("https://i.scdn.co/image/ab67616d00001e02ff");
        assert_eq!(sp[0], "https://i.scdn.co/image/ab67616d0000b273ff");
        assert_eq!(super::sharper("https://x/y.jpg"), ["https://x/y.jpg"]);
    }
}
