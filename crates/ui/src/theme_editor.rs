//! Live theme editor behind the Settings page. Every change is applied to the window at
//! once and written to the theme file shortly after.

use std::{cell::RefCell, fs, path::PathBuf, rc::Rc, time::Duration};

use app_core::theme::{Color, Material, Theme as CoreTheme};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::{backdrop, i18n::t, ColorToken, MainWindow};

const SAVE_DELAY: Duration = Duration::from_millis(500);
/// Largest corner radius the slider reaches (medium radius, in logical px).
const MAX_RADIUS: f32 = 24.0;
const MIN_TEXT: f32 = 11.0;
const MAX_TEXT: f32 = 18.0;
/// Wheel bitmap side in pixels (shown at 220 logical px; covers 2x scaling).
const WHEEL_SIZE: u32 = 440;

const LABELS: [(&str, &str); 9] = [
    ("Accento", "Accent"),
    ("Secondario", "Secondary"),
    ("Sfondo della finestra", "Window background"),
    ("Barra del player", "Player bar"),
    ("Superfici e schede", "Surfaces and cards"),
    ("Testo", "Text"),
    ("Testo secondario", "Secondary text"),
    ("Bordi", "Borders"),
    ("Avvisi", "Alerts"),
];

enum Undo {
    Reset(CoreTheme),
    Deleted { name: String, text: String },
}

pub struct ThemeEditor {
    theme: RefCell<CoreTheme>,
    path: PathBuf,
    ui: Weak<MainWindow>,
    tokens: Rc<VecModel<ColorToken>>,
    save_timer: Timer,
    /// What "Annulla" on the last toast takes back.
    undo: RefCell<Option<Undo>>,
}

fn token_mut(t: &mut CoreTheme, i: usize) -> Option<&mut Color> {
    let c = &mut t.colors;
    Some(match i {
        0 => &mut c.primary,
        1 => &mut c.secondary,
        2 => &mut c.window_tint,
        3 => &mut c.panel,
        4 => &mut c.surface,
        5 => &mut c.text,
        6 => &mut c.text_muted,
        7 => &mut c.border,
        8 => &mut c.danger,
        _ => return None,
    })
}

fn channel(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// HSV (each 0..=1) to RGB (each 0..=1).
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let h6 = h.rem_euclid(1.0) * 6.0;
    let c = v * s;
    let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
    let (r, g, b) = match h6 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (r + m, g + m, b + m)
}

/// RGB (each 0..=1) to HSV (each 0..=1). Grey has hue 0.
pub fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };
    let s = if max == 0.0 { 0.0 } else { delta / max };
    (h, s, max)
}

/// The colour wheel bitmap: hue by angle (0 at 3 o'clock, growing clockwise, as in
/// `ColorWheel` in app.slint), saturation by distance from the centre, full brightness.
pub fn wheel_image(size: u32) -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size, size);
    let half = size as f32 / 2.0;
    for (i, px) in buffer.make_mut_slice().iter_mut().enumerate() {
        let x = (i as u32 % size) as f32 + 0.5 - half;
        let y = (i as u32 / size) as f32 + 0.5 - half;
        let dist = (x * x + y * y).sqrt() / half;
        // One pixel of soft edge so the rim is not jagged.
        let edge = ((1.0 - dist) * half).clamp(0.0, 1.0);
        let (r, g, b) = hsv_to_rgb(y.atan2(x) / std::f32::consts::TAU, dist.min(1.0), 1.0);
        *px = slint::Rgba8Pixel { r: channel(r), g: channel(g), b: channel(b), a: channel(edge) };
    }
    slint::Image::from_rgba8(buffer)
}

fn material_index(m: Material) -> i32 {
    match m {
        Material::None => 0,
        Material::Blur => 1,
        Material::Acrylic => 2,
        Material::Mica | Material::MicaAlt => 3,
    }
}

impl ThemeEditor {
    pub fn new(theme: CoreTheme, path: PathBuf, ui: &MainWindow) -> Rc<Self> {
        let editor = Rc::new(Self {
            theme: RefCell::new(theme),
            path,
            ui: ui.as_weak(),
            tokens: Rc::default(),
            save_timer: Timer::default(),
            undo: RefCell::default(),
        });
        ui.set_theme_colors(ModelRc::from(editor.tokens.clone()));
        ui.set_color_wheel(wheel_image(WHEEL_SIZE));
        editor.show_saved(ui);

        editor.sync(ui);
        editor.select(ui, 0);
        editor
    }

    pub fn material(&self) -> Material {
        self.theme.borrow().material
    }

    fn show_saved(&self, ui: &MainWindow) {
        let names: Vec<SharedString> = CoreTheme::saved_names().iter().map(|n| n.as_str().into()).collect();
        ui.set_saved_themes(ModelRc::from(Rc::new(VecModel::from(names))));
    }

    /// Refreshes the labels after a language change.
    pub fn relabel(&self, ui: &MainWindow) {
        self.sync(ui);
    }

    /// Pushes the whole theme to the window and the editor controls.
    fn sync(&self, ui: &MainWindow) {
        let t = self.theme.borrow();
        crate::apply_theme(ui, &t);
        let rows: Vec<ColorToken> = (0..LABELS.len())
            .map(|i| {
                let mut copy = t.clone();
                let c = *token_mut(&mut copy, i).expect("index in range");
                ColorToken { label: crate::i18n::t(LABELS[i].0, LABELS[i].1).into(), hex: c.to_string().into(), color: crate::to_slint(c) }
            })
            .collect();
        // Update rows in place: rebuilding the model recreates the list mid-drag, which
        // changes the page height and throws the settings page back to the top.
        if self.tokens.row_count() == rows.len() {
            for (i, row) in rows.into_iter().enumerate() {
                if self.tokens.row_data(i).as_ref() != Some(&row) {
                    self.tokens.set_row_data(i, row);
                }
            }
        } else {
            self.tokens.set_vec(rows);
        }
        ui.set_window_opacity(t.colors.window_tint.a as f32 / 255.0);
        ui.set_radius_value(t.shape.radius_medium / MAX_RADIUS);
        ui.set_text_size_value((t.typography.size_base - MIN_TEXT) / (MAX_TEXT - MIN_TEXT));
        ui.set_material_index(material_index(t.material));
    }

    fn select(&self, ui: &MainWindow, index: i32) {
        let mut t = self.theme.borrow_mut();
        let Some(c) = token_mut(&mut t, index as usize) else { return };
        let c = *c;
        let (h, s, v) = rgb_to_hsv(c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
        ui.set_edit_index(index);
        ui.set_edit_h(h);
        ui.set_edit_s(s);
        ui.set_edit_v(v);
        ui.set_edit_a(c.a as f32 / 255.0);
    }

    /// Applies an edit, refreshes the window and schedules the save.
    fn edit(&self, change: impl FnOnce(&mut CoreTheme)) {
        let Some(ui) = self.ui.upgrade() else { return };
        let before = self.theme.borrow().material;
        change(&mut self.theme.borrow_mut());
        self.sync(&ui);
        let after = self.theme.borrow().material;
        if before != after {
            if let Err(e) = backdrop::apply(ui.window(), after) {
                crate::toast(&ui, &format!("{}: {e}", t("Sfondo non applicato", "Background not applied")));
            }
        }
        let (theme, path, weak) = (self.theme.borrow().clone(), self.path.clone(), self.ui.clone());
        self.save_timer.start(TimerMode::SingleShot, SAVE_DELAY, move || {
            let result = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| fs::write(&path, theme.to_toml()));
            if let (Err(e), Some(ui)) = (result, weak.upgrade()) {
                crate::toast(&ui, &format!("{}: {e}", t("Tema non salvato", "Theme not saved")));
            }
        });
    }

    /// Brings back the theme as it was before a reset or a deletion.
    fn apply_undo(self: &Rc<Self>) {
        let e = self;
        let Some(undo) = e.undo.borrow_mut().take() else { return };
        let Some(ui) = e.ui.upgrade() else { return };
        match undo {
            Undo::Reset(previous) => {
                e.edit(|t| *t = previous);
                e.select(&ui, ui.get_edit_index());
            }
            Undo::Deleted { name, text } => {
                if let Err(err) = CoreTheme::restore_named(&name, &text) {
                    eprintln!("theme not restored: {err}");
                }
                e.show_saved(&ui);
            }
        }
    }

    pub fn wire(self: &Rc<Self>, ui: &MainWindow) {
        let e = Rc::clone(self);
        ui.on_select_color(move |i| {
            if let Some(ui) = e.ui.upgrade() {
                e.select(&ui, i);
            }
        });

        let e = Rc::clone(self);
        ui.on_hsv_edited(move |i, h, s, v, a| {
            let (r, g, b) = hsv_to_rgb(h, s, v);
            e.edit(|t| {
                if let Some(c) = token_mut(t, i as usize) {
                    *c = Color::rgba(channel(r), channel(g), channel(b), channel(a));
                }
            });
        });

        let e = Rc::clone(self);
        ui.on_hex_entered(move |i, text| {
            let text = text.trim();
            let text = if text.starts_with('#') { text.to_string() } else { format!("#{text}") };
            match text.parse::<Color>() {
                Ok(color) => {
                    e.edit(|t| {
                        if let Some(c) = token_mut(t, i as usize) {
                            *c = color;
                        }
                    });
                    if let Some(ui) = e.ui.upgrade() {
                        e.select(&ui, i);
                    }
                }
                Err(_) => {
                    if let Some(ui) = e.ui.upgrade() {
                        crate::toast(&ui, t("Colore non valido: usa un codice come #b2b9ff", "Invalid color: use a code like #b2b9ff"));
                    }
                }
            }
        });

        let e = Rc::clone(self);
        ui.on_preset_accent(move |c| {
            e.edit(|t| t.colors.primary = Color::rgba(c.red(), c.green(), c.blue(), 255));
            if let Some(ui) = e.ui.upgrade() {
                if ui.get_edit_index() == 0 {
                    e.select(&ui, 0);
                }
            }
        });

        let e = Rc::clone(self);
        ui.on_window_opacity_edited(move |v| {
            e.edit(|t| t.colors.window_tint.a = channel(v));
            if let Some(ui) = e.ui.upgrade() {
                if ui.get_edit_index() == 2 {
                    e.select(&ui, 2);
                }
            }
        });

        let e = Rc::clone(self);
        ui.on_radius_edited(move |v| {
            e.edit(|t| {
                let medium = v.clamp(0.0, 1.0) * MAX_RADIUS;
                t.shape.radius_medium = medium;
                t.shape.radius_small = medium * 0.6;
                t.shape.radius_large = medium * 1.4;
            })
        });

        let e = Rc::clone(self);
        ui.on_text_size_edited(move |v| {
            e.edit(|t| t.typography.size_base = (MIN_TEXT + v.clamp(0.0, 1.0) * (MAX_TEXT - MIN_TEXT)).round())
        });

        let e = Rc::clone(self);
        ui.on_set_material(move |i| {
            e.edit(|t| {
                t.material = match i {
                    0 => Material::None,
                    1 => Material::Blur,
                    2 => Material::Acrylic,
                    _ => Material::Mica,
                }
            })
        });

        let e = Rc::clone(self);
        ui.on_reset_theme(move || {
            *e.undo.borrow_mut() = Some(Undo::Reset(e.theme.borrow().clone()));
            e.edit(|t| *t = CoreTheme::default());
            if let Some(ui) = e.ui.upgrade() {
                e.select(&ui, ui.get_edit_index());
                let undo = Rc::clone(&e);
                crate::toast_with_undo(&ui, t("Tema ripristinato", "Theme restored"), move || undo.apply_undo());
            }
        });


        let e = Rc::clone(self);
        ui.on_save_theme(move |name| {
            let Some(ui) = e.ui.upgrade() else { return };
            let result = e.theme.borrow().save_named(&name);
            match result {
                Ok(saved) => {
                    e.show_saved(&ui);
                    ui.set_theme_name("".into());
                    crate::toast(&ui, &format!("{} «{saved}»", t("Tema salvato:", "Theme saved:")));
                }
                Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {
                    crate::toast(&ui, t("Scegli un nome con lettere o numeri.", "Pick a name with letters or numbers."));
                }
                Err(err) => {
                    eprintln!("theme not saved: {err}");
                    crate::toast(&ui, t("Tema non salvato.", "Theme not saved."));
                }
            }
        });

        let e = Rc::clone(self);
        ui.on_delete_theme(move |name| {
            let Some(ui) = e.ui.upgrade() else { return };
            match CoreTheme::delete_named(&name) {
                Ok(text) => {
                    *e.undo.borrow_mut() = Some(Undo::Deleted { name: name.to_string(), text });
                    e.show_saved(&ui);
                    let undo = Rc::clone(&e);
                    crate::toast_with_undo(&ui, &format!("{} «{name}»", t("Tema eliminato:", "Theme deleted:")), move || undo.apply_undo());
                }
                Err(err) => {
                    eprintln!("theme not deleted: {err}");
                    crate::toast(&ui, t("Tema non eliminato.", "Theme not deleted."));
                }
            }
        });

        // The file dialog gets its own thread: it blocks until the user chooses.
        let weak = self.ui.clone();
        ui.on_import_theme(move || {
            let weak = weak.clone();
            let title = t("Scegli un tema", "Choose a theme").to_string();
            let filter = t("Tema di Lume", "Lume theme").to_string();
            std::thread::spawn(move || {
                if let Some(path) = rfd::FileDialog::new().set_title(title).add_filter(filter, &["toml"]).pick_file() {
                    let path = path.display().to_string();
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.invoke_theme_file_picked(path.into()));
                }
            });
        });

        let e = Rc::clone(self);
        ui.on_theme_file_picked(move |path| {
            let Some(ui) = e.ui.upgrade() else { return };
            match CoreTheme::import_file(std::path::Path::new(path.as_str())) {
                Ok(name) => {
                    e.show_saved(&ui);
                    crate::toast(&ui, &format!("{} «{name}»", t("Tema importato:", "Theme imported:")));
                }
                Err(err) => {
                    eprintln!("theme not imported: {err}");
                    crate::toast(&ui, t("Questo file non è un tema valido.", "This file is not a valid theme."));
                }
            }
        });

        let e = Rc::clone(self);
        ui.on_apply_theme(move |name| {
            let path = app_core::config::themes_dir().join(format!("{name}.toml"));
            match CoreTheme::load_file(&path) {
                Ok(theme) => {
                    e.edit(|t| *t = theme);
                    if let Some(ui) = e.ui.upgrade() {
                        e.select(&ui, ui.get_edit_index());
                    }
                }
                Err(err) => {
                    eprintln!("theme not loaded: {err}");
                    if let Some(ui) = e.ui.upgrade() {
                        crate::toast(&ui, t("Questo tema non si può aprire.", "This theme can't be opened."));
                    }
                }
            }
        });

        ui.on_open_themes_folder(|| {
            let dir = app_core::config::themes_dir();
            let _ = fs::create_dir_all(&dir);
            let _ = std::process::Command::new("explorer").arg(dir).spawn();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32, f32), b: (f32, f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01 && (a.2 - b.2).abs() < 0.01
    }

    #[test]
    fn hsv_primaries() {
        assert!(close(hsv_to_rgb(0.0, 1.0, 1.0), (1.0, 0.0, 0.0)));
        assert!(close(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), (0.0, 1.0, 0.0)));
        assert!(close(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), (0.0, 0.0, 1.0)));
        assert!(close(hsv_to_rgb(0.5, 0.0, 0.5), (0.5, 0.5, 0.5)));
    }

    #[test]
    fn hsv_roundtrip() {
        for &(r, g, b) in &[(0.3, 0.66, 1.0), (0.1, 0.1, 0.1), (1.0, 0.36, 0.42), (0.55, 0.49, 0.96)] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            assert!(close(hsv_to_rgb(h, s, v), (r, g, b)), "{r} {g} {b}");
        }
    }
}
