//! Page history behind the title bar's back and forward buttons, and the animated
//! hand-over between pages.

use std::{cell::RefCell, time::Duration};

use slint::{ComponentHandle, Timer};

use crate::{library, MainWindow, Motion};

/// `page-phase` values in app.slint.
const IDLE: i32 = 0;
const ENTERING: i32 = 2;

/// One frame, so the new page is drawn off to the side before it slides in.
const SETTLE: Duration = Duration::from_millis(16);

#[derive(Default)]
struct History {
    back: Vec<i32>,
    forward: Vec<i32>,
}

thread_local! {
    static HISTORY: RefCell<History> = RefCell::default();
}

fn switch_to(ui: &MainWindow, page: i32) {
    ui.set_page(page);
    ui.invoke_focus_main();
    library::with(|lib| lib.navigated(page));
}

fn show(ui: &MainWindow, page: i32) {
    HISTORY.with(|h| {
        let h = h.borrow();
        ui.set_can_back(!h.back.is_empty());
        ui.set_can_forward(!h.forward.is_empty());
    });
    if !ui.global::<Motion>().get_on() {
        switch_to(ui, page);
        return;
    }
    ui.set_page_phase(ENTERING);
    switch_to(ui, page);
    let weak = ui.as_weak();
    Timer::single_shot(SETTLE, move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_page_phase(IDLE);
        }
    });
}

/// Opens `page`, remembering where we came from.
pub fn go(ui: &MainWindow, page: i32) {
    let from = ui.get_page();
    if from != page {
        HISTORY.with(|h| {
            let mut h = h.borrow_mut();
            h.back.push(from);
            h.forward.clear();
        });
        show(ui, page);
    }
}

pub fn back(ui: &MainWindow) {
    let from = ui.get_page();
    let target = HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        let target = h.back.pop();
        if target.is_some() {
            h.forward.push(from);
        }
        target
    });
    if let Some(page) = target {
        show(ui, page);
    }
}

pub fn forward(ui: &MainWindow) {
    let from = ui.get_page();
    let target = HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        let target = h.forward.pop();
        if target.is_some() {
            h.back.push(from);
        }
        target
    });
    if let Some(page) = target {
        show(ui, page);
    }
}
