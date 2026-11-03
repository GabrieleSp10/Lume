#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backdrop;
mod i18n;
mod images;
mod library;
mod memory;
mod models;
mod nav;
mod theme_editor;
mod tray;

use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use app_core::{
    api::Api,
    auth::Auth,
    config::AppConfig,
    theme::{Color as ThemeColor, Material, Theme as CoreTheme},
};
use app_player::{Backend, Command, EngineChoice, EngineKind, Event, PlaybackEngine, PlaybackState, Ytdlp};
use slint::{
    winit_030::{winit, WinitWindowAccessor},
    ComponentHandle, Timer, TimerMode,
};

use i18n::t;
use library::Library;

slint::include_modules!();

type SharedState = Arc<Mutex<Option<PlaybackState>>>;

const POSITION_TICK_MS: u64 = 50;
const LYRICS_PAGE: i32 = 6;

/// Where users create their own Spotify app.
const SPOTIFY_DASHBOARD: &str = "https://developer.spotify.com/dashboard";
/// Engines in the order Settings shows them.
const ENGINES: [EngineKind; 3] = [EngineKind::Spotify, EngineKind::YouTube, EngineKind::Librespot];
const USAGE_TICK: Duration = Duration::from_secs(2);
const SETTINGS_PAGE: i32 = 4;

/// Renders through wgpu on DirectX 12 presented via DirectComposition: the only path that
/// keeps the window's alpha on Windows (OpenGL and GDI surfaces are composited opaque, which
/// is why transparency and blur did not show). Falls back to Slint's default renderer.
fn select_renderer() {
    use slint::wgpu_30::{wgpu, WGPUConfiguration, WGPUSettings};
    let mut settings = WGPUSettings::default();
    settings.backends = wgpu::Backends::DX12;
    settings.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
    if let Err(e) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .require_wgpu_30(WGPUConfiguration::Automatic(settings))
        // Without this, DWM draws the window's opaque GDI surface under the DirectX layer.
        .with_winit_window_attributes_hook(|attrs| {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_no_redirection_bitmap(true)
        })
        .select()
    {
        eprintln!("DirectX 12 renderer unavailable ({e}); the window will be opaque");
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // Starting Lume again shows the copy already running, maybe hidden in the tray.
    if tray::already_running() {
        return Ok(());
    }
    select_renderer();
    app_core::config::move_old_folders();
    let config = AppConfig::load_or_create().unwrap_or_default();
    let theme = CoreTheme::load_named(&config.theme);

    i18n::set_english(config.language == "en");

    let ui = MainWindow::new()?;
    ui.global::<L>().set_english(i18n::is_english());
    ui.global::<Motion>().set_on(config.animations);
    let theme_path = app_core::config::themes_dir().join(format!("{}.toml", config.theme));
    let editor = theme_editor::ThemeEditor::new(theme, theme_path, &ui);
    editor.wire(&ui);
    models::install(&ui);
    wire_window_callbacks(&ui);
    wire_undo(&ui);

    library::init_search_page(&ui, &config.recent_searches);
    ui.set_history_state(library::page_state(1, "", "", ""));
    ui.set_lyrics_state(library::lyrics_idle());
    memory::restore(&ui);
    wire_preferences(&ui, Rc::clone(&editor));
    wire_client_id(&ui);
    wire_playback_settings(&ui, &config);
    ui.set_client_id(config.client_id.trim().into());
    ui.set_redirect_uri(config.redirect_uri().into());
    ui.set_keep_in_tray(config.keep_in_tray);
    wire_onboarding(&ui);
    // Someone who set up Spotify before the first-run choice existed already chose.
    ui.set_onboarding(!config.onboarded && config.client_id.trim().is_empty());

    start_backend(&ui, &config);

    ui.show()?;
    apply_backdrop_when_ready(&ui, editor.material());
    memory::start_autosave(&ui);
    let result = ui.run();
    tray::remove_icon();
    memory::save(&ui);
    result
}

thread_local! {
    /// The local server could not start, so nothing can play.
    static SERVER_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set once the backend runs, so a new Client ID or engine can be applied.
    static SPOTIFY: std::cell::RefCell<Option<(Arc<Auth>, Arc<Backend>, Arc<Api>)>> = const { std::cell::RefCell::new(None) };
}

fn server_failed_state() -> PageState {
    library::page_state(
        3,
        t("L'app non è partita", "The app didn't start"),
        t("Forse è già aperta in un'altra finestra: chiudila e riapri.", "It may already be open in another window: close it and reopen."),
        "",
    )
}

/// The first-run choice: Spotify (own Client ID, Premium) or YouTube (no account).
fn wire_onboarding(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_onboarding_choose(move |choice| {
        let Some(ui) = weak.upgrade() else { return };
        let engine = if choice == 0 { EngineKind::Spotify } else { EngineKind::YouTube };
        ui.set_engine_index(ENGINES.iter().position(|e| *e == engine).unwrap_or(0) as i32);
        change_engine(|c| c.engine = engine.name().to_string());
        // Choosing YouTube installs what it needs, with nothing to do by hand.
        if choice == 1 {
            check_ytdlp_then(&ui, true);
        }
        // Steps 2 and 3 set up Spotify and YouTube; step 0 was the language.
        ui.set_onboarding_step(choice + 2);
    });
    let weak = ui.as_weak();
    ui.on_onboarding_done(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Ok(mut config) = AppConfig::load_or_create() {
            config.onboarded = true;
            let _ = config.save();
        }
        ui.set_onboarding(false);
        library::with(|lib| lib.refresh_home());
    });
    let weak = ui.as_weak();
    ui.on_home_acted(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_logged_in() {
            library::with(|lib| lib.refresh(0));
        } else {
            ui.set_import_open(true);
            nav::go(&ui, 2);
        }
    });
}

/// Saves the Client ID typed in Settings and connects with it, no restart needed.
fn wire_client_id(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_client_id_entered(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let id = text.trim();
        if !Auth::is_valid_client_id(id) {
            toast(&ui, t("Client ID non valido: sono 32 caratteri, lettere e numeri.", "Invalid Client ID: it's 32 characters, letters and numbers."));
            return;
        }
        let mut config = AppConfig::load_or_create().unwrap_or_default();
        if config.client_id == id {
            return;
        }
        config.client_id = id.to_string();
        if let Err(e) = config.save() {
            eprintln!("config not saved: {e}");
            toast(&ui, t("Client ID non salvato.", "Client ID not saved."));
            return;
        }
        ui.set_client_id(id.into());
        // A login belongs to one Spotify app: the new one starts logged out.
        if let Some((auth, backend, api)) = SPOTIFY.with(|s| s.borrow().clone()) {
            if ui.get_logged_in() {
                backend.send(Command::Pause);
                backend.logout();
                ui.set_logged_in(false);
                library::with(|lib| {
                    lib.set_spotify(false);
                    lib.refresh_home();
                });
            }
            api.forget_limits();
            auth.set_client_id(id);
        }
        toast(&ui, t("Client ID salvato: ora accedi a Spotify.", "Client ID saved: now log in to Spotify."));
    });

    ui.on_open_spotify_dashboard(|| open_url(SPOTIFY_DASHBOARD));
}

fn open_url(url: &str) {
    // rundll32 avoids cmd.exe quoting problems with URLs.
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

fn engine_choice(config: &AppConfig) -> EngineChoice {
    EngineChoice { kind: EngineKind::from_name(&config.engine), ytdlp: Ytdlp::from_config(config) }
}

/// Checks yt-dlp on a worker thread, since it can take a moment to start.
fn check_ytdlp(ui: &MainWindow) {
    check_ytdlp_then(ui, false);
}

/// Checks yt-dlp; with `install`, a missing or incomplete one is replaced by the app's own.
fn check_ytdlp_then(ui: &MainWindow, install: bool) {
    let weak = ui.as_weak();
    let tool = engine_choice(&AppConfig::load_or_create().unwrap_or_default()).ytdlp;
    ui.set_ytdlp_status(t("Controllo yt-dlp…", "Checking yt-dlp…").into());
    std::thread::spawn(move || {
        let version = tool.version();
        let complete = version.is_some() && tool.solves_challenges();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_ytdlp_found(complete);
            let status = match (version, complete) {
                (Some(version), true) => format!("{} (yt-dlp {version})", t("Componenti per YouTube pronti", "YouTube components ready")),
                (Some(_), false) => t("A yt-dlp manca una parte: installa i componenti per YouTube.", "yt-dlp is missing a part: install the YouTube components.").to_string(),
                (None, _) => t("Mancano i componenti per YouTube.", "The YouTube components are missing.").to_string(),
            };
            ui.set_ytdlp_status(status.into());
            if install && !complete {
                install_youtube_tools(&ui);
            }
        });
    });
}

/// Downloads yt-dlp and Deno into the app's folder, telling each step on screen.
fn install_youtube_tools(ui: &MainWindow) {
    if ui.get_tools_installing() {
        return;
    }
    ui.set_tools_installing(true);
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let step_weak = weak.clone();
        let result = app_core::tools::install(move |step| {
            let text = match step {
                app_core::tools::Step::YtDlp => t("Scarico yt-dlp (1 di 2)…", "Downloading yt-dlp (1 of 2)…"),
                app_core::tools::Step::Deno => t("Scarico Deno (2 di 2)…", "Downloading Deno (2 of 2)…"),
            };
            let _ = step_weak.upgrade_in_event_loop(move |ui| ui.set_ytdlp_status(text.into()));
        });
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_tools_installing(false);
            match result {
                Ok(()) => {
                    // The new yt-dlp only reaches the engine through a new resolver.
                    change_engine(|_| {});
                    check_ytdlp(&ui);
                }
                Err(e) => {
                    eprintln!("YouTube components not installed: {e}");
                    let text = if library::is_dns_problem(&e) || e.starts_with("network") {
                        t("Download non riuscito: controlla la connessione (o la VPN) e riprova.", "Download failed: check the connection (or the VPN) and try again.")
                    } else {
                        t("Installazione non riuscita: riprova.", "Installation failed: try again.")
                    };
                    ui.set_ytdlp_status(text.into());
                }
            }
        });
    });
}

/// Saves a change made in the playback section; it applies from the next song.
fn change_engine(change: impl Fn(&mut AppConfig)) {
    let Ok(mut config) = AppConfig::load_or_create() else { return };
    change(&mut config);
    if let Err(e) = config.save() {
        eprintln!("config not saved: {e}");
    }
    if let Some((_, backend, _)) = SPOTIFY.with(|s| s.borrow().clone()) {
        backend.set_engine(engine_choice(&config));
    }
}

fn wire_playback_settings(ui: &MainWindow, config: &AppConfig) {
    let kind = EngineKind::from_name(&config.engine);
    ui.set_engine_index(ENGINES.iter().position(|e| *e == kind).unwrap_or(0) as i32);
    ui.set_youtube_connected(app_core::config::youtube_cookies_file().exists());
    // Starting yt-dlp costs a second of CPU: only when YouTube is in use or being set up;
    // Settings asks again when it opens.
    if kind == EngineKind::YouTube || !config.onboarded {
        check_ytdlp(ui);
    }

    ui.on_engine_changed(|index| change_engine(|c| c.engine = ENGINES[index as usize].name().to_string()));
    ui.on_connect_youtube(|| {
        if let Some((_, backend, _)) = SPOTIFY.with(|s| s.borrow().clone()) {
            backend.youtube_login();
        }
    });
    ui.on_disconnect_youtube(|| {
        if let Some((_, backend, _)) = SPOTIFY.with(|s| s.borrow().clone()) {
            backend.youtube_logout();
        }
    });
    let weak = ui.as_weak();
    ui.on_check_ytdlp(move || {
        if let Some(ui) = weak.upgrade() {
            check_ytdlp(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_install_youtube_tools(move || {
        if let Some(ui) = weak.upgrade() {
            install_youtube_tools(&ui);
        }
    });
    // The app's own yt-dlp follows YouTube's changes; in the background, at most once a day.
    std::thread::spawn(app_core::tools::update_ytdlp);
}

/// Keeps the request counters in Settings current while that page is open.
fn start_usage_timer(ui: &MainWindow, api: Arc<Api>) {
    let weak = ui.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, USAGE_TICK, move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_page() != SETTINGS_PAGE {
            return;
        }
        let usage = api.usage();
        ui.set_usage_window(usage.last_window.to_string().into());
        ui.set_usage_total(usage.total.to_string().into());
        ui.set_usage_busiest(usage.busiest.clone().map_or_else(|| "—".to_string(), |(path, count)| format!("{path} ({count})")).into());
        ui.set_usage_playback_blocked(usage.playback_blocked);
        ui.set_usage_note(library::usage_note(&usage).into());
    });
    std::mem::forget(timer);
}

fn wire_preferences(ui: &MainWindow, editor: Rc<theme_editor::ThemeEditor>) {
    let save_preferences = |change: &dyn Fn(&mut AppConfig)| {
        if let Ok(mut config) = AppConfig::load_or_create() {
            change(&mut config);
            let _ = config.save();
        }
    };

    let weak = ui.as_weak();
    ui.on_language_changed(move |english| {
        i18n::set_english(english);
        save_preferences(&|c| c.language = if english { "en".into() } else { "it".into() });
        let Some(ui) = weak.upgrade() else { return };
        editor.relabel(&ui);
        ui.set_lyrics_state(library::lyrics_idle());
        if SERVER_FAILED.get() {
            ui.set_home_state(server_failed_state());
        }
        library::with(|lib| lib.relocalize());
    });

    ui.on_animations_changed(move |on| save_preferences(&|c| c.animations = on));
}

/// Starts the local server, playback engine and library, and binds them to the window.
/// Spotify joins in only once logged in.
fn start_backend(ui: &MainWindow, config: &AppConfig) {
    let auth = Arc::new(Auth::new(config.client_id.trim(), config.redirect_uri()));
    let api = Arc::new(Api::new(Arc::clone(&auth)));
    let (backend, events) = match Backend::new(Arc::clone(&auth), Arc::clone(&api), config.port, engine_choice(config)) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("local server failed: {e}");
            SERVER_FAILED.set(true);
            ui.set_home_state(server_failed_state());
            return;
        }
    };
    let backend = Arc::new(backend);
    SPOTIFY.with(|s| *s.borrow_mut() = Some((auth, Arc::clone(&backend), Arc::clone(&api))));
    let engine: Arc<dyn PlaybackEngine> = backend.clone();
    start_usage_timer(ui, Arc::clone(&api));
    let lib = Rc::new(Library::new(api, engine, ui.as_weak()));
    library::wire(ui, &lib);
    library::install(Rc::clone(&lib));
    ui.set_offline_enabled(config.offline);
    ui.set_download_saver(config.download_quality == "saver");
    ui.set_fast_downloads(config.fast_downloads);
    ui.set_offline_usage(library::downloads::usage_text().into());
    library::downloads::resume_all(&lib);

    let state: SharedState = Arc::default();
    spawn_event_pump(events, ui.as_weak(), Arc::clone(&backend), Arc::clone(&state));
    start_position_timer(ui, state);
    wire_player_callbacks(ui, backend.clone());

    backend.start_engine();
    let logged_in = !config.client_id.trim().is_empty() && backend.has_saved_login();
    if logged_in {
        ui.set_logged_in(true);
        lib.set_spotify(true);
        backend.connect_spotify();
    }
    lib.restore_session();
    lib.refresh_home();
}

fn to_slint(c: ThemeColor) -> slint::Color {
    slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
}

fn apply_theme(ui: &MainWindow, t: &CoreTheme) {
    let g = ui.global::<Theme>();
    let c = &t.colors;
    g.set_primary(to_slint(c.primary));
    // Relative luminance (sRGB weights) decides the readable text colour on the accent.
    let lum = 0.2126 * c.primary.r as f32 + 0.7152 * c.primary.g as f32 + 0.0722 * c.primary.b as f32;
    g.set_on_primary(if lum > 150.0 { slint::Color::from_rgb_u8(0x10, 0x13, 0x20) } else { slint::Color::from_rgb_u8(0xff, 0xff, 0xff) });
    g.set_secondary(to_slint(c.secondary));
    g.set_window_tint(to_slint(c.window_tint));
    // The more see-through the window, the stronger the veil under the content.
    let scrim = (0.82 - c.window_tint.a as f32 / 255.0).max(0.25);
    g.set_scrim(slint::Color::from_argb_u8((scrim * 255.0) as u8, 0x0a, 0x0d, 0x1a));
    g.set_panel(to_slint(c.panel));
    g.set_surface(to_slint(c.surface));
    g.set_text(to_slint(c.text));
    g.set_text_muted(to_slint(c.text_muted));
    g.set_border(to_slint(c.border));
    g.set_danger(to_slint(c.danger));
    g.set_radius_small(t.shape.radius_small);
    g.set_radius_medium(t.shape.radius_medium);
    g.set_radius_large(t.shape.radius_large);
    g.set_spacing(t.shape.spacing);
    g.set_font_size(t.typography.size_base);
    g.set_font_family(t.typography.family.clone().into());
}

thread_local! {
    /// What the toast's "Undo" button does, while that toast is up.
    static UNDO: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
}

/// Shows a short message above the player bar for a few seconds.
/// Closing ends Lume, or with "keep playing" on, leaves it playing in the notification area.
fn close_window(ui: &MainWindow) {
    if ui.get_keep_in_tray() {
        memory::save(ui);
        // The pointer leaves the window, so the close button is not still held when it comes
        // back (it would close it again); and it hides once this click is fully handled.
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerExited);
        ui.invoke_stay_open();
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                tray::hide_to_tray(&ui);
            }
        });
    } else {
        let _ = slint::quit_event_loop();
    }
}

fn toast(ui: &MainWindow, message: &str) {
    show_toast(ui, message, "", 4500);
}

/// A problem stays up longer than news: it usually says what to do.
fn toast_error(ui: &MainWindow, message: &str) {
    show_toast(ui, message, "", 7000);
}

/// Like `toast`, with an "Undo" button that runs `undo`; stays longer so it can be reached.
fn toast_with_undo(ui: &MainWindow, message: &str, undo: impl FnOnce() + 'static) {
    UNDO.with(|u| *u.borrow_mut() = Some(Box::new(undo)));
    show_toast(ui, message, t("Annulla", "Undo"), 8000);
}

fn wire_undo(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_toast_acted(move || {
        if let Some(undo) = UNDO.with(|u| u.borrow_mut().take()) {
            undo();
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_toast("".into());
            ui.set_toast_action("".into());
        }
    });
}

fn show_toast(ui: &MainWindow, message: &str, action: &str, duration: u64) {
    ui.set_toast(message.into());
    ui.set_toast_action(action.into());
    let weak = ui.as_weak();
    let shown = message.to_string();
    Timer::single_shot(Duration::from_millis(duration), move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_toast() == shown.as_str() {
                ui.set_toast("".into());
                ui.set_toast_action("".into());
            }
        }
    });
}

/// The native window only exists once the event loop is running, so retry briefly. Each
/// try schedules the next, so nothing keeps ticking once it worked.
fn apply_backdrop_when_ready(ui: &MainWindow, material: Material) {
    try_backdrop(ui.as_weak(), material, 1);
}

fn try_backdrop(weak: slint::Weak<MainWindow>, material: Material, attempt: u32) {
    Timer::single_shot(Duration::from_millis(100), move || {
        let Some(ui) = weak.upgrade() else { return };
        match backdrop::apply(ui.window(), material) {
            Ok(()) => memory::apply_window_size(&ui),
            Err(_) if attempt < 30 => try_backdrop(weak, material, attempt + 1),
            Err(e) => eprintln!("system backdrop not applied ({e}); showing the plain tint"),
        }
    });
}

fn wire_window_callbacks(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.on_close_app(move || {
        if let Some(ui) = weak.upgrade() {
            close_window(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            close_window(&ui);
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let weak = ui.as_weak();
    ui.on_keep_in_tray_changed(move |on| {
        if let Ok(mut config) = AppConfig::load_or_create() {
            config.keep_in_tray = on;
            let _ = config.save();
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_keep_in_tray(on);
        }
    });
    let weak = ui.as_weak();
    tray::install(move |action| {
        let Some(ui) = weak.upgrade() else { return };
        match action {
            tray::Action::Show => tray::show_window(&ui),
            tray::Action::PlayPause => library::with(|lib| lib.play_pause()),
            tray::Action::Quit => {
                let _ = slint::quit_event_loop();
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_minimized(true);
        }
    });

    let weak = ui.as_weak();
    ui.on_toggle_maximize(move || {
        if let Some(ui) = weak.upgrade() {
            let next = !ui.window().is_maximized();
            ui.window().set_maximized(next);
            ui.set_is_maximized(next);
        }
    });

    let weak = ui.as_weak();
    ui.on_nav_to(move |page| {
        if let Some(ui) = weak.upgrade() {
            if page == SETTINGS_PAGE && ui.get_ytdlp_status().is_empty() {
                check_ytdlp(&ui);
            }
            nav::go(&ui, page);
        }
    });
    let weak = ui.as_weak();
    ui.on_nav_back(move || {
        if let Some(ui) = weak.upgrade() {
            nav::back(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_nav_forward(move || {
        if let Some(ui) = weak.upgrade() {
            nav::forward(&ui);
        }
    });

    let weak = ui.as_weak();
    ui.on_set_fullscreen(move |on| {
        if let Some(ui) = weak.upgrade() {
            ui.window().set_fullscreen(on);
        }
    });

    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_resize_edge(move |dir| {
        use winit::window::ResizeDirection as D;
        let direction = match dir {
            0 => D::North,
            1 => D::South,
            2 => D::West,
            3 => D::East,
            4 => D::NorthWest,
            5 => D::NorthEast,
            6 => D::SouthWest,
            _ => D::SouthEast,
        };
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|w| {
                let _ = w.drag_resize_window(direction);
            });
        }
    });
}

fn wire_player_callbacks(ui: &MainWindow, backend: Arc<Backend>) {
    let b = Arc::clone(&backend);
    ui.on_login(move || {
        if let Err(e) = b.open_login_in_browser() {
            eprintln!("cannot open browser: {e}");
        }
    });

    let b = Arc::clone(&backend);
    let weak = ui.as_weak();
    ui.on_logout(move || {
        b.send(Command::Pause);
        b.logout();
        if let Some(ui) = weak.upgrade() {
            ui.set_logged_in(false);
            library::with(|lib| {
                lib.set_spotify(false);
                lib.refresh_home();
            });
            nav::go(&ui, 0);
        }
    });

    // Next and previous on a song restored from the last run start it first.
    let b = Arc::clone(&backend);
    ui.on_next(move || {
        let mut resumed = false;
        library::with(|lib| resumed = lib.resume());
        if !resumed {
            b.send(Command::Next);
        }
    });
    let b = Arc::clone(&backend);
    ui.on_prev(move || {
        let mut resumed = false;
        library::with(|lib| resumed = lib.resume());
        if !resumed {
            b.send(Command::Prev);
        }
    });
    let b = Arc::clone(&backend);
    ui.on_set_volume(move |v| b.send(Command::Volume(v)));
    // While dragging, send at most ~16 volume changes per second.
    let b = Arc::clone(&backend);
    let last = std::cell::Cell::new(std::time::Instant::now() - Duration::from_secs(1));
    ui.on_set_volume_live(move |v| {
        if last.get().elapsed() >= Duration::from_millis(60) {
            last.set(std::time::Instant::now());
            b.send(Command::Volume(v));
        }
    });

    let b = Arc::clone(&backend);
    let weak = ui.as_weak();
    ui.on_seek(move |fraction| {
        if let Some(ui) = weak.upgrade() {
            b.send(Command::SeekMs((fraction * ui.get_duration_ms()) as u32));
        }
    });

    let b = Arc::clone(&backend);
    let weak = ui.as_weak();
    ui.on_seek_relative(move |delta_ms| {
        if let Some(ui) = weak.upgrade() {
            if ui.get_has_track() {
                let target = (ui.get_position_ms() + delta_ms as f32).clamp(0.0, ui.get_duration_ms());
                b.send(Command::SeekMs(target as u32));
            }
        }
    });

    let b = Arc::clone(&backend);
    let weak = ui.as_weak();
    ui.on_toggle_shuffle(move || {
        if let Some(ui) = weak.upgrade() {
            b.send(Command::Shuffle(!ui.get_shuffle_on()));
        }
    });

    let b = backend;
    let weak = ui.as_weak();
    ui.on_toggle_repeat(move || {
        if let Some(ui) = weak.upgrade() {
            b.send(Command::Repeat(((ui.get_repeat_mode() + 1) % 3) as u8));
        }
    });
}

fn spawn_event_pump(
    events: std::sync::mpsc::Receiver<Event>,
    ui: slint::Weak<MainWindow>,
    backend: Arc<Backend>,
    state: SharedState,
) {
    std::thread::Builder::new()
        .name("event-pump".into())
        .spawn(move || {
            for event in events {
                let backend = Arc::clone(&backend);
                let state = Arc::clone(&state);
                let _ = ui.upgrade_in_event_loop(move |ui| handle_event(&ui, event, &backend, &state));
            }
        })
        .expect("spawn event pump");
}

fn handle_event(ui: &MainWindow, event: Event, backend: &Backend, shared: &SharedState) {
    match event {
        Event::Login(Ok(())) => {
            ui.set_logged_in(true);
            backend.connect_spotify();
            // With the account in, Spotify's own player is the steadiest engine; YouTube stays
            // as the backup it switches to on its own.
            if ui.get_engine_index() != 0 {
                ui.set_engine_index(0);
                change_engine(|c| c.engine = EngineKind::Spotify.name().to_string());
                toast(ui, t("Spotify collegato: la musica arriva da Spotify, YouTube resta di riserva.", "Spotify connected: music comes from Spotify, YouTube stays as backup."));
            }
            library::with(|lib| {
                lib.set_spotify(true);
                lib.refresh_home();
            });
        }
        Event::Login(Err(e)) => {
            eprintln!("login failed: {e}");
            ui.set_home_state(library::page_state(
                3,
                t("Accesso non riuscito", "Login failed"),
                t("Riprova ad accedere a Spotify.", "Try logging in to Spotify again."),
                t("Accedi", "Log in"),
            ));
        }
        Event::Ready { .. } => {
            ui.set_engine_ready(true);
            // The player starts at its default volume: apply the remembered one.
            backend.send(Command::Volume(if ui.get_muted() { 0.0 } else { ui.get_volume() }));
        }
        Event::Error(message) => {
            ui.set_track_loading(false);
            toast_error(ui, library::engine_message(&message));
        }
        Event::Switched { to, blocked_seconds } => toast(ui, &library::switch_message(to, blocked_seconds)),
        Event::YouTubeAccount(connected) => {
            ui.set_youtube_connected(connected);
            // A new resolver picks the login up, and forgets a block made without it.
            change_engine(|_| {});
            toast(ui, if connected { t("Account YouTube collegato.", "YouTube account connected.") } else { t("Account YouTube scollegato.", "YouTube account disconnected.") });
        }
        Event::State(state) => {
            // Starting is over once sound plays; a local engine says when it is still loading.
            if state.loading {
                ui.set_track_loading(true);
            } else if !state.paused {
                ui.set_track_loading(false);
            }
            let playing = !state.paused && state.track.is_some();
            ui.set_playing(playing);
            POSITION_TIMER.with(|timer| match (playing, timer.running()) {
                (true, false) => timer.restart(),
                (false, true) => timer.stop(),
                _ => {}
            });
            ui.set_has_track(state.track.is_some());
            ui.set_shuffle_on(state.shuffle);
            ui.set_repeat_mode(state.repeat_mode as i32);
            ui.set_duration_ms(state.duration_ms as f32);
            ui.set_duration_text(models::format_time(state.duration_ms).into());
            match &state.track {
                Some(t) => {
                    ui.set_track_title(t.name.clone().into());
                    ui.set_track_artist(t.artists.join(", ").into());
                    if models::now_uri() != t.uri {
                        library::with(|lib| lib.on_track_changed(t));
                    }
                }
                None => library::with(|lib| lib.on_track_cleared()),
            }
            ui.set_position_ms(state.position_ms as f32);
            ui.set_position_text(models::format_time(state.position_ms).into());
            // A seek or a pause leaves the timer still: the lyric line follows here.
            library::show_lyric_line(ui, state.position_ms);
            memory::on_state(&state);
            if state.track.is_some() {
                library::with(|lib| lib.on_state_changed());
            }
            *shared.lock().unwrap() = Some(state);
        }
    }
}

thread_local! {
    /// Runs only while a song plays: a paused app should not wake up twenty times a second.
    static POSITION_TIMER: Timer = Timer::default();
}

/// The SDK only reports position on state changes, so advance it locally while playing.
/// Ticks often so lyrics stay on the beat; the rest of the window updates every few ticks.
fn start_position_timer(ui: &MainWindow, shared: SharedState) {
    let weak = ui.as_weak();
    let mut tick = 0u32;
    POSITION_TIMER.with(|timer| timer.start(TimerMode::Repeated, Duration::from_millis(POSITION_TICK_MS), move || {
        let Some(ui) = weak.upgrade() else { return };
        let guard = shared.lock().unwrap();
        let Some(state) = guard.as_ref() else { return };
        if state.paused || state.track.is_none() {
            return;
        }
        let elapsed = state.updated_at.elapsed().as_millis() as u32;
        let position = (state.position_ms + elapsed).min(state.duration_ms);
        if ui.get_page() == LYRICS_PAGE {
            library::show_lyric_line(&ui, position);
        }
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(5) {
            ui.set_position_ms(position as f32);
            ui.set_position_text(models::format_time(position).into());
        }
    }));
    POSITION_TIMER.with(Timer::stop);
}
