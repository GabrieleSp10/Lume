//! Lume in the notification area while its window is closed and the music goes on, and a
//! single running copy: starting Lume again brings the running one back instead of a second
//! copy that could not reach its local server.

use std::{cell::RefCell, mem::size_of, rc::Rc};

use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::CreateMutexW},
        UI::{
            Shell::{Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW},
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, FindWindowExW, GetCursorPos, LoadIconW, PostMessageW,
                RegisterClassW, SetForegroundWindow, ShowWindow, TrackPopupMenu, HICON, HWND_MESSAGE, IDI_APPLICATION, MF_SEPARATOR, MF_STRING,
                SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_LBUTTONUP,
                WM_RBUTTONUP, WNDCLASSW,
            },
        },
    },
};

use slint::ComponentHandle;

use crate::{backdrop::hwnd_of, i18n::t, MainWindow};

const CLASS: PCWSTR = w!("LumeTray");
/// The tray icon tells about clicks with this message.
const WM_TRAY: u32 = WM_APP + 1;
/// Another start of Lume asks this one to show itself.
const WM_SHOW_LUME: u32 = WM_APP + 2;
const OPEN: usize = 1;
const PLAY_PAUSE: usize = 2;
const QUIT: usize = 3;

/// What the notification area asks of the app.
pub enum Action {
    Show,
    PlayPause,
    Quit,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    hwnd: HWND,
    icon_shown: bool,
    on_action: Rc<dyn Fn(Action)>,
}

/// True if Lume already runs: that copy is asked to show itself, and this one should end.
pub fn already_running() -> bool {
    // Kept for the life of the process; Windows frees it when the process ends.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\Lume.single")) };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return false;
    }
    if let Ok(handle) = mutex {
        let _ = unsafe { CloseHandle(handle) };
    }
    if let Ok(other) = unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, PCWSTR::null()) } {
        let _ = unsafe { PostMessageW(Some(other), WM_SHOW_LUME, WPARAM(0), LPARAM(0)) };
    }
    true
}

/// Makes the hidden window that hears the tray icon and other starts of Lume. UI thread.
pub fn install(on_action: impl Fn(Action) + 'static) {
    let instance = unsafe { GetModuleHandleW(None) }.ok();
    let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.map(Into::into).unwrap_or_default(), lpszClassName: CLASS, ..Default::default() };
    unsafe { RegisterClassW(&class) };
    let hwnd = unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("Lume"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, class.hInstance.into(), None)
    };
    match hwnd {
        Ok(hwnd) => STATE.with(|s| *s.borrow_mut() = Some(State { hwnd, icon_shown: false, on_action: Rc::new(on_action) })),
        Err(e) => eprintln!("tray window not made: {e}"),
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() };
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = app_icon();
    let tip: Vec<u16> = "Lume".encode_utf16().collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

/// The icon embedded in the .exe (resource 1), or Windows' generic one.
fn app_icon() -> HICON {
    let instance = unsafe { GetModuleHandleW(None) }.ok();
    unsafe { LoadIconW(instance.map(Into::into), PCWSTR(1 as *const u16)) }
        .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
        .unwrap_or_default()
}

/// Hides the window, leaving Lume in the notification area.
pub fn hide_to_tray(ui: &MainWindow) {
    STATE.with(|s| {
        let mut state = s.borrow_mut();
        let Some(state) = state.as_mut() else { return };
        if !state.icon_shown {
            state.icon_shown = unsafe { Shell_NotifyIconW(NIM_ADD, &icon_data(state.hwnd)) }.as_bool();
        }
    });
    if let Some(hwnd) = hwnd_of(ui.window()) {
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

/// Brings the window back from the notification area.
pub fn show_window(ui: &MainWindow) {
    if let Some(hwnd) = hwnd_of(ui.window()) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
    remove_icon();
}

/// Takes the icon away, so none is left behind after Lume ends.
pub fn remove_icon() {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut().filter(|state| state.icon_shown) {
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &icon_data(state.hwnd)) };
            state.icon_shown = false;
        }
    });
}

fn act(action: Action) {
    // Run after the message is handled: the action may touch the window.
    let _ = slint::invoke_from_event_loop(move || {
        // Taken out first: the action itself may change the state.
        let on_action = STATE.with(|s| s.borrow().as_ref().map(|state| Rc::clone(&state.on_action)));
        if let Some(on_action) = on_action {
            on_action(action);
        }
    });
}

fn menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let label = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (open, play, quit) = (label(t("Apri Lume", "Open Lume")), label(t("Riproduci o pausa", "Play or pause")), label(t("Esci", "Quit")));
        let _ = AppendMenuW(menu, MF_STRING, OPEN, PCWSTR(open.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, PLAY_PAUSE, PCWSTR(play.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, QUIT, PCWSTR(quit.as_ptr()));
        let mut at = POINT::default();
        let _ = GetCursorPos(&mut at);
        // Without this the menu would not close when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, at.x, at.y, None, hwnd, None).0 as usize;
        let _ = DestroyMenu(menu);
        match chosen {
            OPEN => act(Action::Show),
            PLAY_PAUSE => act(Action::PlayPause),
            QUIT => act(Action::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_SHOW_LUME => act(Action::Show),
        WM_TRAY => match lparam.0 as u32 {
            WM_LBUTTONUP => act(Action::Show),
            WM_RBUTTONUP => menu(hwnd),
            _ => {}
        },
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    LRESULT(0)
}
