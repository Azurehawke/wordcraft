//! Updates from the GitHub repository the APK was built from.
//!
//! CI publishes every build of the `android` branch as the `android-latest` prerelease, titled
//! "WordCraft for Android (build N)", with N the APK's versionCode. At start-up, and when the app
//! comes back to the front after an hour or more, WordCraft asks GitHub's API for that release; a
//! higher build number than its own shows a card offering the update. Update downloads the APK
//! into the app's cache and hands it to Android's PackageInstaller, which asks the user to
//! confirm (the first time Android also asks to allow WordCraft to install apps). Android only
//! accepts an update signed with the installed app's key, so a tampered APK can't replace it.
//!
//! The HTTP and the install are Java (`WordCraftActivity`: Android's own TLS, no extra crates),
//! run on a worker thread; this module keeps the state and draws the card.

use std::sync::mpsc::{Receiver, TryRecvError, channel};

use egui::{Align2, Color32, RichText};
use jni::objects::JValue;
use serde_json::Value;

use crate::file_access::with_activity;
use crate::incoming::string_of;

/// `owner/repo` the APK was built from (GitHub Actions sets GITHUB_REPOSITORY for the build).
const REPOSITORY: &str = match option_env!("GITHUB_REPOSITORY") {
    Some(r) => r,
    None => "Azurehawke/wordcraft",
};
const TAG: &str = "android-latest";
/// Check again after this long (seconds) when the app comes back to the front.
const RECHECK: f64 = 3600.0;
/// After a check that failed (offline, GitHub busy), try again this much sooner (seconds).
const RETRY_AFTER_FAILURE: f64 = 300.0;

/// A newer build on GitHub.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub build: i64,
    pub url: String,
    pub size: u64,
}

enum State {
    Idle,
    Checking(Receiver<Result<Option<Release>, String>>),
    Available(Release),
    Downloading(Release, Receiver<Result<(), String>>),
    /// Handed to Android, which shows its install confirmation.
    Installing,
    /// Android needs the user to allow WordCraft to install apps first (Settings is open).
    NeedsPermission(Release),
    Failed(String, Option<Release>),
    /// The user said Later to this build: don't offer it again this run.
    Dismissed,
}

pub struct Updater {
    state: State,
    /// This app's build number (versionCode), once known.
    current: Option<i64>,
    /// When the last check started (input time, seconds).
    last_check: Option<f64>,
    /// A newer build the user dismissed.
    dismissed: Option<i64>,
}

impl Default for Updater {
    fn default() -> Self {
        Self { state: State::Idle, current: None, last_check: None, dismissed: None }
    }
}

/// The build number in a release title: "WordCraft for Android (build 12)" → 12.
fn build_in_title(title: &str) -> Option<i64> {
    let start = title.rfind("(build ")? + "(build ".len();
    let rest = title.get(start..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The release from GitHub's JSON answer: its build and the APK asset.
fn parse_release(json: &str) -> Result<Release, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unexpected answer from GitHub ({e})"))?;
    let title = v.get("name").and_then(Value::as_str).unwrap_or_default();
    let build = build_in_title(title).ok_or_else(|| format!("no build number in the release title \"{title}\""))?;
    let asset = v
        .get("assets")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find(|x| x.get("name").and_then(Value::as_str).is_some_and(|n| n.ends_with(".apk"))))
        .ok_or("the release has no APK")?;
    let url = asset.get("browser_download_url").and_then(Value::as_str).ok_or("the APK has no download link")?.to_string();
    if !url.starts_with("https://") {
        return Err("the APK's download link isn't https".into());
    }
    let size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);
    Ok(Release { build, url, size })
}

/// This app's versionCode.
fn version_code() -> Option<i64> {
    with_activity("reading WordCraft's version", |env, activity| env.call_method(activity, "wcVersionCode", "()J", &[])?.j()).ok()
}

/// Ask GitHub for the latest build (blocking; on a worker thread).
fn fetch_latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/tags/{TAG}");
    let text = with_activity("checking for updates", |env, activity| {
        let u = env.new_string(&url)?;
        let s = env.call_method(activity, "wcHttpGet", "(Ljava/lang/String;)Ljava/lang/String;", &[JValue::Object(&u)])?.l()?;
        string_of(env, s)
    })?
    .ok_or("no answer from GitHub")?;
    parse_release(&text)
}

fn call_long(method: &str) -> i64 {
    with_activity("update progress", |env, activity| env.call_method(activity, method, "()J", &[])?.j()).unwrap_or(0)
}

/// The text of a Java exception message without the exception's class name.
fn plain(e: &str) -> String {
    match e.split_once(": java.") {
        Some((_, rest)) => rest.split_once(": ").map_or(rest, |(_, m)| m).to_string(),
        None => e.to_string(),
    }
}

impl Updater {
    /// Check now if it's time: at start-up, and after [`RECHECK`] when back in front.
    pub fn maybe_check(&mut self, now: f64) {
        if !matches!(self.state, State::Idle | State::Dismissed | State::Failed(_, None)) {
            return;
        }
        if self.last_check.is_some_and(|t| now - t < RECHECK) {
            return;
        }
        self.last_check = Some(now);
        if self.current.is_none() {
            self.current = version_code();
        }
        let Some(current) = self.current else {
            log::warn!("updates: this app's build number is unknown; not checking");
            return;
        };
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let r = fetch_latest().map(|r| (r.build > current).then_some(r));
            let _ = tx.send(r);
        });
        match spawned {
            Ok(_) => self.state = State::Checking(rx),
            Err(e) => log::warn!("updates: no thread for the check ({e})"),
        }
    }

    fn start_download(&mut self, release: Release) {
        let (tx, rx) = channel();
        let url = release.url.clone();
        let spawned = std::thread::Builder::new().name("update-download".into()).spawn(move || {
            let r = with_activity("downloading the update", |env, activity| {
                let u = env.new_string(&url)?;
                env.call_method(activity, "wcDownload", "(Ljava/lang/String;)V", &[JValue::Object(&u)])?;
                Ok(())
            });
            let _ = tx.send(r);
        });
        self.state = match spawned {
            Ok(_) => State::Downloading(release, rx),
            Err(e) => State::Failed(format!("Couldn't start the download ({e})"), Some(release)),
        };
    }

    fn install(&mut self, release: Release) {
        let r = with_activity("installing the update", |env, activity| env.call_method(activity, "wcInstall", "()Z", &[])?.z());
        self.state = match r {
            Ok(true) => State::Installing,
            Ok(false) => State::NeedsPermission(release),
            Err(e) => State::Failed(format!("Couldn't install the update: {}", plain(&e)), Some(release)),
        };
    }

    /// Pick up workers' answers and Android's install outcome.
    fn poll(&mut self) {
        match &self.state {
            State::Checking(rx) => match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Idle,
                Ok(Ok(Some(r))) if self.dismissed == Some(r.build) => self.state = State::Dismissed,
                Ok(Ok(Some(r))) => {
                    log::info!("updates: build {} is available (this is {:?})", r.build, self.current);
                    self.state = State::Available(r);
                }
                Ok(Ok(None)) => {
                    log::info!("updates: up to date (build {:?})", self.current);
                    self.state = State::Idle;
                }
                Ok(Err(e)) => {
                    // A failed background check stays quiet: offline is normal.
                    log::warn!("updates: check failed: {e}");
                    // Try again in a few minutes rather than an hour.
                    self.last_check = self.last_check.map(|t| t - RECHECK + RETRY_AFTER_FAILURE);
                    self.state = State::Idle;
                }
            },
            State::Downloading(release, rx) => match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Failed("The download stopped.".into(), Some(release.clone())),
                Ok(Ok(())) => {
                    let release = release.clone();
                    self.install(release);
                }
                Ok(Err(e)) => self.state = State::Failed(format!("The download failed: {}", plain(&e)), Some(release.clone())),
            },
            State::Installing => {
                let status = with_activity("update status", |env, activity| {
                    let s = env.call_method(activity, "wcTakeInstallStatus", "()Ljava/lang/String;", &[])?.l()?;
                    string_of(env, s)
                });
                match status.ok().flatten().as_deref() {
                    None => {}
                    // Android replaces the app and restarts it; this rarely shows.
                    Some("ok") => self.state = State::Idle,
                    Some(s) if s.starts_with("cancelled") => self.state = State::Idle,
                    Some(s) => self.state = State::Failed(format!("The update wasn't installed ({s})."), None),
                }
            }
            _ => {}
        }
    }

    /// Draw the update card, if there is something to say, at the bottom right of `area` (the
    /// app's part of the screen, clear of the system bars).
    pub fn show(&mut self, ctx: &egui::Context, area: egui::Rect) {
        self.poll();
        let card = match &self.state {
            State::Available(r) => Card::Available(r.clone()),
            State::Downloading(r, _) => Card::Downloading(r.build, call_long("wcDownloaded"), call_long("wcDownloadTotal")),
            State::Installing => Card::Installing,
            State::NeedsPermission(r) => Card::NeedsPermission(r.clone()),
            State::Failed(msg, retry) => Card::Failed(msg.clone(), retry.clone()),
            State::Idle | State::Checking(_) | State::Dismissed => return,
        };
        let current = self.current.unwrap_or(0);
        let mut action = None;
        egui::Area::new(egui::Id::new("wordcraft_android_update"))
            .order(egui::Order::Foreground)
            .pivot(Align2::RIGHT_BOTTOM)
            .fixed_pos(area.right_bottom() - egui::vec2(16.0, 40.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(12.0).show(ui, |ui| {
                    ui.set_max_width(320.0);
                    action = card.ui(ui, current);
                });
            });
        if matches!(self.state, State::Downloading(..) | State::Installing) {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        match action {
            Some(Action::Download(r)) => self.start_download(r),
            Some(Action::Install(r)) => self.install(r),
            Some(Action::Dismiss(build)) => {
                self.dismissed = Some(build);
                self.state = State::Dismissed;
            }
            None => {}
        }
    }
}

/// What the card shows (a copy of the state, so drawing doesn't hold it).
enum Card {
    Available(Release),
    /// Build, bytes so far, total bytes (0: unknown).
    Downloading(i64, i64, i64),
    Installing,
    NeedsPermission(Release),
    Failed(String, Option<Release>),
}

/// What a button on the card asked for.
enum Action {
    Download(Release),
    Install(Release),
    Dismiss(i64),
}

impl Card {
    fn ui(&self, ui: &mut egui::Ui, current: i64) -> Option<Action> {
        let mut action = None;
        match self {
            Card::Available(r) => {
                let mb = r.size as f64 / 1_048_576.0;
                ui.label(RichText::new(format!("WordCraft build {} is available", r.build)).strong());
                ui.label(format!("You have build {current}. The download is {mb:.0} MB."));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Update").clicked() {
                        action = Some(Action::Download(r.clone()));
                    }
                    if ui.button("Later").clicked() {
                        action = Some(Action::Dismiss(r.build));
                    }
                });
            }
            Card::Downloading(build, done, total) => {
                ui.label(RichText::new(format!("Downloading build {build}…")).strong());
                let frac = if *total > 0 { (*done as f32 / *total as f32).clamp(0.0, 1.0) } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(260.0));
            }
            Card::Installing => {
                ui.label(RichText::new("Installing the update…").strong());
                ui.label("Confirm in Android's install window. Save your work first: WordCraft closes and restarts with the new build.");
            }
            Card::NeedsPermission(r) => {
                ui.label(RichText::new("Allow WordCraft to install updates").strong());
                ui.label("Turn on \"Allow from this source\" in the settings that opened, then come back and tap Install.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Install").clicked() {
                        action = Some(Action::Install(r.clone()));
                    }
                    if ui.button("Later").clicked() {
                        action = Some(Action::Dismiss(r.build));
                    }
                });
            }
            Card::Failed(msg, retry) => {
                ui.label(RichText::new("Update").strong());
                ui.label(RichText::new(msg.as_str()).color(Color32::from_rgb(0xc4, 0x2b, 0x1c)));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if let Some(r) = retry
                        && ui.button("Try again").clicked()
                    {
                        action = Some(Action::Download(r.clone()));
                    }
                    if ui.button("Close").clicked() {
                        action = Some(Action::Dismiss(retry.as_ref().map_or(0, |r| r.build)));
                    }
                });
            }
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_titles_and_answers_parse() {
        assert_eq!(build_in_title("WordCraft for Android (build 12)"), Some(12));
        assert_eq!(build_in_title("WordCraft for Android"), None);
        let json = r#"{"name":"WordCraft for Android (build 7)","assets":[{"name":"wordcraft.apk","size":2048,"browser_download_url":"https://github.com/a/b/releases/download/android-latest/wordcraft.apk"}]}"#;
        assert_eq!(parse_release(json).map(|r| (r.build, r.size)), Ok((7, 2048)));
        assert!(parse_release(r#"{"name":"x (build 3)","assets":[]}"#).is_err());
        assert_eq!(plain("downloading the update: java.io.IOException: HTTP 404 from github.com"), "HTTP 404 from github.com");
    }
}
