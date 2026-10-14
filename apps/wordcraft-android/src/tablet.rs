//! What a tablet needs around the desktop UI.
//!
//! - **System bars.** NativeActivity draws under the status bar and the taskbar. Android reports
//!   the part of the window they leave free (the content rect); the app is laid out inside it,
//!   and egui is told the same insets so dialogs and menus stay clear too.
//! - **Touch.** vendor/winit tags each touch with its tool (finger, S Pen, eraser, mouse, palm).
//!   egui-winit would turn whichever touch came first into a pressed mouse pointer, so a finger
//!   dragging the page would select text and a resting palm would click. Here the pointer is
//!   rebuilt from the touches instead:
//!   - the S Pen (or a mouse) is the pointer: it clicks, selects text and drags, and while it is
//!     down fingers and palms don't count (palm rejection);
//!   - a finger tap clicks, a finger held still for half a second right-clicks (context menu),
//!     and a finger drag scrolls, sent as touchpad-style wheel events so the page coasts after a
//!     flick (`wordcraft_ui_egui::scroll`);
//!   - two fingers pan, and pinch to zoom (egui's multi-touch);
//!   - a palm never counts.
//! - **Keyboard.** With a keyboard cover or Bluetooth keyboard attached, text fields don't ask
//!   for the on-screen keyboard.
//! - **Clipboard.** What WordCraft copies goes on the system clipboard, and Ctrl+V pastes what
//!   other apps copied (`clipboard`).
//! - **Documents from other apps** open as they arrive (`incoming`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::{Event, Modifiers, MouseWheelUnit, PointerButton, Pos2, TouchPhase, Vec2};
use serde_json::json;
use winit::platform::android::activity::AndroidApp;
use wordcraft_ui_egui::WordApp;

use crate::incoming::Incoming;

/// A finger that moves further than this (points) before lifting scrolls instead of tapping.
const SLOP: f32 = 10.0;
/// A finger held this long (seconds) without moving right-clicks.
const LONG_PRESS: f64 = 0.5;

pub struct TabletApp {
    pub app: WordApp,
    android: AndroidApp,
    touches: TouchRouter,
    /// The insets last applied, in points.
    insets: egui::epaint::MarginF32,
    /// Look for a document sent by another app (`incoming`): at start-up and when the app comes
    /// back to the front.
    check_incoming: bool,
    focused: bool,
}

impl TabletApp {
    pub fn new(app: WordApp, android: AndroidApp) -> Self {
        Self { app, android, touches: TouchRouter::default(), insets: egui::epaint::MarginF32::default(), check_incoming: true, focused: true }
    }

    /// Open the document another app sent, if there is one. One that only exists in the other
    /// app (an attachment, a cloud file) is first kept as a file of WordCraft's own, so Save
    /// writes somewhere real.
    fn open_incoming(&mut self) {
        let path = match crate::incoming::take() {
            None => return,
            Some(Incoming::Path(path)) => path,
            Some(Incoming::Bytes { name, bytes }) => match keep_copy(&name, &bytes) {
                Ok(path) => {
                    self.app.status(format!("Saved a copy of {name} in {}", path.parent().map_or_else(String::new, |p| p.display().to_string())));
                    path.to_string_lossy().into_owned()
                }
                Err(e) => {
                    self.app.status(format!("Couldn't open {name}: {e}"));
                    return;
                }
            },
            Some(Incoming::Failed(e)) => {
                self.app.status(e);
                return;
            }
        };
        log::info!("opening {path}");
        match self.app.run("file.open", json!({"path": path})) {
            // Off the start screen, onto the document (or the "Save changes?" question).
            Ok(_) => self.app.ui.backstage = false,
            Err(e) => self.app.status(format!("Couldn't open {path}: {e}")),
        }
    }

    /// With a keyboard cover or Bluetooth keyboard attached, typing arrives as key presses, so a
    /// text field doesn't ask for the on-screen keyboard: Samsung shows its keyboard toolbar for
    /// that request, which takes room from the app (the content rect shrinks) and comes and goes
    /// as fields gain and lose focus. Without one, fields ask as usual.
    fn keep_soft_keyboard_away(&self, ctx: &egui::Context) {
        if matches!(self.android.config().keys_hidden(), ndk::configuration::KeysHidden::No) {
            ctx.output_mut(|o| o.ime = None);
        }
    }

    /// The window's edges covered by system bars, in points, from Android's content rect.
    fn system_bar_insets(&self, ctx: &egui::Context, raw: &egui::RawInput) -> Option<egui::epaint::MarginF32> {
        let ppp = ctx.pixels_per_point();
        let screen = raw.screen_rect?;
        if !(ppp.is_finite() && ppp > 0.0) {
            return None;
        }
        let r = self.android.content_rect();
        if r.right <= r.left || r.bottom <= r.top {
            return None;
        }
        let (w, h) = (screen.width() * ppp, screen.height() * ppp);
        let inset = |px: f32| (px / ppp).clamp(0.0, 400.0);
        Some(egui::epaint::MarginF32 {
            left: inset(r.left as f32),
            top: inset(r.top as f32),
            right: inset(w - r.right as f32),
            bottom: inset(h - r.bottom as f32),
        })
    }

    /// Ctrl+V pastes what other apps copied: egui-winit's own clipboard only knows WordCraft's
    /// copies (and sends no paste at all when it holds nothing).
    fn system_paste(raw: &mut egui::RawInput) {
        let ctrl_v = raw.events.iter().any(|e| {
            matches!(e, Event::Key { key: egui::Key::V, pressed: true, modifiers, .. } if modifiers.command && !modifiers.alt && !modifiers.shift)
        });
        let has_paste = raw.events.iter().any(|e| matches!(e, Event::Paste(_)));
        if !(ctrl_v || has_paste) {
            return;
        }
        let Some(text) = crate::clipboard::text() else { return };
        if has_paste {
            for e in &mut raw.events {
                if let Event::Paste(p) = e {
                    *p = text.clone();
                }
            }
        } else {
            raw.events.push(Event::Paste(text));
        }
    }
}

impl eframe::App for TabletApp {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(insets) = self.system_bar_insets(ctx, raw) {
            if insets != self.insets {
                log::info!("system bar insets (points): {insets:?}");
            }
            self.insets = insets;
            raw.safe_area_insets = Some(egui::SafeAreaInsets(insets));
        }
        if raw.focused && !self.focused {
            // Back in front, perhaps with another document to open.
            self.check_incoming = true;
        }
        if !raw.focused && self.focused {
            // Android may stop a backgrounded app without warning: keep the settings now.
            crate::save_prefs(&self.app);
        }
        self.focused = raw.focused;
        self.touches.route(&mut raw.events, raw.time.unwrap_or(0.0));
        if self.touches.waiting() {
            // A lift held back for the next frame, or a finger that may become a long press.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        Self::system_paste(raw);
        self.app.raw_input_hook(raw);
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.check_incoming) {
            self.open_incoming();
        }
        self.app.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let full = ui.max_rect();
        let m = self.insets;
        let inner = egui::Rect::from_min_max(full.min + egui::vec2(m.left, m.top), full.max - egui::vec2(m.right, m.bottom));
        if inner.width() < 100.0 || inner.height() < 100.0 {
            self.app.ui(ui);
        } else {
            // Behind the status bar: the title bar's colour, so the bar reads as part of the app.
            let bar = wordcraft_ui_egui::theme::Tokens::get(ui.ctx()).title_bar;
            ui.painter().rect_filled(full, 0.0, bar);
            ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                ui.set_clip_rect(inner);
                self.app.ui(ui);
            });
        }
        // What WordCraft copied this frame goes on the system clipboard too.
        let copied = ui.ctx().output(|o| {
            o.commands.iter().rev().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
        });
        if let Some(text) = copied {
            crate::clipboard::set_text(&text);
        }
        self.keep_soft_keyboard_away(ui.ctx());
    }

    fn on_exit(&mut self) {
        crate::save_prefs(&self.app);
    }
}

/// Keep the bytes of a document that only exists in another app as a file WordCraft can open
/// and save: in Documents/WordCraft when shared storage is accessible, else in the app's own
/// folder. An existing file of that name is never replaced ("Report (2).docx").
fn keep_copy(name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let shared = PathBuf::from("/storage/emulated/0/Documents/WordCraft");
    let dir = if crate::file_access::granted() { shared } else { crate::app_dir().map(|d| d.join("Received")).ok_or("no app folder")? };
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = free_name(&dir, &safe_file_name(name));
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// `name` without folders or characters a file name can't hold; never empty.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String =
        base.chars().map(|c| if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { "Document.docx".to_string() } else { cleaned }
}

/// `dir/name`, or `dir/stem (2).ext` and so on when that file already exists.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.to_string());
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// Which tool a touch is, from the bits vendor/winit puts above the pointer id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Finger,
    Pen,
    Mouse,
    Palm,
}

fn tool_of(id: u64) -> Tool {
    match (id >> 32) & 0xff {
        2 | 4 => Tool::Pen, // stylus, eraser end
        3 => Tool::Mouse,
        5 => Tool::Palm,
        _ => Tool::Finger, // finger, or unknown
    }
}

/// A touch that is down.
#[derive(Clone, Copy, Debug)]
struct Down {
    tool: Tool,
    device: egui::TouchDeviceId,
    pos: Pos2,
}

/// What fingers are doing (no pen or mouse down).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fingers {
    Idle,
    /// One finger down, not moved past [`SLOP`] yet: a tap, a long press or a scroll.
    Pending {
        id: u64,
        start: Pos2,
        since: f64,
    },
    /// One finger scrolling; `last` is where the last wheel event was sent from.
    Scrolling {
        id: u64,
        last: Pos2,
    },
    /// Two or more fingers: pan by their centre (and egui pinch-zooms).
    Multi {
        last: Option<Pos2>,
    },
    /// The gesture was used up (a long press fired): ignore the rest until every finger lifts.
    Spent,
}

#[derive(Default)]
struct TouchRouter {
    /// Touches down now, by id (hidden ones too).
    down: HashMap<u64, Down>,
    /// Touches whose `Touch` events egui doesn't see (a palm; fingers while the pen is down).
    hidden: HashSet<u64>,
    /// The pen or mouse touch that drives egui's pointer.
    owner: Option<u64>,
    fingers: FingersState,
    /// Modifier keys as last reported (pointer and wheel events carry them).
    modifiers: Modifiers,
    /// Pointer events for the next frame, and for the one after. A quick tap often arrives in one
    /// frame (down, up), and egui finds what is under the pointer once per frame, after the
    /// frame's events: with the lift and the pointer's leaving in the same frame as the press,
    /// nothing was under it and the tap clicked nothing. So a lift waits for the next frame when
    /// its press came in this one, and the pointer leaves one frame after the lift.
    next: Vec<Event>,
    after: Vec<Event>,
    /// Touches that started in the frame being routed.
    started_now: HashSet<u64>,
}

/// [`Fingers`] with a default.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FingersState(Fingers);

impl Default for FingersState {
    fn default() -> Self {
        Self(Fingers::Idle)
    }
}

impl TouchRouter {
    /// Something waits for a later frame: held-back pointer events, or a finger that may turn
    /// into a long press.
    fn waiting(&self) -> bool {
        !self.next.is_empty() || !self.after.is_empty() || matches!(self.fingers.0, Fingers::Pending { .. })
    }

    fn pen_down(&self) -> bool {
        self.down.values().any(|d| matches!(d.tool, Tool::Pen | Tool::Mouse))
    }

    fn finger_ids(&self) -> Vec<u64> {
        self.down.iter().filter(|(id, d)| d.tool == Tool::Finger && !self.hidden.contains(id)).map(|(id, _)| *id).collect()
    }

    fn centre(&self) -> Option<Pos2> {
        let ids = self.finger_ids();
        if ids.is_empty() {
            return None;
        }
        let sum = ids.iter().filter_map(|id| self.down.get(id)).fold(Vec2::ZERO, |s, d| s + d.pos.to_vec2());
        Some((sum / ids.len() as f32).to_pos2())
    }

    fn wheel(&self, out: &mut Vec<Event>, delta: Vec2, phase: TouchPhase) {
        out.push(Event::MouseWheel { unit: MouseWheelUnit::Point, delta, phase, modifiers: self.modifiers });
    }

    /// A click of `button` at `pos`: the press now, the release next frame (or now, when the
    /// press is from an earlier frame), and the pointer leaves a frame after that.
    fn click(&mut self, out: &mut Vec<Event>, pos: Pos2, button: PointerButton) {
        out.push(Event::PointerMoved(pos));
        out.push(Event::PointerButton { pos, button, pressed: true, modifiers: self.modifiers });
        self.next.push(Event::PointerButton { pos, button, pressed: false, modifiers: self.modifiers });
        self.after.push(Event::PointerGone);
    }

    /// The owner's lift at `pos`: now (or next frame if it was pressed in this one), and the
    /// pointer leaves a frame later.
    fn lift(&mut self, id: u64, pos: Pos2, out: &mut Vec<Event>) {
        let release = Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: self.modifiers };
        if self.started_now.contains(&id) {
            self.next.push(release);
            self.after.push(Event::PointerGone);
        } else {
            out.push(release);
            self.next.push(Event::PointerGone);
        }
    }

    /// Stop egui seeing a touch it has seen start: it ends as cancelled.
    fn hide(&mut self, id: u64, out: &mut Vec<Event>) {
        if !self.hidden.insert(id) {
            return;
        }
        if let Some(d) = self.down.get(&id) {
            out.push(Event::Touch { device_id: d.device, id: egui::TouchId(id), phase: TouchPhase::Cancel, pos: d.pos, force: None });
        }
    }

    /// End whatever the fingers were doing (a scroll ends, so the page may coast).
    fn end_fingers(&mut self, out: &mut Vec<Event>) {
        if matches!(self.fingers.0, Fingers::Scrolling { .. } | Fingers::Multi { last: Some(_) }) {
            self.wheel(out, Vec2::ZERO, TouchPhase::End);
        }
        self.fingers.0 = Fingers::Idle;
    }

    /// Replace egui-winit's touch-to-pointer translation (module docs). `now` is the input time.
    fn route(&mut self, events: &mut Vec<Event>, now: f64) {
        // What earlier frames held back comes first.
        let mut out = std::mem::take(&mut self.next);
        self.next = std::mem::take(&mut self.after);
        self.started_now.clear();
        // egui-winit puts a touch's pointer events right after its `Touch` event: those are
        // rebuilt here. Other pointer events come from a mouse (vendor/winit) and stay.
        let mut after_touch = false;
        for event in std::mem::take(events) {
            match event {
                Event::Touch { device_id, id, phase, pos, force } => {
                    after_touch = true;
                    self.touch(device_id, id.0, phase, pos, now, &mut out);
                    let ended = matches!(phase, TouchPhase::End | TouchPhase::Cancel);
                    let hidden = if ended { self.hidden.remove(&id.0) } else { self.hidden.contains(&id.0) };
                    if !hidden {
                        out.push(Event::Touch { device_id, id, phase, pos, force });
                    }
                }
                Event::PointerMoved(_) | Event::PointerButton { .. } | Event::PointerGone if after_touch => {
                    if let Event::PointerButton { modifiers, .. } = event {
                        self.modifiers = modifiers;
                    }
                }
                other => {
                    after_touch = false;
                    if let Event::Key { modifiers, .. } | Event::PointerButton { modifiers, .. } = &other {
                        self.modifiers = *modifiers;
                    }
                    out.push(other);
                }
            }
        }
        // A finger held still long enough: a right-click where it rests (the context menu).
        if let Fingers::Pending { start, since, .. } = self.fingers.0
            && now - since >= LONG_PRESS
        {
            self.click(&mut out, start, PointerButton::Secondary);
            self.fingers.0 = Fingers::Spent;
        }
        *events = out;
    }

    fn touch(&mut self, device: egui::TouchDeviceId, id: u64, phase: TouchPhase, pos: Pos2, now: f64, out: &mut Vec<Event>) {
        let tool = tool_of(id);
        match phase {
            TouchPhase::Start => {
                let pen_was_down = self.pen_down();
                self.down.insert(id, Down { tool, device, pos });
                if tool == Tool::Palm || (tool == Tool::Finger && pen_was_down) {
                    // Not shown to egui at all: nothing to cancel.
                    self.hidden.insert(id);
                    return;
                }
                match tool {
                    Tool::Pen | Tool::Mouse => {
                        // The pen takes over: whatever the fingers were doing ends, and they stop
                        // counting (no pinch while writing).
                        self.end_fingers(out);
                        for finger in self.finger_ids() {
                            self.hide(finger, out);
                        }
                        if self.owner.is_none() {
                            self.owner = Some(id);
                            self.started_now.insert(id);
                            // A new press: the last touch's pointer doesn't leave after all.
                            out.retain(|e| !matches!(e, Event::PointerGone));
                            self.next.retain(|e| !matches!(e, Event::PointerGone));
                            self.after.retain(|e| !matches!(e, Event::PointerGone));
                            out.push(Event::PointerMoved(pos));
                            out.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: self.modifiers });
                        }
                    }
                    Tool::Finger => {
                        let fingers = self.finger_ids().len();
                        self.fingers.0 = match self.fingers.0 {
                            Fingers::Idle if fingers == 1 => {
                                // Hover there, so wheel events reach what is under the finger.
                                out.retain(|e| !matches!(e, Event::PointerGone));
                                self.next.retain(|e| !matches!(e, Event::PointerGone));
                                self.after.retain(|e| !matches!(e, Event::PointerGone));
                                out.push(Event::PointerMoved(pos));
                                Fingers::Pending { id, start: pos, since: now }
                            }
                            Fingers::Spent => Fingers::Spent,
                            Fingers::Scrolling { .. } => {
                                self.wheel(out, Vec2::ZERO, TouchPhase::End);
                                Fingers::Multi { last: None }
                            }
                            _ => Fingers::Multi { last: None },
                        };
                    }
                    Tool::Palm => {}
                }
            }
            TouchPhase::Move => {
                if let Some(d) = self.down.get_mut(&id) {
                    d.pos = pos;
                }
                if self.hidden.contains(&id) {
                    return;
                }
                if self.owner == Some(id) {
                    out.push(Event::PointerMoved(pos));
                    return;
                }
                if tool != Tool::Finger {
                    return;
                }
                match self.fingers.0 {
                    Fingers::Pending { id: f, start, .. } if f == id && (pos - start).length() > SLOP => {
                        self.wheel(out, pos - start, TouchPhase::Start);
                        out.push(Event::PointerMoved(pos));
                        self.fingers.0 = Fingers::Scrolling { id, last: pos };
                    }
                    Fingers::Scrolling { id: f, last } if f == id => {
                        self.wheel(out, pos - last, TouchPhase::Move);
                        out.push(Event::PointerMoved(pos));
                        self.fingers.0 = Fingers::Scrolling { id, last: pos };
                    }
                    Fingers::Multi { last } => {
                        if let Some(c) = self.centre() {
                            match last {
                                Some(l) => self.wheel(out, c - l, TouchPhase::Move),
                                None => self.wheel(out, Vec2::ZERO, TouchPhase::Start),
                            }
                            out.push(Event::PointerMoved(c));
                            self.fingers.0 = Fingers::Multi { last: Some(c) };
                        }
                    }
                    _ => {}
                }
            }
            TouchPhase::End | TouchPhase::Cancel => {
                let was_hidden = self.hidden.contains(&id);
                self.down.remove(&id);
                if self.owner == Some(id) {
                    self.owner = None;
                    if phase == TouchPhase::End {
                        self.lift(id, pos, out);
                    } else {
                        out.push(Event::PointerGone);
                    }
                    return;
                }
                if tool != Tool::Finger || was_hidden {
                    return;
                }
                let left = self.finger_ids().iter().filter(|f| **f != id).count();
                match self.fingers.0 {
                    Fingers::Pending { id: f, start, .. } if f == id => {
                        self.fingers.0 = Fingers::Idle;
                        if phase == TouchPhase::End {
                            self.click(out, start, PointerButton::Primary);
                        }
                    }
                    Fingers::Scrolling { id: f, .. } if f == id => self.end_fingers(out),
                    Fingers::Multi { .. } if left == 0 => self.end_fingers(out),
                    // One finger of a pan lifted: keep panning from the new centre.
                    Fingers::Multi { last: Some(_) } => {
                        self.wheel(out, Vec2::ZERO, TouchPhase::End);
                        self.fingers.0 = Fingers::Multi { last: None };
                    }
                    Fingers::Spent if left == 0 => self.fingers.0 = Fingers::Idle,
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(id: u64, phase: TouchPhase, x: f32, y: f32) -> Event {
        Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(id), phase, pos: Pos2::new(x, y), force: None }
    }

    const PEN: u64 = 2 << 32;

    #[test]
    fn a_finger_tap_clicks_and_a_drag_scrolls() {
        let mut r = TouchRouter::default();
        let mut ev = vec![touch(1, TouchPhase::Start, 10.0, 10.0), touch(1, TouchPhase::End, 10.0, 10.0)];
        r.route(&mut ev, 0.0);
        assert!(ev.iter().any(|e| matches!(e, Event::PointerButton { pressed: true, button: PointerButton::Primary, .. })));
        let mut ev = vec![];
        r.route(&mut ev, 0.02);
        assert!(ev.iter().any(|e| matches!(e, Event::PointerButton { pressed: false, .. })));

        let mut r = TouchRouter::default();
        let mut ev = vec![touch(1, TouchPhase::Start, 10.0, 10.0), touch(1, TouchPhase::Move, 10.0, 60.0), touch(1, TouchPhase::End, 10.0, 60.0)];
        r.route(&mut ev, 0.0);
        assert!(!ev.iter().any(|e| matches!(e, Event::PointerButton { .. })));
        let scrolled: f32 = ev.iter().filter_map(|e| if let Event::MouseWheel { delta, .. } = e { Some(delta.y) } else { None }).sum();
        assert_eq!(scrolled, 50.0);
        assert!(ev.iter().any(|e| matches!(e, Event::MouseWheel { phase: TouchPhase::End, .. })));
    }

    #[test]
    fn a_held_finger_right_clicks() {
        let mut r = TouchRouter::default();
        let mut ev = vec![touch(1, TouchPhase::Start, 10.0, 10.0)];
        r.route(&mut ev, 0.0);
        let mut ev = vec![];
        r.route(&mut ev, 0.6);
        assert!(ev.iter().any(|e| matches!(e, Event::PointerButton { pressed: true, button: PointerButton::Secondary, .. })));
        let mut ev = vec![touch(1, TouchPhase::End, 10.0, 10.0)];
        r.route(&mut ev, 0.7);
        assert!(!ev.iter().any(|e| matches!(e, Event::PointerButton { pressed: true, button: PointerButton::Primary, .. })));
    }

    #[test]
    fn the_pen_drags_and_a_palm_is_ignored() {
        let mut r = TouchRouter::default();
        let mut ev =
            vec![touch(5 << 32, TouchPhase::Start, 50.0, 50.0), touch(PEN, TouchPhase::Start, 10.0, 10.0), touch(PEN, TouchPhase::Move, 30.0, 10.0)];
        r.route(&mut ev, 0.0);
        assert!(ev.iter().any(|e| matches!(e, Event::PointerButton { pressed: true, pos, .. } if pos.x == 10.0)));
        assert!(ev.iter().any(|e| matches!(e, Event::PointerMoved(p) if p.x == 30.0)));
        assert!(!ev.iter().any(|e| matches!(e, Event::Touch { id, .. } if id.0 == 5 << 32)));
        assert!(!ev.iter().any(|e| matches!(e, Event::MouseWheel { .. })));
    }

    #[test]
    fn copies_of_incoming_documents_get_free_names() {
        assert_eq!(safe_file_name("../a/b:c.docx"), "b_c.docx");
        assert_eq!(safe_file_name(""), "Document.docx");
        let dir = std::env::temp_dir().join(format!("wordcraft-android-free-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("R.docx"), b"x").unwrap();
        assert_eq!(free_name(&dir, "R.docx"), dir.join("R (2).docx"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
