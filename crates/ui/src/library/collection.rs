//! The user's own collection: playlists made in the app, songs added to or removed from any
//! local playlist through the song menu, covers made from the songs', and local copies of
//! Spotify lists rebuilt from YouTube videos.

use std::{cell::RefCell, path::PathBuf, thread, time::Duration};

use app_core::{
    api::TrackSummary,
    config::{covers_dir, library_dir, AppConfig},
    local_library::{ImportedPlaylist, SOURCE_COPY, SOURCE_OWN},
};
use app_player::{video_for, Ytdlp};
use slint::{Image, ModelRc, SharedString, VecModel};

use super::{downloads, imported_summary, summary_of, with, Library, LOCAL_PLAYLIST};
use crate::{i18n::t, models, nav, MainWindow};

/// Actions of the song menu, as numbered in `TrackMenu`.
const QUEUE: i32 = 0;
const ADD: i32 = 1;
const REMOVE: i32 = 2;
const DOWNLOAD: i32 = 3;
const FORGET: i32 = 4;
const PLAYLISTS_PAGE: i32 = 2;
/// A short pause between YouTube searches while copying, so a long list does not look like
/// a bot.
const COPY_PAUSE: Duration = Duration::from_millis(800);

thread_local! {
    /// The playlists the song menu offers, in its order.
    static TARGETS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// The image chosen in the playlist form, if any.
    static FORM_COVER: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// Songs to add once the new playlist they were meant for is made.
    static PENDING: RefCell<Vec<TrackSummary>> = const { RefCell::new(Vec::new()) };
    /// The Spotify list being copied, if a copy is running.
    static COPYING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Lists the local playlists in the song menu.
pub(super) fn set_targets(ui: &MainWindow, playlists: &[ImportedPlaylist]) {
    TARGETS.with(|t| *t.borrow_mut() = playlists.iter().map(ImportedPlaylist::uri).collect());
    let names: Vec<SharedString> = playlists.iter().map(|p| p.name.as_str().into()).collect();
    ui.set_own_playlists(ModelRc::new(VecModel::from(names)));
}

/// Shown on the playlists' cards and pages instead of an owner.
pub(super) fn origin_text(p: &ImportedPlaylist) -> Option<String> {
    match p.source.as_str() {
        SOURCE_OWN => Some(t("Creata da te", "Made by you").to_string()),
        SOURCE_COPY => Some(format!("{} «{}»", t("Copia di", "Copy of"), p.origin)),
        _ => None,
    }
}

/// A playlist that makes its image from its songs gets a new one when its first songs change.
pub(super) fn refresh_cover(playlist: &ImportedPlaylist) {
    if playlist.custom_image || !matches!(playlist.source.as_str(), SOURCE_OWN | SOURCE_COPY) {
        return;
    }
    let covers = playlist.mosaic_covers();
    let target = match covers.len() {
        0 => String::new(),
        1..=3 => covers[0].clone(),
        _ => mosaic_path(&playlist.id, &covers).to_string_lossy().into_owned(),
    };
    if target == playlist.image_url {
        return;
    }
    let (uri, old) = (playlist.uri(), playlist.image_url.clone());
    thread::spawn(move || {
        if covers.len() == 4 {
            if let Err(e) = crate::images::compose_mosaic(&covers, PathBuf::from(&target).as_path()) {
                eprintln!("playlist cover not made: {e}");
                return;
            }
        }
        if old.starts_with(&*covers_dir().to_string_lossy()) {
            let _ = std::fs::remove_file(&old);
        }
        let _ = slint::invoke_from_event_loop(move || {
            with(|lib| {
                let Some(mut playlist) = lib.likes.local.playlist(&uri) else { return };
                playlist.image_url = target;
                lib.likes.local.update(&playlist);
                lib.after_change(&uri);
            });
        });
    });
}

/// Covers change name with their songs, so a new one is never mistaken for the old.
fn mosaic_path(id: &str, covers: &[String]) -> PathBuf {
    let hash = covers.iter().flat_map(|c| c.bytes()).fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    covers_dir().join(format!("mosaic-{id}-{hash:016x}.png"))
}

impl Library {
    /// Refreshes what shows a local playlist after it changed.
    pub(super) fn after_change(&self, uri: &str) {
        let open = self.detail_item.borrow().as_ref().is_some_and(|item| item.uri == uri);
        if open {
            self.reload_detail();
        }
        self.refresh_home();
    }

    /// Ctrl or Shift on a song.
    pub fn select_track(&self, source: i32, index: i32, mode: i32) {
        let count = models::select(source, index.max(0) as usize, mode);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selection_count(count as i32);
        }
    }

    pub fn clear_selection(&self) {
        models::clear_selection();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_selection_count(0);
        }
    }

    /// Something chosen in a song's menu. The menu of a selected song acts on all the songs
    /// selected; the menu of another song on that one only.
    pub fn track_menu(&self, source: i32, index: i32, action: i32, arg: i32) {
        let Some(row) = models::get().track_source(source).and_then(|m| slint::Model::row_data(&**m, index.max(0) as usize)) else { return };
        let rows = if row.selected { models::selected_rows(source) } else { vec![row] };
        self.clear_selection();
        let tracks: Vec<TrackSummary> = rows.iter().map(|row| summary_of(models::row_track(row))).collect();
        match action {
            QUEUE if tracks.len() == 1 => self.enqueue_item(source, index),
            QUEUE => self.enqueue_tracks(tracks.into_iter().map(super::track_of).collect()),
            ADD if arg < 0 => self.add_to_new_playlist(tracks),
            ADD => {
                if let Some(uri) = TARGETS.with(|t| t.borrow().get(arg as usize).cloned()) {
                    self.add_to_playlist(&uri, &tracks);
                }
            }
            REMOVE => self.remove_from_open_playlist(&tracks),
            DOWNLOAD => downloads::keep_songs(self, &tracks),
            FORGET => downloads::forget_songs(self, &tracks),
            _ => {}
        }
    }

    fn add_to_playlist(&self, uri: &str, tracks: &[TrackSummary]) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(name) = self.likes.local.playlist(uri).map(|p| p.name) else { return };
        let added = tracks.iter().filter(|track| self.likes.local.add_track(uri, track) == Some(true)).count();
        let text = match (added, tracks.len()) {
            (0, _) => format!("{} «{name}»", t("È già in", "Already in")),
            (1, 1) => format!("{} «{name}»", t("Aggiunto a", "Added to")),
            (n, _) => format!("{n} {} «{name}»", t("brani aggiunti a", "songs added to")),
        };
        crate::toast(&ui, &text);
        if added == 0 {
            return;
        }
        if let Some(playlist) = self.likes.local.playlist(uri) {
            refresh_cover(&playlist);
            if playlist.offline && playlist.downloadable() {
                downloads::queue_playlist(self, &playlist.uri());
            }
        }
        self.after_change(uri);
    }

    /// "New playlist…" in the menu: the form opens, and the songs go in once it is made.
    fn add_to_new_playlist(&self, tracks: Vec<TrackSummary>) {
        let Some(ui) = self.ui.upgrade() else { return };
        PENDING.with(|p| *p.borrow_mut() = tracks);
        nav::go(&ui, PLAYLISTS_PAGE);
        ui.invoke_open_create(true);
    }

    fn remove_from_open_playlist(&self, tracks: &[TrackSummary]) {
        let Some(uri) = self.detail_item.borrow().as_ref().map(|item| item.uri.to_string()) else { return };
        let Some(before) = self.likes.local.playlist(&uri) else { return };
        let mut after = before.clone();
        for track in tracks {
            if let Some(changed) = self.likes.local.remove_track(&uri, &track.uri) {
                after = changed;
            }
        }
        downloads::release(self, &after, tracks);
        refresh_cover(&after);
        self.after_change(&uri);
        if let Some(ui) = self.ui.upgrade() {
            let local = self.likes.local.clone();
            let text = match tracks.len() {
                1 => format!("{} «{}»", t("Rimosso da", "Removed from"), before.name),
                n => format!("{n} {} «{}»", t("brani rimossi da", "songs removed from"), before.name),
            };
            crate::toast_with_undo(&ui, &text, move || {
                local.update(&before);
                with(|lib| {
                    refresh_cover(&before);
                    lib.after_change(&before.uri());
                });
            });
        }
    }

    // ---- the playlist form ----

    pub fn pick_playlist_cover(&self) {
        let weak = self.ui.clone();
        let title = t("Immagine della playlist", "Playlist image").to_string();
        let filter = t("Immagini", "Images").to_string();
        thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title(title).add_filter(filter, &["jpg", "jpeg", "png"]).pick_file() else { return };
            let _ = weak.upgrade_in_event_loop(move |ui| match Image::load_from_path(&path) {
                Ok(image) => {
                    FORM_COVER.with(|c| *c.borrow_mut() = Some(path));
                    ui.set_form_cover(image);
                    ui.set_form_has_cover(true);
                }
                Err(_) => crate::toast_error(&ui, t("Questa immagine non si apre: scegline un'altra (JPG o PNG).", "This image won't open: pick another one (JPG or PNG).")),
            });
        });
    }

    pub fn clear_playlist_cover(&self) {
        FORM_COVER.with(|c| *c.borrow_mut() = None);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_form_cover(Image::default());
            ui.set_form_has_cover(false);
        }
    }

    pub fn create_playlist(&self, name: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let name = name.trim();
        if name.is_empty() {
            crate::toast(&ui, t("Dai un nome alla playlist.", "Give the playlist a name."));
            return;
        }
        let image = match FORM_COVER.with(|c| c.borrow().clone()).map(|path| keep_image(&path)) {
            Some(Ok(image)) => image,
            Some(Err(e)) => {
                eprintln!("playlist image not kept: {e}");
                crate::toast_error(&ui, t("L'immagine non si può salvare: riprova o scegline un'altra.", "The image can't be saved: try again or pick another."));
                return;
            }
            None => String::new(),
        };
        let playlist = self.likes.local.create(name, &image);
        ui.set_create_open(false);
        let pending = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
        if !pending.is_empty() {
            for track in &pending {
                let _ = self.likes.local.add_track(&playlist.uri(), track);
            }
            if let Some(playlist) = self.likes.local.playlist(&playlist.uri()) {
                refresh_cover(&playlist);
            }
        }
        self.refresh_home();
        crate::toast(&ui, &format!("{} «{}»", t("Creata", "Created"), playlist.name));
        let playlist = self.likes.local.playlist(&playlist.uri()).unwrap_or(playlist);
        self.open_detail(models::playlist_item(&imported_summary(&playlist), &self.ui));
    }

    /// "Edit" on a local playlist: the form fills with its name and image.
    pub fn edit_playlist(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(playlist) = self.open_local() else { return };
        ui.set_form_name(playlist.name.as_str().into());
        let custom = Some(PathBuf::from(&playlist.image_url)).filter(|_| playlist.custom_image);
        match custom.as_ref().and_then(|path| Image::load_from_path(path).ok()) {
            Some(image) => {
                ui.set_form_cover(image);
                ui.set_form_has_cover(true);
            }
            None => self.clear_playlist_cover(),
        }
        FORM_COVER.with(|c| *c.borrow_mut() = custom);
        ui.set_detail_editing(true);
    }

    pub fn save_playlist(&self, name: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(mut playlist) = self.open_local() else { return };
        let name = name.trim();
        if name.is_empty() {
            crate::toast(&ui, t("Dai un nome alla playlist.", "Give the playlist a name."));
            return;
        }
        playlist.name = name.to_string();
        match FORM_COVER.with(|c| c.borrow().clone()) {
            Some(path) if playlist.custom_image && path == PathBuf::from(&playlist.image_url) => {}
            Some(path) => match keep_image(&path) {
                Ok(image) => {
                    playlist.image_url = image;
                    playlist.custom_image = true;
                }
                Err(e) => {
                    eprintln!("playlist image not kept: {e}");
                    crate::toast_error(&ui, t("L'immagine non si può salvare: riprova o scegline un'altra.", "The image can't be saved: try again or pick another."));
                    return;
                }
            },
            None if playlist.custom_image => {
                playlist.custom_image = false;
                playlist.image_url.clear();
            }
            None => {}
        }
        self.likes.local.update(&playlist);
        refresh_cover(&playlist);
        ui.set_detail_editing(false);
        // The page header shows the item it was opened with: give it the new name and image.
        *self.detail_item.borrow_mut() = Some(models::playlist_item(&imported_summary(&playlist), &self.ui));
        self.after_change(&playlist.uri());
        crate::toast(&ui, t("Playlist aggiornata", "Playlist updated"));
    }

    pub(super) fn open_local(&self) -> Option<ImportedPlaylist> {
        let uri = self.detail_item.borrow().as_ref().map(|item| item.uri.to_string())?;
        uri.starts_with(LOCAL_PLAYLIST).then(|| self.likes.local.playlist(&uri)).flatten()
    }

    // ---- local copies of Spotify lists ----

    /// Rebuilds the open Spotify list from YouTube videos, as a playlist of the user's own:
    /// only names and artists are looked up, nothing is taken from Spotify but the list.
    pub fn copy_detail(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if COPYING.with(|c| c.borrow().is_some()) {
            crate::toast(&ui, t("Sto già copiando un'altra playlist: attendi che finisca.", "Already copying another playlist: wait for it to finish."));
            return;
        }
        let Some(source) = self.detail_item.borrow().as_ref().map(|item| item.uri.to_string()) else { return };
        let name = ui.get_detail_title().to_string();
        let tracks: Vec<TrackSummary> = slint::Model::iter(&*models::get().detail_tracks).map(|row| summary_of(models::row_track(&row))).collect();
        if tracks.is_empty() {
            return;
        }
        COPYING.with(|c| *c.borrow_mut() = Some(source.clone()));
        show_copy_progress(&ui, &source, 0, tracks.len());
        let (store, weak) = (self.api.store().clone(), self.ui.clone());
        thread::spawn(move || {
            let tool = Ytdlp::from_config(&AppConfig::load_or_create().unwrap_or_default());
            let total = tracks.len();
            let mut copied = Vec::new();
            let mut stopped = None;
            for (i, track) in tracks.into_iter().enumerate() {
                let known = app_core::offline::video_id(&track.uri).is_some();
                match video_for(&tool, &store, &super::track_of(track.clone())) {
                    Ok(video) => copied.push(TrackSummary {
                        uri: format!("youtube:{video}"),
                        album: String::new(),
                        image_url: format!("https://i.ytimg.com/vi/{video}/hqdefault.jpg"),
                        explicit: false,
                        ..track
                    }),
                    Err(e) if e.contains("not a bot") => {
                        stopped = Some(e);
                        break;
                    }
                    Err(e) => eprintln!("not copied: {e}"),
                }
                let (source, done) = (source.clone(), i + 1);
                let _ = weak.upgrade_in_event_loop(move |ui| show_copy_progress(&ui, &source, done, total));
                if !known {
                    thread::sleep(COPY_PAUSE);
                }
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                COPYING.with(|c| *c.borrow_mut() = None);
                with(|lib| lib.copy_done(&ui, &source, name, copied, total, stopped));
            });
        });
    }

    fn copy_done(&self, ui: &MainWindow, source: &str, name: String, tracks: Vec<TrackSummary>, total: usize, stopped: Option<String>) {
        show_copy_progress(ui, source, 0, 0);
        if tracks.is_empty() {
            let text = match stopped {
                Some(_) => t("YouTube ha chiesto una pausa: riprova la copia tra un quarto d'ora.", "YouTube asked for a break: try the copy again in a quarter of an hour."),
                None => t("Nessun brano trovato su YouTube: copia non creata.", "No songs found on YouTube: no copy made."),
            };
            crate::toast_error(ui, text);
            return;
        }
        let found = tracks.len();
        let taken: Vec<String> = self.likes.local.playlists().into_iter().map(|p| p.name).collect();
        let playlist = ImportedPlaylist {
            id: format!("copy-{:016x}", rand_id()),
            name: copy_name(&name, &taken),
            source: SOURCE_COPY.into(),
            origin: name.clone(),
            total: found as u32,
            tracks,
            ..Default::default()
        };
        self.likes.local.add(playlist.clone());
        refresh_cover(&playlist);
        self.refresh_home();
        let text = if stopped.is_some() {
            format!("{} «{name}» {} {found} {} {total}: {}", t("Copia di", "Copy of"), t("con", "with"), t("brani su", "of"), t("YouTube ha chiesto una pausa", "YouTube asked for a break"))
        } else if found < total {
            format!("{} «{name}»: {found} {} {total} {}", t("Copia pronta", "Copy ready"), t("brani su", "of"), t("trovati su YouTube", "songs found on YouTube"))
        } else {
            format!("{} «{name}»", t("Copia pronta", "Copy ready"))
        };
        crate::toast(ui, &text);
    }
}

/// "Mix (YouTube)", or "Mix (YouTube 2)" when that is taken: the library never shows the
/// same name twice.
fn copy_name(name: &str, taken: &[String]) -> String {
    let base = format!("{name} (YouTube)");
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{name} (YouTube {n})")).find(|candidate| !taken.contains(candidate)).unwrap_or(base)
}

fn rand_id() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64) ^ u64::from(std::process::id())
}

/// The copy's progress on the page it was started from; "" when none runs there.
fn show_copy_progress(ui: &MainWindow, source: &str, done: usize, total: usize) {
    let open = with_open(|uri| uri == source);
    if !open {
        return;
    }
    let text = if total == 0 { String::new() } else { format!("{} {done} {} {total}…", t("Copio", "Copying"), t("di", "of")) };
    ui.set_detail_copy_text(text.into());
}

/// Whether the open detail page passes `check`.
fn with_open(check: impl FnOnce(&str) -> bool) -> bool {
    let mut result = false;
    with(|lib| result = lib.detail_item.borrow().as_ref().is_some_and(|item| check(&item.uri)));
    result
}

/// What the copy button shows when a page opens: the running copy, if it is this list's.
pub(super) fn copy_text_for(uri: &str, ui: &MainWindow) {
    let running = COPYING.with(|c| c.borrow().as_deref() == Some(uri));
    if !running {
        ui.set_detail_copy_text("".into());
    }
}

/// Copies a chosen image into the library, so it stays if the original moves.
fn keep_image(path: &std::path::Path) -> Result<String, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png").to_lowercase();
    let dir = library_dir().join("images");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(format!("{:016x}.{ext}", rand_id()));
    std::fs::copy(path, &target).map_err(|e| e.to_string())?;
    Ok(target.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_never_share_a_name() {
        assert_eq!(copy_name("Mix", &[]), "Mix (YouTube)");
        assert_eq!(copy_name("Mix", &["Mix (YouTube)".into()]), "Mix (YouTube 2)");
    }

    #[test]
    fn mosaic_files_change_with_their_covers() {
        let a = mosaic_path("own-1", &["a".into(), "b".into(), "c".into(), "d".into()]);
        let b = mosaic_path("own-1", &["a".into(), "b".into(), "c".into(), "e".into()]);
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("mosaic-own-1-"));
    }

    #[test]
    fn own_and_copied_playlists_say_where_they_come_from() {
        let own = ImportedPlaylist { source: SOURCE_OWN.into(), ..Default::default() };
        assert!(origin_text(&own).is_some());
        let copy = ImportedPlaylist { source: SOURCE_COPY.into(), origin: "Mix".into(), ..Default::default() };
        assert!(origin_text(&copy).unwrap().contains("«Mix»"));
        assert!(origin_text(&ImportedPlaylist::default()).is_none());
    }
}
