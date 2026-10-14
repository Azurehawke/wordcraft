//! WordCraft on Android, aimed at tablets with a keyboard, a stylus and a mouse.
//!
//! Android starts the app through `WordCraftActivity`, a `NativeActivity` (packaging/android/java:
//! its only Java keeps the latest intent), which loads this library and calls [`android_main`].
//! The UI is the desktop one (`wordcraft-ui-egui`); what differs on Android:
//! - Files: Open, Save As and the other file choices use WordCraft's built-in file browser over
//!   shared storage, which needs "All files access" granted once in Settings (`file_access`).
//! - Settings, the custom dictionary, added fonts and logs live in the app's internal data folder.
//! - Touch, S Pen, mouse, system bars and the keyboard cover (`tablet`); the system clipboard
//!   through JNI (`clipboard`); documents from other apps ("Open with", `incoming`).
//! - No printing to a printer, no `--control` channel, no system clipboard pictures.
//!
//! The approach (NativeActivity, the patched winit, JNI for what Rust can't reach) follows
//! PdfCraft's Android port. Elsewhere the crate is empty, so `cargo build --workspace` on a
//! desktop still works.
#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod clipboard;
mod file_access;
mod incoming;
mod tablet;

use std::path::PathBuf;
use std::sync::OnceLock;

use winit::platform::android::activity::AndroidApp;
use wordcraft_engine::Session;
use wordcraft_ui_egui::file_browser::StorageAccess;
use wordcraft_ui_egui::{Services, UiState, WordApp};

/// Shared storage as apps see it (the device's "Internal storage").
const SHARED: &str = "/storage/emulated/0";

/// The app's internal data folder (settings, dictionary, fonts, logs, received documents).
static APP_DIR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn app_dir() -> Option<PathBuf> {
    APP_DIR.get().cloned()
}

/// `ui.json` in the settings folder (`<app data>/wordcraft`), as on the desktop.
fn prefs_path() -> Option<PathBuf> {
    wordcraft_control_key::settings_dir().map(|d| d.join("ui.json"))
}

fn load_prefs(app: &mut WordApp) {
    if let Some(p) = prefs_path()
        && let Ok(bytes) = std::fs::read(&p)
        && let Some(ui) = UiState::from_json(&bytes)
    {
        app.apply_prefs(ui);
    }
}

/// Write the interface settings: on leaving the foreground (Android may then stop the app
/// without warning) and on exit.
pub(crate) fn save_prefs(app: &WordApp) {
    let Some(p) = prefs_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_vec_pretty(&app.prefs()) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&p, bytes) {
                log::warn!("settings not saved to {}: {e}", p.display());
            }
        }
        Err(e) => log::warn!("settings not saved: {e}"),
    }
}

/// The custom dictionary (Add to Dictionary) and the fonts the user added, from the settings
/// folder, as on the desktop.
fn load_user_files(app: &mut WordApp) {
    let Some(dir) = wordcraft_control_key::settings_dir() else { return };
    let dict = dir.join(Session::USER_DICT_FILE);
    wordcraft_proof::load_user_dictionary(&dict);
    app.session.user_dict_path = Some(dict);
    let (loaded, skipped) = wordcraft_fonts::FontDb::global().load_user_fonts(&dir.join("fonts"));
    if !loaded.is_empty() || !skipped.is_empty() {
        log::info!("user fonts: {} loaded, {} skipped", loaded.len(), skipped.len());
    }
}

/// The file browser's shortcuts: the device's storage and its usual folders, SD cards and USB
/// drives that are plugged in, and WordCraft's own folder.
fn places() -> Vec<(String, PathBuf)> {
    let mut out = vec![
        ("Internal storage".to_string(), PathBuf::from(SHARED)),
        ("Documents".to_string(), PathBuf::from(SHARED).join("Documents")),
        ("Download".to_string(), PathBuf::from(SHARED).join("Download")),
    ];
    if let Ok(entries) = std::fs::read_dir("/storage") {
        let mut volumes: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name != "emulated" && name != "self" && e.path().is_dir()
            })
            .map(|e| e.path())
            .collect();
        volumes.sort();
        for v in volumes {
            let name = v.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            out.push((format!("SD card or USB drive ({name})"), v));
        }
    }
    if let Some(dir) = app_dir() {
        out.push(("App folder".to_string(), dir));
    }
    out
}

/// What the host gives the app on Android.
fn services() -> Services {
    Services {
        builtin_file_browser: true,
        storage: Some(StorageAccess { granted: Box::new(file_access::granted), request: Box::new(file_access::request), places: Box::new(places) }),
        clipboard_text: Some(Box::new(clipboard::text)),
        clipboard_html: Some(Box::new(clipboard::html)),
        ..Default::default()
    }
}

/// Entry point, called by android-activity on its own thread once the activity is created.
#[allow(unsafe_code)] // Exported by name: android-activity looks this symbol up.
#[unsafe(no_mangle)]
fn android_main(android: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("WordCraft"));
    std::panic::set_hook(Box::new(|info| {
        let trace = std::backtrace::Backtrace::force_capture();
        log::error!("internal error: {info}\n{trace}");
    }));

    let data = android.internal_data_path();
    if let Some(dir) = &data {
        let _ = APP_DIR.set(dir.clone());
        let tmp = dir.join("tmp");
        let _ = std::fs::create_dir_all(&tmp);
        // Android sets none of these. Settings go to the app's private folder (XDG_CONFIG_HOME,
        // read by `settings_dir`); the file browser's Home and `~` are shared storage; temporary
        // files go where the app may write. Nothing else runs yet: this is the app's only thread
        // besides Java's.
        #[allow(unsafe_code)]
        // SAFETY: called before eframe or the app start any thread that could read the environment.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", dir);
            std::env::set_var("HOME", SHARED);
            std::env::set_var("TMPDIR", &tmp);
        }
        log::info!("WordCraft {} data folder {}", env!("CARGO_PKG_VERSION"), dir.display());
    }
    file_access::set_app(android.clone());
    log::info!("All files access: {}", if file_access::granted() { "granted" } else { "not granted" });
    let shell_android = android.clone();

    let options = eframe::NativeOptions { android_app: Some(android), renderer: eframe::Renderer::Wgpu, ..Default::default() };
    let result = eframe::run_native(
        "WordCraft",
        options,
        Box::new(|cc| {
            let region = wordcraft_ui_egui::i18n::system_region();
            let mut session = Session::new(wordcraft_engine::blank_document(&region));
            session.system_region = region;
            let mut app = WordApp::new(session, services());
            load_prefs(&mut app);
            load_user_files(&mut app);
            // A region chosen in File › Options replaces the system's for the first blank document.
            if app.session.prefs.region.is_some() {
                let doc = app.session.blank_document();
                app.session.set_document(doc);
            }
            if let Some(state) = &cc.wgpu_render_state {
                let info = state.adapter.get_info();
                log::info!("renderer: wgpu on {} ({:?}, {:?})", info.name, info.device_type, info.backend);
            }
            // Start on File › Home, as on the desktop; a document sent by another app opens over it.
            app.start_screen();
            Ok(Box::new(tablet::TabletApp::new(app, shell_android)))
        }),
    );
    if let Err(e) = result {
        log::error!("WordCraft stopped: {e}");
    }
}
