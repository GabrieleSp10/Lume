//! UI-thread registry of the list models bound to the window, plus conversions from
//! API types to `Item` rows.

use std::{cell::RefCell, rc::Rc};

use app_player::Track;
use app_core::api::{AlbumSummary, PlaylistSummary, TrackSummary};
use slint::{Model, ModelRc, VecModel};

use crate::{i18n::t, images, Item, MainWindow};

#[derive(Clone)]
pub struct Models {
    pub quick_picks: Rc<VecModel<Item>>,
    pub playlists: Rc<VecModel<Item>>,
    pub results_tracks: Rc<VecModel<Item>>,
    pub results_albums: Rc<VecModel<Item>>,
    pub results_playlists: Rc<VecModel<Item>>,
    pub detail_tracks: Rc<VecModel<Item>>,
    pub history: Rc<VecModel<Item>>,
    pub queue: Rc<VecModel<Item>>,
}

impl Models {
    fn all(&self) -> [&Rc<VecModel<Item>>; 8] {
        [
            &self.quick_picks,
            &self.playlists,
            &self.results_tracks,
            &self.results_albums,
            &self.results_playlists,
            &self.detail_tracks,
            &self.history,
            &self.queue,
        ]
    }

    /// Track list by the `source` number the UI sends (see `play-track` in app.slint).
    pub fn track_source(&self, source: i32) -> Option<&Rc<VecModel<Item>>> {
        match source {
            0 => Some(&self.quick_picks),
            1 => Some(&self.results_tracks),
            2 => Some(&self.detail_tracks),
            3 => Some(&self.history),
            4 => Some(&self.queue),
            _ => None,
        }
    }
}

/// URLs/URIs of what the window currently shows outside the list models.
#[derive(Default)]
struct Current {
    now_art_url: String,
    detail_art_url: String,
    now_uri: String,
}

thread_local! {
    static MODELS: RefCell<Option<Models>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Current> = RefCell::new(Current::default());
    /// YouTube videos downloaded, so rows can show it without reading the disk each time.
    static OFFLINE: RefCell<std::collections::HashSet<String>> = RefCell::new(app_core::offline::downloaded());
}

/// Creates the models and binds them to the window. Call once on the UI thread.
pub fn install(ui: &MainWindow) {
    let m = Models {
        quick_picks: Rc::default(),
        playlists: Rc::default(),
        results_tracks: Rc::default(),
        results_albums: Rc::default(),
        results_playlists: Rc::default(),
        detail_tracks: Rc::default(),
        history: Rc::default(),
        queue: Rc::default(),
    };
    ui.set_quick_picks(ModelRc::from(m.quick_picks.clone()));
    ui.set_playlists(ModelRc::from(m.playlists.clone()));
    ui.set_results_tracks(ModelRc::from(m.results_tracks.clone()));
    ui.set_results_albums(ModelRc::from(m.results_albums.clone()));
    ui.set_results_playlists(ModelRc::from(m.results_playlists.clone()));
    ui.set_detail_tracks(ModelRc::from(m.detail_tracks.clone()));
    ui.set_history(ModelRc::from(m.history.clone()));
    ui.set_queue_items(ModelRc::from(m.queue.clone()));
    MODELS.with(|c| *c.borrow_mut() = Some(m));
}

pub fn get() -> Models {
    MODELS.with(|c| c.borrow().clone().expect("models::install was not called"))
}

pub fn set_now(uri: &str, art_url: &str) {
    CURRENT.with(|c| {
        let mut c = c.borrow_mut();
        c.now_uri = uri.to_string();
        c.now_art_url = art_url.to_string();
    });
}

pub fn now_uri() -> String {
    CURRENT.with(|c| c.borrow().now_uri.clone())
}

pub fn set_detail_art_url(url: &str) {
    CURRENT.with(|c| c.borrow_mut().detail_art_url = url.to_string());
}

/// Fills in covers that have finished downloading, for every model and the two big art slots.
pub fn refresh_images(ui: &MainWindow) {
    let models = get();
    for model in models.all() {
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else { continue };
            if row.has_image || row.image_url.is_empty() {
                continue;
            }
            if let Some(img) = images::get(&row.image_url) {
                row.image = img;
                row.has_image = true;
                model.set_row_data(i, row);
            }
        }
    }
    let (now, detail) = CURRENT.with(|c| {
        let c = c.borrow();
        (c.now_art_url.clone(), c.detail_art_url.clone())
    });
    if let Some(img) = images::get(&now) {
        ui.set_now_art(img);
        ui.set_now_has_art(true);
    }
    if let Some(img) = images::get(&detail) {
        ui.set_detail_art(img);
        ui.set_detail_has_art(true);
    }
}

/// Marks every row with this URI as liked/unliked, in all lists and the player bar.
pub fn set_liked_everywhere(ui: &MainWindow, uri: &str, liked: bool) {
    for model in get().all() {
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i) {
                if row.uri == uri && row.liked != liked {
                    row.liked = liked;
                    model.set_row_data(i, row);
                }
            }
        }
    }
    if now_uri() == uri {
        ui.set_now_liked(liked);
    }
}

/// Marks every row of a YouTube video as downloaded or not.
pub fn set_offline(video: &str, offline: bool) {
    OFFLINE.with(|o| {
        let mut set = o.borrow_mut();
        if offline { set.insert(video.to_string()) } else { set.remove(video) }
    });
    mark_offline(|uri| app_core::offline::video_id(uri) == Some(video), offline);
}

/// Reads the downloads again, after all of them were deleted.
pub fn reload_offline() {
    OFFLINE.with(|o| *o.borrow_mut() = app_core::offline::downloaded());
    mark_offline(|_| true, false);
}

fn mark_offline(matches: impl Fn(&str) -> bool, offline: bool) {
    for model in get().all() {
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i) {
                if row.youtube && row.offline != offline && matches(&row.uri) {
                    row.offline = offline;
                    model.set_row_data(i, row);
                }
            }
        }
    }
}

/// Shows a song waiting (1) or downloading (2, with its progress) in every list, or neither (0).
pub fn set_download_state(uri: &str, state: i32, progress: f32) {
    for model in get().all() {
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i) {
                if row.uri == uri && (row.download_state != state || row.download_progress != progress) {
                    row.download_state = state;
                    row.download_progress = progress;
                    model.set_row_data(i, row);
                }
            }
        }
    }
}

/// A Spotify song became its YouTube one: the rows follow, already downloaded.
pub fn rename_uri(old: &str, new: &str, video: &str) {
    for model in get().all() {
        for i in 0..model.row_count() {
            if let Some(mut row) = model.row_data(i) {
                if row.uri == old {
                    row.uri = new.into();
                    row.youtube = true;
                    row.offline = OFFLINE.with(|o| o.borrow().contains(video));
                    model.set_row_data(i, row);
                }
            }
        }
    }
}

// ---- several songs picked with Ctrl and Shift ----

thread_local! {
    /// The list and row a Shift selection counts from.
    static ANCHOR: std::cell::Cell<Option<(i32, usize)>> = const { std::cell::Cell::new(None) };
}

/// Ctrl (mode 1) adds or removes a song from the selection; Shift (2) selects from the last
/// one picked up to it. Returns how many songs are selected.
pub fn select(source: i32, index: usize, mode: i32) -> usize {
    let models = get();
    let Some(model) = models.track_source(source) else { return 0 };
    if ANCHOR.with(|a| a.get()).is_some_and(|(s, _)| s != source) {
        clear_selection();
    }
    let anchor = ANCHOR.with(|a| a.get()).map(|(_, i)| i);
    match (mode, anchor) {
        (2, Some(from)) => {
            let (low, high) = (from.min(index), from.max(index));
            for i in 0..model.row_count() {
                set_selected(model, i, (low..=high).contains(&i));
            }
        }
        _ => {
            let on = model.row_data(index).is_some_and(|row| !row.selected);
            set_selected(model, index, on);
            ANCHOR.with(|a| a.set(Some((source, index))));
        }
    }
    selected_rows(source).len()
}

fn set_selected(model: &VecModel<Item>, i: usize, on: bool) {
    if let Some(mut row) = model.row_data(i).filter(|row| row.selected != on) {
        row.selected = on;
        model.set_row_data(i, row);
    }
}

/// The selected rows of a list, in order.
pub fn selected_rows(source: i32) -> Vec<Item> {
    get().track_source(source).map(|model| model.iter().filter(|row| row.selected).collect()).unwrap_or_default()
}

pub fn clear_selection() {
    ANCHOR.with(|a| a.set(None));
    for model in get().all() {
        for i in 0..model.row_count() {
            set_selected(model, i, false);
        }
    }
}

fn is_offline(uri: &str) -> bool {
    app_core::offline::video_id(uri).is_some_and(|video| OFFLINE.with(|o| o.borrow().contains(video)))
}

/// Sets the hearts of a list loaded without them; `uris[i]` must match row `i`.
pub fn apply_likes(model: &VecModel<Item>, uris: &[String], flags: &[bool]) {
    for (i, (uri, &liked)) in uris.iter().zip(flags).enumerate() {
        if let Some(mut row) = model.row_data(i) {
            if row.uri == uri.as_str() && row.liked != liked {
                row.liked = liked;
                model.set_row_data(i, row);
            }
        }
    }
}

pub fn format_time(ms: u32) -> String {
    let total = ms / 1000;
    format!("{}:{:02}", total / 60, total % 60)
}

fn with_cover(mut item: Item, max_side: u32, ui: &slint::Weak<MainWindow>) -> Item {
    if let Some(img) = images::get(&item.image_url) {
        item.image = img;
        item.has_image = true;
    } else {
        images::request_sized(&item.image_url, max_side, ui);
    }
    item
}

pub fn track_item(t: &TrackSummary, liked: bool, ui: &slint::Weak<MainWindow>) -> Item {
    with_cover(
        Item {
            kind: 0,
            uri: t.uri.as_str().into(),
            title: t.name.as_str().into(),
            subtitle: t.artists.join(", ").into(),
            album: t.album.as_str().into(),
            duration: format_time(t.duration_ms).into(),
            duration_ms: t.duration_ms as i32,
            image_url: t.image_url.as_str().into(),
            explicit: t.explicit,
            liked,
            youtube: app_core::offline::video_id(&t.uri).is_some(),
            offline: is_offline(&t.uri),
            ..Default::default()
        },
        images::THUMBNAIL,
        ui,
    )
}

/// A row as the player needs it.
pub fn row_track(row: &Item) -> Track {
    Track {
        uri: row.uri.to_string(),
        name: row.title.to_string(),
        artists: row.subtitle.split(", ").map(String::from).collect(),
        album: row.album.to_string(),
        image_url: row.image_url.to_string(),
        duration_ms: row.duration_ms.max(0) as u32,
    }
}

pub fn playlist_item(p: &PlaylistSummary, ui: &slint::Weak<MainWindow>) -> Item {
    let subtitle = match p.track_count {
        Some(n) => format!("{} · {n} {}", p.owner, t("brani", "songs")),
        None => p.owner.clone(),
    };
    with_cover(
        Item {
            kind: 1,
            id: p.id.as_str().into(),
            uri: p.uri.as_str().into(),
            title: p.name.as_str().into(),
            subtitle: subtitle.into(),
            image_url: p.image_url.as_str().into(),
            ..Default::default()
        },
        images::FULL,
        ui,
    )
}

pub fn album_item(a: &AlbumSummary, ui: &slint::Weak<MainWindow>) -> Item {
    with_cover(
        Item {
            kind: 2,
            id: a.id.as_str().into(),
            uri: a.uri.as_str().into(),
            title: a.name.as_str().into(),
            subtitle: a.artists.join(", ").into(),
            image_url: a.image_url.as_str().into(),
            ..Default::default()
        },
        images::FULL,
        ui,
    )
}

/// The "Liked Songs" entry shown first among the playlists.
pub fn liked_card() -> Item {
    Item {
        kind: 3,
        title: t("Brani che ti piacciono", "Liked songs").into(),
        subtitle: t("Playlist · i tuoi preferiti", "Playlist · your favorites").into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn formats_minutes_and_seconds() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(61_500), "1:01");
        assert_eq!(format_time(3_599_000), "59:59");
    }
}
