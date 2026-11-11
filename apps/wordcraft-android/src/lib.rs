//! WordCraft on Android, aimed at tablets with a keyboard, a stylus and a mouse.
//!
//! Android starts the app through `WordCraftActivity`, a `NativeActivity` (packaging/android/java:
//! its only Java keeps the latest intent), which loads this library and calls [`android_main`].
//! The UI is the desktop one (`wordcraft-ui-egui`); what differs on Android:
//! - Files: Open, Save As and the other file choices use WordCraft's built-in file browser over
//!   shared storage, which needs "All files access" granted once in Settings (`file_access`).
//! - Settings, the custom dictionary, added fonts and recovery copies (unsaved work kept for when
//!   Android stops the app, as the desktop keeps it for a crash) live in the app's data folder.
//! - Touch, S Pen, mouse, system bars and the keyboard cover (`tablet`); the system clipboard
//!   through JNI (`clipboard`); documents from other apps ("Open with", `incoming`); new builds
//!   from the GitHub repository the APK was built from (`updater`).
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
mod updater;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
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
    if let Some(dir) = p.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        log::warn!("settings: cannot create {}: {e}", dir.display());
        return;
    }
    match serde_json::to_vec_pretty(&app.prefs()) {
        Ok(bytes) => {
            if let Err(e) = write_atomically(&p, &bytes) {
                log::warn!("settings not saved to {}: {e}", p.display());
            }
        }
        Err(e) => log::warn!("settings not saved: {e}"),
    }
}

/// Write `bytes` to a sibling temporary file and rename it over `path`, so Android stopping the
/// app mid-write leaves the previous settings in place (as the desktop app writes them).
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "ui.json".into());
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The UI's context, for recovery's worker to wake the UI when it has news.
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// The custom dictionary (Add to Dictionary) file in the settings folder, as on the desktop.
fn user_dict_path() -> Option<PathBuf> {
    wordcraft_control_key::settings_dir().map(|d| d.join(Session::USER_DICT_FILE))
}

/// The custom dictionary and the fonts the user added, once at start-up.
fn load_user_files() {
    if let Some(dict) = user_dict_path() {
        wordcraft_proof::load_user_dictionary(&dict);
    }
    let Some(dir) = wordcraft_control_key::settings_dir() else { return };
    let (loaded, skipped) = wordcraft_fonts::FontDb::global().load_user_fonts(&dir.join("fonts"));
    if !loaded.is_empty() || !skipped.is_empty() {
        log::info!("user fonts: {} loaded, {} skipped", loaded.len(), skipped.len());
    }
}

/// The removable volumes (SD cards, USB drives) that are mounted now, each as its folder. Android
/// doesn't always list them under `/storage`, so the mount table is read as well.
fn removable_volumes() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir("/storage")
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        found.extend(mounted_volumes(&mounts));
    }
    found.retain(|p| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        name != "emulated" && name != "self" && !name.is_empty() && p.is_dir()
    });
    found.sort();
    found.dedup();
    found
}

/// The folders under `/storage` or `/mnt/media_rw` in a mount table (`/proc/mounts`), one per
/// line: `device mountpoint fstype …`. Mount points escape spaces and tabs as octal (`\040`).
fn mounted_volumes(mounts: &str) -> Vec<PathBuf> {
    mounts
        .lines()
        .filter_map(|line| {
            let mount = line.split_whitespace().nth(1)?.replace("\\040", " ").replace("\\011", "\t");
            let path = PathBuf::from(mount);
            let parent = path.parent()?;
            (parent == std::path::Path::new("/storage") || parent == std::path::Path::new("/mnt/media_rw")).then_some(path)
        })
        .collect()
}

/// The file browser's shortcuts: the device's storage and its usual folders, SD cards and USB
/// drives that are plugged in, and WordCraft's own folder.
fn places() -> Vec<(String, PathBuf)> {
    let mut out = vec![
        ("Internal storage".to_string(), PathBuf::from(SHARED)),
        ("Documents".to_string(), PathBuf::from(SHARED).join("Documents")),
        ("Download".to_string(), PathBuf::from(SHARED).join("Download")),
    ];
    for v in removable_volumes() {
        let name = v.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push((format!("SD card or USB drive ({name})"), v));
    }
    if let Some(dir) = app_dir() {
        out.push(("App folder".to_string(), dir));
    }
    out
}

/// New and Open asked for in a tab, for the host to open in a tab of their own (`tablet`).
pub(crate) type TabRequests = Rc<RefCell<Vec<(String, serde_json::Value)>>>;

/// What the host gives each tab's app on Android.
fn services(requests: &TabRequests) -> Services {
    let requests = Rc::clone(requests);
    Services {
        document_tabs: Some(Box::new(move |id, params| {
            requests.borrow_mut().push((id.to_string(), params.clone()));
            true
        })),
        builtin_file_browser: true,
        storage: Some(StorageAccess { granted: Box::new(file_access::granted), request: Box::new(file_access::request), places: Box::new(places) }),
        clipboard_text: Some(Box::new(clipboard::text)),
        clipboard_html: Some(Box::new(clipboard::html)),
        set_clipboard_rich: Some(Box::new(|text, html, _rtf, _mathml| clipboard::set_html(html, text))),
        // Personal templates (File › New, Save as Word Template), in the settings folder as on the desktop.
        templates_dir: wordcraft_control_key::settings_dir().map(|d| d.join("Templates")),
        ..Default::default()
    }
}

/// A tab's app on a blank document, with the saved settings.
pub(crate) fn new_tab(requests: &TabRequests) -> WordApp {
    let region = wordcraft_ui_egui::i18n::system_region();
    let mut session = Session::new(wordcraft_engine::blank_document(&region));
    session.system_region = region;
    session.user_dict_path = user_dict_path();
    // Recovery copies of unsaved work, as on the desktop: each tab's documents are locked by it,
    // so tabs share the folder the way desktop windows do, and File › Home offers what a stopped
    // app left.
    if let Some(dir) = wordcraft_control_key::settings_dir()
        && let Some(ctx) = CTX.get().cloned()
        && let Err(e) = session.enable_recovery(dir.join("recovery"), std::sync::Arc::new(move || ctx.request_repaint()))
    {
        log::warn!("local recovery unavailable: {e}");
    }
    let mut app = WordApp::new(session, services(requests));
    load_prefs(&mut app);
    // A region chosen in File › Options replaces the system's for the blank document.
    if app.session.prefs.region.is_some() {
        let doc = app.session.blank_document();
        app.session.set_document(doc);
    }
    app
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
            load_user_files();
            let _ = CTX.set(cc.egui_ctx.clone());
            let requests = TabRequests::default();
            let mut app = new_tab(&requests);
            if let Some(state) = &cc.wgpu_render_state {
                let info = state.adapter.get_info();
                log::info!("renderer: wgpu on {} ({:?}, {:?})", info.name, info.device_type, info.backend);
            }
            // Start on File › Home, as on the desktop; a document sent by another app opens over it.
            app.start_screen();
            Ok(Box::new(tablet::TabletApp::new(app, requests, shell_android)))
        }),
    );
    if let Err(e) = result {
        log::error!("WordCraft stopped: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::mounted_volumes;
    use std::path::PathBuf;

    #[test]
    fn removable_volumes_come_from_the_mount_table() {
        let mounts = "\
/dev/block/vold/public:8,1 /storage/1A2B-3C4D vfat rw,nosuid 0 0
/dev/fuse /storage/emulated fuse rw 0 0
/dev/block/dm-0 /data ext4 rw 0 0
/dev/block/vold/public:8,2 /storage/My\\040Card exfat rw 0 0
/dev/block/vold/public:8,3 /mnt/media_rw/ABCD-1234 vfat rw 0 0
/dev/block/vold/public:8,4 /storage/1A2B-3C4D/Sub vfat rw 0 0
";
        assert_eq!(
            mounted_volumes(mounts),
            vec![
                PathBuf::from("/storage/1A2B-3C4D"),
                PathBuf::from("/storage/emulated"),
                PathBuf::from("/storage/My Card"),
                PathBuf::from("/mnt/media_rw/ABCD-1234"),
            ]
        );
    }

    #[test]
    fn empty_mount_table_has_no_volumes() {
        assert!(mounted_volumes("").is_empty());
    }
}
