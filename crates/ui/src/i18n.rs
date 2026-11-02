//! Interface language for strings built in Rust. Strings in app.slint use the `L` global;
//! `set_english` keeps the two in step.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Picks the text for the current language.
pub fn t<'a>(italian: &'a str, english: &'a str) -> &'a str {
    if is_english() {
        english
    } else {
        italian
    }
}
