//! Window backdrop.
//!
//! `Acrylic` and `Blur` use the undocumented `SetWindowCompositionAttribute` accent because
//! the documented DWM Acrylic/Mica backdrops turn solid whenever the window loses focus.
//! The blur radius is fixed by the OS in every case; the theme's `window_tint` is drawn by
//! the UI on top and controls how see-through the window looks.

use std::ffi::c_void;

use app_core::theme::Material;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::{
    core::{s, w, BOOL},
    Win32::{
        Foundation::HWND,
        Graphics::Dwm::{
            DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_SYSTEMBACKDROP_TYPE,
            DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWINDOWATTRIBUTE,
        },
        System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
        UI::{
            Controls::MARGINS,
            WindowsAndMessaging::{
                GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
                SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_SYSMENU,
            },
        },
    },
};

const DWMSBT_NONE: i32 = 1;
const DWMSBT_MAINWINDOW: i32 = 2; // Mica
const DWMSBT_TABBEDWINDOW: i32 = 4; // Mica Alt
const DWMWCP_ROUND: i32 = 2;

const WCA_ACCENT_POLICY: u32 = 19;
/// Tells DWM the client area is see-through; without an accent DWM paints its own fill.
const ACCENT_ENABLE_TRANSPARENTGRADIENT: u32 = 2;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
/// ABGR. An alpha of 0 renders opaque black on some builds; the visible tint comes from the UI.
const ACCENT_GRADIENT: u32 = 0x0100_0000;

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    gradient: u32,
    animation: u32,
}

#[repr(C)]
struct CompositionAttributeData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

type SetWindowCompositionAttribute = unsafe extern "system" fn(HWND, *mut CompositionAttributeData) -> BOOL;

pub fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    match HasWindowHandle::window_handle(&handle).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut c_void)),
        _ => None,
    }
}

fn set_i32(hwnd: HWND, attr: DWMWINDOWATTRIBUTE, value: i32) -> windows::core::Result<()> {
    // SAFETY: `value` outlives the call and the size matches the i32 attributes used here.
    unsafe { DwmSetWindowAttribute(hwnd, attr, &value as *const i32 as *const c_void, size_of::<i32>() as u32) }
}

fn set_accent(hwnd: HWND, state: u32) -> Result<(), String> {
    // SAFETY: user32 is always loaded in a GUI process, and the symbol has had this
    // signature on every Windows version that exports it.
    let set: SetWindowCompositionAttribute = unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).map_err(|e| e.to_string())?;
        let symbol = GetProcAddress(user32, s!("SetWindowCompositionAttribute"))
            .ok_or("SetWindowCompositionAttribute not found")?;
        std::mem::transmute(symbol)
    };
    // Only the transparent gradient needs flag 2 (use the gradient color); with it the blur\n    // accents render opaque.
    let flags = if state == ACCENT_ENABLE_TRANSPARENTGRADIENT { 2 } else { 0 };
    let mut policy = AccentPolicy { state, flags, gradient: ACCENT_GRADIENT, animation: 0 };
    let mut data = CompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: &mut policy as *mut AccentPolicy as *mut c_void,
        size: size_of::<AccentPolicy>(),
    };
    // SAFETY: `data` and `policy` outlive the call and describe a valid accent policy.
    if unsafe { set(hwnd, &mut data) }.as_bool() {
        Ok(())
    } else {
        Err("SetWindowCompositionAttribute failed".into())
    }
}

/// Removes `WS_SYSMENU`. winit restores styles on some state changes, so call this after
/// maximizing or restoring too.
pub fn hide_caption_buttons(hwnd: HWND) {
    // SAFETY: `hwnd` is a live window of this process.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        if style & WS_SYSMENU.0 as isize != 0 {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_SYSMENU.0 as isize));
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
    }
}

/// Applies the backdrop. Safe to call again to switch material at runtime. On failure the
/// UI stays tinted, and the error says which step the OS rejected.
pub fn apply(window: &slint::Window, material: Material) -> Result<(), String> {
    let hwnd = hwnd_of(window).ok_or("no Win32 window handle")?;
    let _ = set_i32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    let _ = set_i32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND);

    // The window's alpha already reaches DWM through DirectComposition. Mica alone needs the
    // frame extended under the client area; elsewhere that would paint DWM's dark frame
    // there. With the frame extended DWM also draws caption buttons, but only for windows
    // that have a system menu.
    hide_caption_buttons(hwnd);
    let mica = matches!(material, Material::Mica | Material::MicaAlt);
    let margins = if mica {
        MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 }
    } else {
        MARGINS::default()
    };
    // SAFETY: `margins` is a valid MARGINS for the duration of the call.
    unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) }.map_err(|e| format!("DwmExtendFrameIntoClientArea: {e}"))?;

    let backdrop = match material {
        Material::Mica => DWMSBT_MAINWINDOW,
        Material::MicaAlt => DWMSBT_TABBEDWINDOW,
        _ => DWMSBT_NONE,
    };
    set_i32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, backdrop).map_err(|e| format!("DWMWA_SYSTEMBACKDROP_TYPE: {e}"))?;

    set_accent(
        hwnd,
        match material {
            Material::Acrylic => ACCENT_ENABLE_ACRYLICBLURBEHIND,
            Material::Blur => ACCENT_ENABLE_BLURBEHIND,
            _ => ACCENT_ENABLE_TRANSPARENTGRADIENT,
        },
    )
}
