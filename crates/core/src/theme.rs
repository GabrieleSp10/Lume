//! Theme tokens. Every visual value the UI uses comes from here, so the whole
//! look can be changed by editing a TOML file (or, later, the in-app editor).

use std::{fmt, fs, io, path::Path, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::config::themes_dir;

/// File of the theme in use, rewritten at every edit; saved themes are separate copies.
const ACTIVE_THEME: &str = "default-dark";
/// Theme files are tiny; anything bigger is not one.
const MAX_THEME_FILE: u64 = 64 * 1024;

/// A name that is safe as a file name, or None if nothing usable is left. The active theme's
/// name is refused: saving there would be overwritten by the next edit.
pub fn clean_theme_name(name: &str) -> Option<String> {
    let cleaned: String = name.chars().filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_')).collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty() && cleaned.chars().count() <= 40 && cleaned != ACTIVE_THEME).then_some(cleaned)
}

/// RGBA color written as `#rrggbb` or `#rrggbbaa` in theme files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

impl FromStr for Color {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let hex = s.strip_prefix('#').ok_or_else(|| format!("color must start with '#': {s}"))?;
        if !matches!(hex.len(), 6 | 8) || !hex.is_ascii() {
            return Err(format!("color must be #rrggbb or #rrggbbaa: {s}"));
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| format!("{s}: {e}"));
        Ok(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: if hex.len() == 8 { byte(6)? } else { 255 } })
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(de::Error::custom)
    }
}

/// System backdrop behind the window (Windows 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Material {
    /// No backdrop: `window_tint` over whatever is behind, unblurred.
    None,
    /// Blur only.
    Blur,
    /// Blur plus the Windows acrylic grain and luminosity.
    Acrylic,
    /// Wallpaper-tinted system material; Windows turns it solid when the window loses focus.
    Mica,
    MicaAlt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Colors {
    /// Main brand color (active items, play button).
    pub primary: Color,
    /// Secondary accent (hover states, secondary buttons).
    pub secondary: Color,
    /// Tint laid over the backdrop behind the whole window. Its alpha controls transparency.
    pub window_tint: Color,
    /// Sidebar and player bar.
    pub panel: Color,
    /// Cards and list rows.
    pub surface: Color,
    pub text: Color,
    pub text_muted: Color,
    pub border: Color,
    pub danger: Color,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Shape {
    pub radius_small: f32,
    pub radius_medium: f32,
    pub radius_large: f32,
    /// Base spacing unit in logical pixels; layouts use multiples of it.
    pub spacing: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Typography {
    pub family: String,
    pub size_base: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub name: String,
    pub material: Material,
    pub colors: Colors,
    pub shape: Shape,
    pub typography: Typography,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            primary: Color::rgb(0xb2, 0xb9, 0xff),
            secondary: Color::rgb(0x5a, 0x5b, 0x7b),
            window_tint: Color::rgba(0x41, 0x42, 0x4b, 0xdd),
            panel: Color::rgba(0x26, 0x25, 0x26, 0x54),
            surface: Color::rgba(0xff, 0xff, 0xff, 0x17),
            text: Color::rgb(0xf2, 0xf4, 0xf8),
            text_muted: Color::rgb(0xc4, 0xca, 0xd8),
            border: Color::rgba(0xff, 0xff, 0xff, 0x26),
            danger: Color::rgb(0xff, 0x5c, 0x6c),
        }
    }
}

impl Default for Shape {
    fn default() -> Self {
        Self { radius_small: 6.0, radius_medium: 10.0, radius_large: 14.0, spacing: 8.0 }
    }
}

impl Default for Typography {
    fn default() -> Self {
        Self { family: "Segoe UI".into(), size_base: 14.0 }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "Default Dark".into(),
            material: Material::Acrylic,
            colors: Colors::default(),
            shape: Shape::default(),
            typography: Typography::default(),
        }
    }
}

impl Theme {
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("theme is always serializable")
    }

    pub fn load_file(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut theme = Self::from_toml(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // The text renderer cannot load this variable font and silently falls back to an
        // Arial-like face; older theme files still name it.
        if theme.typography.family == "Segoe UI Variable" {
            theme.typography.family = Typography::default().family;
        }
        Ok(theme)
    }

    /// Writes this theme to `<themes dir>/<name>.toml`, ready to share, and returns the name used.
    pub fn save_named(&self, name: &str) -> io::Result<String> {
        let name = clean_theme_name(name).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid theme name"))?;
        fs::create_dir_all(themes_dir())?;
        let mut theme = self.clone();
        theme.name = name.clone();
        fs::write(themes_dir().join(format!("{name}.toml")), theme.to_toml())?;
        Ok(name)
    }

    /// Copies a theme file into the themes folder and returns its name there. A name already
    /// taken gets a number, so importing never replaces a theme.
    pub fn import_file(path: &Path) -> io::Result<String> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not a theme file");
        if fs::metadata(path)?.len() > MAX_THEME_FILE {
            return Err(invalid());
        }
        let theme = Self::parse_import(&fs::read_to_string(path)?).ok_or_else(invalid)?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let base = clean_theme_name(stem).or_else(|| clean_theme_name(&theme.name)).unwrap_or_else(|| "Tema importato".into());
        let taken = Self::saved_names();
        let name = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base} {n}") })
            .find(|n| !taken.iter().any(|t| t.eq_ignore_ascii_case(n)))
            .expect("some number is free");
        theme.save_named(&name)
    }

    /// A theme from foreign text. Every field has a default, so any TOML would load: a real
    /// theme file must at least have its colors.
    fn parse_import(text: &str) -> Option<Self> {
        let value: toml::Table = text.parse().ok()?;
        value.get("colors")?.as_table()?;
        Self::from_toml(text).ok()
    }

    /// Deletes a saved theme and returns its file's content, for undo.
    pub fn delete_named(name: &str) -> io::Result<String> {
        if clean_theme_name(name).as_deref() != Some(name) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid theme name"));
        }
        let path = themes_dir().join(format!("{name}.toml"));
        let text = fs::read_to_string(&path)?;
        fs::remove_file(path)?;
        Ok(text)
    }

    /// Puts back a theme removed by `delete_named`.
    pub fn restore_named(name: &str, text: &str) -> io::Result<()> {
        fs::write(themes_dir().join(format!("{name}.toml")), text)
    }

    /// Names of the themes saved in the themes folder, sorted.
    pub fn saved_names() -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(themes_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension()? == "toml").then(|| path.file_stem()?.to_str().map(String::from))?
            })
            .filter(|name| name != ACTIVE_THEME)
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names
    }

    /// Loads `<themes dir>/<name>.toml`; the default theme is written to disk if
    /// it is missing so users have a template to edit. Falls back to the built-in
    /// theme if the file is unreadable or invalid.
    pub fn load_named(name: &str) -> Self {
        let path = themes_dir().join(format!("{name}.toml"));
        if !path.exists() && name == "default-dark" {
            let _ = fs::create_dir_all(themes_dir());
            let _ = fs::write(&path, Theme::default().to_toml());
        }
        Self::load_file(&path).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_names_are_safe_file_names() {
        assert_eq!(clean_theme_name(" Notte blu! ").as_deref(), Some("Notte blu"));
        assert_eq!(clean_theme_name("../../evil").as_deref(), Some("evil"));
        assert_eq!(clean_theme_name("???"), None);
        assert_eq!(clean_theme_name("default-dark"), None);
    }

    #[test]
    fn import_needs_colors() {
        assert!(Theme::parse_import("[colors]\nprimary = \"#ff0000\"\n").is_some());
        assert!(Theme::parse_import("title = \"not a theme\"\n").is_none());
        assert!(Theme::parse_import("not toml at all [").is_none());
    }

    #[test]
    fn color_roundtrip() {
        let c: Color = "#4ca8ff80".parse().unwrap();
        assert_eq!(c, Color::rgba(0x4c, 0xa8, 0xff, 0x80));
        assert_eq!(c.to_string(), "#4ca8ff80");
        assert_eq!("#101010".parse::<Color>().unwrap().a, 255);
        assert!("101010".parse::<Color>().is_err());
        assert!("#12345".parse::<Color>().is_err());
    }

    #[test]
    fn theme_roundtrip_and_partial_files() {
        let t = Theme::default();
        let back = Theme::from_toml(&t.to_toml()).unwrap();
        assert_eq!(back.colors.primary, t.colors.primary);
        // A theme file may override only a few tokens.
        let partial = Theme::from_toml("[colors]\nprimary = \"#ff0000\"\n").unwrap();
        assert_eq!(partial.colors.primary, Color::rgb(255, 0, 0));
        assert_eq!(partial.shape.spacing, 8.0);
    }
}
