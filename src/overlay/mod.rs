//! In-game overlay: telemetry boxes drawn into the game's own frames.
//!
//! * Windows build (game under Proton or on Windows): hooks
//!   `IDXGISwapChain::Present` and draws with Direct3D 11 ([`d3d11`]).
//! * Linux build (native game): hooks `glXSwapBuffers` and draws with
//!   OpenGL ([`glx`]).
//!
//! Every telemetry item is its own box at its own position. Both backends
//! call [`Core::frame`] once per presented frame with the back buffer size
//! and the polled mouse/keyboard state; the core handles edit mode (dragging
//! boxes, the control panel), layout and rasterisation (software, via
//! [`render`]) and returns a list of [`Sprite`]s (premultiplied RGBA images
//! at positions). Nothing is hooked unless `overlay.enabled = true` in the
//! settings file.
//!
//! Threads: the game thread only hands over snapshots ([`Overlay::feed`],
//! never blocking) and drains log lines; everything else runs on the game's
//! render thread inside the hook.

pub mod items;
pub mod render;

#[cfg(windows)]
mod d3d11;
#[cfg(windows)]
mod dinput;
#[cfg(target_os = "linux")]
mod glx;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tiny_skia::Pixmap;

use crate::settings::{self, OverlaySettings};
use crate::snapshot::{self, Envelope, SessionMeta, Snapshot};
use items::ITEMS;
use render::{Button, Fonts, Hit, Scene, SliderKind};

/// How often new telemetry is turned into a new panel image.
const DATA_INTERVAL: Duration = Duration::from_millis(50);
/// How often the settings file is checked for edits.
const CONFIG_INTERVAL: Duration = Duration::from_secs(1);

/// What the game thread hands over.
pub(crate) struct Feed {
    snap: Snapshot,
    at: Instant,
}

/// State shared between the game thread and the render thread.
pub(crate) struct Shared {
    feed: Mutex<Option<Feed>>,
    logs: Mutex<Vec<String>>,
    meta: SessionMeta,
}

impl Shared {
    pub(crate) fn log(&self, msg: impl Into<String>) {
        let mut logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        if logs.len() < 100 {
            logs.push(msg.into());
        }
    }
}

/// Polled input for one frame, in back-buffer pixels.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Input {
    /// Mouse position, `None` if unknown or the game window is not focused.
    pub pointer: Option<(f32, f32)>,
    pub left_button: bool,
    /// Ctrl+Shift+Backspace currently held.
    pub hotkey: bool,
}

/// Sprite id of the edit-mode control panel (boxes use their item index).
pub(crate) const EDITOR_ID: u32 = 1000;
/// Sprite id of the edit-mode cursor.
pub(crate) const CURSOR_ID: u32 = 1001;
const ALERT_TOP_ID: u32 = 1002;
const ALERT_BOTTOM_ID: u32 = 1003;
const ALERT_LEFT_ID: u32 = 1004;
const ALERT_RIGHT_ID: u32 = 1005;
const ALERT_ALPHA: u8 = 191;

/// A premultiplied-RGBA image to draw at a screen position. Sprites are
/// drawn in order (later on top). `version` changes whenever `image` does,
/// so backends keep one texture per `id` and re-upload only on change.
pub(crate) struct Sprite<'a> {
    pub id: u32,
    pub version: u64,
    pub x: i32,
    pub y: i32,
    pub image: &'a Pixmap,
}

pub(crate) struct Frame<'a> {
    pub sprites: Vec<Sprite<'a>>,
    /// Edit mode is on after this frame.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub edit: bool,
}

/// A rendered element and the image painted from it.
struct Cached {
    scene: Scene,
    image: Pixmap,
    version: u64,
}

#[derive(Clone, Debug, PartialEq)]
enum Target {
    Box(String),
    Editor(Option<Hit>),
}

#[derive(Clone, Debug, PartialEq)]
enum Drag {
    Box { id: String, grab: (f32, f32) },
    Editor { grab: (f32, f32) },
    Slider { id: String, kind: SliderKind, track_x: f32, track_w: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Area {
    fn contains(&self, (px, py): (f32, f32)) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

/// Platform-independent overlay logic, owned by the render thread.
pub(crate) struct Core {
    shared: Arc<Shared>,
    path: Option<PathBuf>,
    cfg: OverlaySettings,
    fonts: Fonts,
    edit: bool,
    hotkey_was: bool,
    left_was: bool,
    drag: Option<Drag>,
    selected_item: Option<String>,
    hover: Option<Target>,
    data: Option<Value>,
    data_at: Option<Instant>,
    last_data: Option<Instant>,
    cfg_mtime: Option<SystemTime>,
    last_cfg_check: Instant,
    bb: (u32, u32),
    boxes: HashMap<String, Cached>,
    floors: HashMap<String, f32>,
    editor: Option<Cached>,
    cursor: Option<Cached>,
    alert_top: Option<Cached>,
    alert_side: Option<Cached>,
    started_at: Instant,
    cursor_k: f32,
    next_version: u64,
    /// What was drawn last frame, bottom to top, for hit testing.
    layout: Vec<(Target, Area)>,
}

fn mtime(path: &Option<PathBuf>) -> Option<SystemTime> {
    path.as_ref().and_then(|p| std::fs::metadata(p).ok()).and_then(|m| m.modified().ok())
}

/// Top-left corner that keeps a `w × h` element fully on a `bb` screen.
fn clamp_pos(x: f32, y: f32, w: u32, h: u32, bb: (u32, u32)) -> (f32, f32) {
    (x.clamp(0.0, (bb.0 as f32 - w as f32).max(0.0)), y.clamp(0.0, (bb.1 as f32 - h as f32).max(0.0)))
}

impl Core {
    pub(crate) fn new(shared: Arc<Shared>, cfg: OverlaySettings, path: Option<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            shared,
            cfg_mtime: mtime(&path),
            path,
            cfg,
            fonts: Fonts::embedded()?,
            edit: false,
            hotkey_was: false,
            left_was: false,
            drag: None,
            selected_item: None,
            hover: None,
            data: None,
            data_at: None,
            last_data: None,
            last_cfg_check: Instant::now(),
            bb: (0, 0),
            boxes: HashMap::new(),
            floors: HashMap::new(),
            editor: None,
            cursor: None,
            alert_top: None,
            alert_side: None,
            started_at: Instant::now(),
            cursor_k: 0.0,
            next_version: 1,
            layout: Vec::new(),
        })
    }

    fn bump(&mut self) -> u64 {
        self.next_version += 1;
        self.next_version
    }

    /// Centre of the control panel (or the screen) in screen pixels: where a
    /// fresh edit-mode cursor starts.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn panel_center(&self) -> (f32, f32) {
        self.layout
            .iter()
            .find(|(t, _)| matches!(t, Target::Editor(_)))
            .map_or((self.bb.0 as f32 / 2.0, self.bb.1 as f32 / 2.0), |(_, a)| (a.x + a.w / 2.0, a.y + a.h / 2.0))
    }

    fn save(&mut self) {
        let Some(path) = self.path.clone() else { return };
        match settings::save_overlay(&path, &self.cfg) {
            Ok(()) => self.cfg_mtime = mtime(&self.path),
            Err(e) => self.shared.log(format!("cannot save {}: {e}", path.display())),
        }
    }

    /// Picks up edits to the settings file made while the game runs.
    fn reload_if_changed(&mut self) {
        if self.last_cfg_check.elapsed() < CONFIG_INTERVAL || self.drag.is_some() {
            return;
        }
        self.last_cfg_check = Instant::now();
        let m = mtime(&self.path);
        if m == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = m;
        let Some(text) = self.path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) else { return };
        let mut warnings = Vec::new();
        let s = settings::parse(&text, &mut warnings);
        for w in warnings {
            self.shared.log(w);
        }
        if s.overlay != self.cfg {
            self.shared.log("overlay settings reloaded from the settings file");
            self.cfg = s.overlay;
            self.floors.clear();
        }
    }

    fn poll_data(&mut self) {
        if self.last_data.is_some_and(|t| t.elapsed() < DATA_INTERVAL) {
            return;
        }
        self.last_data = Some(Instant::now());
        let snap = {
            let feed = self.shared.feed.lock().unwrap_or_else(PoisonError::into_inner);
            match &*feed {
                Some(f) if Some(f.at) != self.data_at => Some((f.snap, f.at)),
                _ => None,
            }
        };
        if let Some((snap, at)) = snap {
            let env = Envelope {
                seq: 0,
                ts_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
                age_ms: at.elapsed().as_millis() as u64,
                alive: true,
            };
            let bytes = snapshot::encode(&snap, &self.shared.meta, env);
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                self.data = Some(v);
            }
            self.data_at = Some(at);
        }
    }

    fn set_edit(&mut self, on: bool) {
        self.edit = on;
        self.drag = None;
        self.hover = None;
        if !on {
            self.save();
        }
    }

    fn on_button(&mut self, b: Button) {
        let step = |v: f32, d: f32| ((v + d) * 10.0).round() / 10.0;
        match b {
            Button::Smaller => {
                self.cfg.scale = step(self.cfg.scale, -0.1);
                for b in &mut self.cfg.boxes {
                    b.scale = step(b.scale, -0.1);
                }
            }
            Button::Bigger => {
                self.cfg.scale = step(self.cfg.scale, 0.1);
                for b in &mut self.cfg.boxes {
                    b.scale = step(b.scale, 0.1);
                }
            }
            Button::Fainter => {
                self.cfg.opacity = step(self.cfg.opacity, -0.1);
                for b in &mut self.cfg.boxes {
                    b.opacity = step(b.opacity, -0.1);
                }
            }
            Button::Stronger => {
                self.cfg.opacity = step(self.cfg.opacity, 0.1);
                for b in &mut self.cfg.boxes {
                    b.opacity = step(b.opacity, 0.1);
                }
            }
            Button::IdleMode => self.cfg.hide_when_idle = !self.cfg.hide_when_idle,
            Button::SpeedAlert => self.cfg.exceed_speed_limit_indicator = !self.cfg.exceed_speed_limit_indicator,
            Button::HideAll => self.cfg.visible = !self.cfg.visible,
            Button::Done => {
                self.set_edit(false);
                return;
            }
        }
        self.cfg.sanitize();
        self.floors.clear();
        self.save();
    }

    fn area_of(&self, target: &Target) -> Option<Area> {
        self.layout.iter().find(|(t, _)| t == target).map(|(_, a)| *a)
    }

    /// Topmost element under `p`.
    fn target_at(&self, p: (f32, f32)) -> Option<Target> {
        let (t, a) = self.layout.iter().rev().find(|(_, a)| a.contains(p))?;
        Some(match t {
            Target::Editor(_) => Target::Editor(self.editor.as_ref().and_then(|e| e.scene.hit(p.0 - a.x, p.1 - a.y))),
            other => other.clone(),
        })
    }

    /// Where a box that is switched on appears: beside the control panel, at
    /// the height of the clicked row.
    fn new_box_position(&self, row_y: f32) -> (i32, i32) {
        let k = self.k();
        let guess_w = 200.0 * k;
        let (x, y) = match self.area_of(&Target::Editor(None)) {
            Some(e) if e.x + e.w + 12.0 + guess_w <= self.bb.0 as f32 => (e.x + e.w + 12.0, row_y),
            Some(e) => ((e.x - 12.0 - guess_w).max(0.0), row_y),
            None => (40.0, row_y),
        };
        (x.round() as i32, (y - render::BOX_H * k / 2.0).max(0.0).round() as i32)
    }

    fn slider_range(&self, index: usize, kind: SliderKind) -> Option<(f32, f32)> {
        let area = self.area_of(&Target::Editor(None))?;
        let hit = Hit::Slider(index, kind);
        let (rect, _) = self.editor.as_ref()?.scene.hits.iter().find(|(_, h)| *h == hit)?;
        Some((area.x + rect.x, rect.w))
    }

    fn set_slider_value(&mut self, id: &str, kind: SliderKind, pointer_x: f32, track_x: f32, track_w: f32) {
        let fraction = ((pointer_x - track_x) / track_w).clamp(0.0, 1.0);
        match kind {
            SliderKind::Scale => self.cfg.set_box_scale(id, ((0.5 + fraction * 2.5) * 100.0).round() / 100.0),
            SliderKind::Opacity => self.cfg.set_box_opacity(id, ((0.15 + fraction * 0.85) * 100.0).round() / 100.0),
        }
        self.floors.remove(id);
    }

    fn handle_input(&mut self, input: Input) {
        let hotkey_edge = input.hotkey && !self.hotkey_was;
        self.hotkey_was = input.hotkey;
        if hotkey_edge {
            let on = !self.edit;
            self.set_edit(on);
            self.shared.log(format!("overlay edit mode {}", if on { "on" } else { "off" }));
        }
        let press = input.left_button && !self.left_was;
        self.left_was = input.left_button;
        if !self.edit {
            return;
        }
        let Some(p) = input.pointer else {
            self.hover = None;
            return;
        };

        if let Some(drag) = self.drag.clone() {
            if input.left_button {
                match &drag {
                    Drag::Box { id, grab } => {
                        let x = (p.0 - grab.0).round() as i32;
                        let y = (p.1 - grab.1).round() as i32;
                        self.cfg.set_position(id, x, y);
                    }
                    Drag::Editor { grab } => {
                        let x = (p.0 - grab.0).round() as i32;
                        let y = (p.1 - grab.1).round() as i32;
                        self.cfg.editor = Some((x.max(0), y.max(0)));
                    }
                    Drag::Slider { id, kind, track_x, track_w } => self.set_slider_value(id, *kind, p.0, *track_x, *track_w),
                }
            } else {
                self.drag = None;
                self.save();
            }
            return;
        }

        self.hover = self.target_at(p);
        if !press {
            return;
        }
        match self.hover.clone() {
            Some(Target::Editor(Some(Hit::Toggle(i)))) => {
                let at = self.new_box_position(p.1);
                self.cfg.toggle(ITEMS[i].id, at);
                self.save();
            }
            Some(Target::Editor(Some(Hit::Select(i)))) => {
                let id = ITEMS[i].id.to_owned();
                self.selected_item = Some(id.clone());
                let at = self.new_box_position(p.1);
                self.cfg.ensure_box(&id, at);
                self.save();
            }
            Some(Target::Editor(Some(Hit::Slider(i, kind)))) => {
                let id = ITEMS[i].id.to_owned();
                let at = self.new_box_position(p.1);
                self.cfg.ensure_box(&id, at);
                if let Some((track_x, track_w)) = self.slider_range(i, kind) {
                    self.selected_item = Some(id.clone());
                    self.drag = Some(Drag::Slider { id: id.clone(), kind, track_x, track_w });
                    self.set_slider_value(&id, kind, p.0, track_x, track_w);
                }
            }
            Some(Target::Editor(Some(Hit::Timezone(direction)))) => {
                if let Some(id) = self.selected_item.clone() {
                    let current: Option<String> = self.cfg.boxes.iter().find(|b| b.id == id).map(|b| b.timezone.clone());
                    if let Some(current) = current {
                        self.cfg.set_box_timezone(&id, items::next_timezone(&current, direction));
                        self.save();
                    }
                }
            }
            Some(Target::Editor(Some(Hit::LabelToggle(i)))) => {
                let id = ITEMS[i].id.to_owned();
                let at = self.new_box_position(p.1);
                self.cfg.ensure_box(&id, at);
                self.cfg.toggle_box_label(&id);
                self.floors.remove(&id);
                self.selected_item = Some(id);
                self.save();
            }
            Some(Target::Editor(Some(Hit::Button(b)))) => self.on_button(b),
            Some(Target::Editor(None)) => {
                if let Some(a) = self.area_of(&Target::Editor(None)) {
                    self.drag = Some(Drag::Editor { grab: (p.0 - a.x, p.1 - a.y) });
                }
            }
            Some(Target::Box(id)) => {
                if let Some(a) = self.area_of(&Target::Box(id.clone())) {
                    self.selected_item = Some(id.clone());
                    self.drag = Some(Drag::Box { id, grab: (p.0 - a.x, p.1 - a.y) });
                }
            }
            None => {}
        }
    }

    fn k(&self) -> f32 {
        self.cfg.scale * self.bb.1 as f32 / 1080.0
    }

    /// Repaints `scene` into `slot` only if it changed.
    fn update(slot: &mut Option<Cached>, scene: Scene, fonts: &mut Fonts, next: &mut u64) {
        if slot.as_ref().is_some_and(|c| c.scene == scene) {
            return;
        }
        if let Some(image) = render::paint(&scene, fonts, 1.0, scene.w, scene.h) {
            *next += 1;
            *slot = Some(Cached { scene, image, version: *next });
        }
    }

    /// One presented frame. `bb` = back buffer size in pixels.
    pub(crate) fn frame(&mut self, bb: (u32, u32), input: Input) -> Frame<'_> {
        self.reload_if_changed();
        if !self.cfg.enabled || bb.0 == 0 || bb.1 == 0 {
            self.layout.clear();
            return Frame { sprites: Vec::new(), edit: false };
        }
        if bb != self.bb {
            self.bb = bb;
            self.floors.clear();
        }
        self.handle_input(input);
        self.poll_data();
        // Boxes use the user's size; the control panel and cursor only follow
        // the screen resolution, so Size −/+ never resizes the panel itself.
        let ui_k = bb.1 as f32 / 1080.0;
        let edit = self.edit;

        // boxes
        let truck = self.data.as_ref().and_then(|d| d.get("truck")).and_then(Value::as_bool).unwrap_or(false);
        // "Hide all" hides everything in game; edit mode still previews the
        // boxes (faded) so they can be arranged.
        let show_boxes = edit || (self.cfg.visible && (truck || !self.cfg.hide_when_idle));
        let ghost = edit && !self.cfg.visible;
        let mut layout = Vec::new();
        let mut order: Vec<(u32, String, (i32, i32))> = Vec::new();
        if show_boxes {
            for b in self.cfg.boxes.clone().into_iter().filter(|b| !b.hidden) {
                let Some(idx) = items::index_of(&b.id) else { continue };
                let item = &ITEMS[idx];
                let show = match (&self.data, truck || edit) {
                    (Some(d), true) if b.id == "real_time" => items::real_time_in_timezone(d, &b.timezone),
                    (Some(d), true) => (item.show)(d),
                    _ => items::Show::Text("—".into(), items::Tone::Dim),
                };
                let target = Target::Box(b.id.clone());
                let scene = render::build_box(
                    &render::BoxModel {
                        k: b.scale * ui_k,
                        opacity: b.opacity,
                        label: item.label,
                        show_label: b.show_label,
                        show,
                        edit,
                        hover: self.hover.as_ref() == Some(&target) || matches!(&self.drag, Some(Drag::Box { id, .. }) if *id == b.id),
                        ghost,
                        value_floor: self.floors.get(&b.id).copied().unwrap_or(0.0),
                    },
                    &self.fonts,
                );
                self.floors.insert(b.id.clone(), scene.value_w);
                let (w, h) = (scene.w, scene.h);
                let mut slot = self.boxes.remove(&b.id);
                Self::update(&mut slot, scene, &mut self.fonts, &mut self.next_version);
                if let Some(c) = slot {
                    self.boxes.insert(b.id.clone(), c);
                }
                let (x, y) = clamp_pos(b.x as f32, b.y as f32, w, h, bb);
                layout.push((target, Area { x, y, w: w as f32, h: h as f32 }));
                order.push((idx as u32, b.id.clone(), (x as i32, y as i32)));
            }
        }
        let cfg = &self.cfg;
        self.boxes.retain(|id, _| cfg.is_shown(id));

        // control panel (on top of the boxes)
        let mut editor_pos = None;
        if edit {
            let shown: Vec<bool> = ITEMS.iter().map(|i| self.cfg.is_shown(i.id)).collect();
            let selected_item = self.selected_item.as_ref().and_then(|id| {
                let index = items::index_of(id)?;
                let b = self.cfg.boxes.iter().find(|b| b.id == *id)?;
                Some((index, b.scale, b.opacity, b.timezone.as_str(), b.show_label))
            });
            let game = self.data.as_ref().and_then(|d| d.get("game")).and_then(Value::as_str).unwrap_or("game");
            let ver = self.data.as_ref().and_then(|d| d.get("game_ver")).and_then(Value::as_str).unwrap_or("?");
            let status = format!("{} schema {ver} · {}×{}", game.to_uppercase(), bb.0, bb.1);
            let hover = match &self.hover {
                Some(Target::Editor(h)) => *h,
                _ => None,
            };
            let scene = render::build_editor(
                &render::EditorModel {
                    k: ui_k,
                    hide_when_idle: self.cfg.hide_when_idle,
                    exceed_speed_limit_indicator: self.cfg.exceed_speed_limit_indicator,
                    visible: self.cfg.visible,
                    shown: &shown,
                    selected_item,
                    status: &status,
                    hover,
                },
                &self.fonts,
            );
            let (w, h) = (scene.w, scene.h);
            Self::update(&mut self.editor, scene, &mut self.fonts, &mut self.next_version);
            let (ex, ey) = self.cfg.editor.map_or(
                ((bb.0 as f32 - w as f32) / 2.0, (bb.1 as f32 - h as f32) / 2.0),
                |(x, y)| (x as f32, y as f32),
            );
            let (ex, ey) = clamp_pos(ex, ey, w, h, bb);
            layout.push((Target::Editor(None), Area { x: ex, y: ey, w: w as f32, h: h as f32 }));
            editor_pos = Some((ex as i32, ey as i32));
        } else {
            self.editor = None;
        }

        let over_limit = self.cfg.exceed_speed_limit_indicator
            && self.data.as_ref().is_some_and(items::is_over_speed_limit);
        let flash_on = over_limit && self.started_at.elapsed().as_millis() % 700 < 350;
        if flash_on {
            let thickness = (12.0 * ui_k).round().max(2.0) as u32;
            let top = render::build_alert_strip(bb.0, thickness, ALERT_ALPHA);
            let side_height = bb.1.saturating_sub(thickness.saturating_mul(2)).max(1);
            let side = render::build_alert_strip(thickness, side_height, ALERT_ALPHA);
            Self::update(&mut self.alert_top, top, &mut self.fonts, &mut self.next_version);
            Self::update(&mut self.alert_side, side, &mut self.fonts, &mut self.next_version);
        }

        // cursor (on top of everything)
        let cursor_at = if edit { input.pointer.map(|(x, y)| (x.round() as i32, y.round() as i32)) } else { None };
        if cursor_at.is_some() && (self.cursor.is_none() || (self.cursor_k - ui_k).abs() > 1e-3) {
            if let Some(image) = render::cursor_pixmap(ui_k) {
                let version = self.bump();
                self.cursor = Some(Cached { scene: Scene::empty(), image, version });
                self.cursor_k = ui_k;
            }
        }
        self.layout = layout;

        let mut sprites = Vec::new();
        for (id, key, (x, y)) in &order {
            if let Some(c) = self.boxes.get(key) {
                sprites.push(Sprite { id: *id, version: c.version, x: *x, y: *y, image: &c.image });
            }
        }
        if let (Some((x, y)), Some(c)) = (editor_pos, self.editor.as_ref()) {
            sprites.push(Sprite { id: EDITOR_ID, version: c.version, x, y, image: &c.image });
        }
        if flash_on {
            if let (Some(top), Some(side)) = (&self.alert_top, &self.alert_side) {
                let thickness = top.image.height() as i32;
                sprites.push(Sprite { id: ALERT_TOP_ID, version: top.version, x: 0, y: 0, image: &top.image });
                sprites.push(Sprite {
                    id: ALERT_BOTTOM_ID,
                    version: top.version,
                    x: 0,
                    y: bb.1.saturating_sub(thickness as u32) as i32,
                    image: &top.image,
                });
                sprites.push(Sprite { id: ALERT_LEFT_ID, version: side.version, x: 0, y: thickness, image: &side.image });
                sprites.push(Sprite {
                    id: ALERT_RIGHT_ID,
                    version: side.version,
                    x: bb.0.saturating_sub(side.image.width()) as i32,
                    y: thickness,
                    image: &side.image,
                });
            }
        }
        if let (Some((x, y)), Some(c)) = (cursor_at, self.cursor.as_ref()) {
            sprites.push(Sprite { id: CURSOR_ID, version: c.version, x: x - 1, y: y - 1, image: &c.image });
        }
        Frame { sprites, edit }
    }
}

/// Handle owned by the plugin (game thread).
pub struct Overlay {
    shared: Arc<Shared>,
    #[cfg(windows)]
    backend: d3d11::Hook,
    #[cfg(target_os = "linux")]
    backend: glx::Hook,
}

impl Overlay {
    /// Installs the rendering hook. Only called when the overlay is enabled.
    pub fn start(cfg: OverlaySettings, path: Option<PathBuf>, meta: SessionMeta) -> Result<Self, String> {
        let shared = Arc::new(Shared { feed: Mutex::new(None), logs: Mutex::new(Vec::new()), meta });
        let core = Core::new(Arc::clone(&shared), cfg, path)?;
        #[cfg(windows)]
        {
            let backend = d3d11::Hook::install(core)?;
            Ok(Self { shared, backend })
        }
        #[cfg(target_os = "linux")]
        {
            let backend = glx::Hook::install(core)?;
            Ok(Self { shared, backend })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (core, shared);
            Err("the in-game overlay is only available on Windows/Proton and Linux".into())
        }
    }

    /// Game thread: hands over the newest snapshot; never waits.
    pub fn feed(&self, snap: &Snapshot) {
        if let Ok(mut f) = self.shared.feed.try_lock() {
            *f = Some(Feed { snap: *snap, at: Instant::now() });
        }
    }

    /// Game thread: messages from the render thread for game.log.txt.
    pub fn drain_logs(&self) -> Vec<String> {
        match self.shared.logs.try_lock() {
            Ok(mut l) if !l.is_empty() => std::mem::take(&mut *l),
            _ => Vec::new(),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        #[cfg(windows)]
        return "Direct3D 11";
        #[cfg(target_os = "linux")]
        return "OpenGL (GLX)";
        #[allow(unreachable_code)]
        "none"
    }

    /// Removes the hook and waits until no frame is inside it, so the plugin
    /// can be unloaded safely.
    pub fn shutdown(self) -> Vec<String> {
        #[cfg(any(windows, target_os = "linux"))]
        self.backend.uninstall();
        self.drain_logs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{BoxPos, stacked};
    use crate::snapshot::Caps;

    fn core(cfg: OverlaySettings, path: Option<PathBuf>) -> Core {
        let shared = Arc::new(Shared {
            feed: Mutex::new(None),
            logs: Mutex::new(Vec::new()),
            meta: SessionMeta { sid: 1, game: "ets2", game_ver: "1.22".into() },
        });
        Core::new(shared, cfg, path).unwrap()
    }

    fn truck_snapshot() -> Snapshot {
        let mut s = Snapshot::new(Caps::default());
        s.truck_present = true;
        s.paused = false;
        s.speed_ms = 25.0;
        s
    }

    fn give(c: &Core, s: Snapshot) {
        *c.shared.feed.lock().unwrap() = Some(Feed { snap: s, at: Instant::now() });
    }

    /// (id, version, x, y, w, h) per sprite.
    fn sprites(c: &mut Core, bb: (u32, u32), input: Input) -> Vec<(u32, u64, i32, i32, u32, u32)> {
        c.frame(bb, input).sprites.iter().map(|s| (s.id, s.version, s.x, s.y, s.image.width(), s.image.height())).collect()
    }

    fn enabled() -> OverlaySettings {
        OverlaySettings { enabled: true, ..Default::default() }
    }

    const BB: (u32, u32) = (1920, 1080);

    fn at(x: f32, y: f32, down: bool) -> Input {
        Input { pointer: Some((x, y)), left_button: down, hotkey: false }
    }

    #[test]
    fn disabled_or_idle_draws_nothing() {
        let mut c = core(OverlaySettings { enabled: false, ..Default::default() }, None);
        give(&c, truck_snapshot());
        assert!(sprites(&mut c, BB, Input::default()).is_empty());

        let mut c = core(enabled(), None);
        assert!(sprites(&mut c, BB, Input::default()).is_empty(), "no data yet, hide_when_idle");
        let mut c = core(OverlaySettings { hide_when_idle: false, ..enabled() }, None);
        assert_eq!(sprites(&mut c, BB, Input::default()).len(), 14, "shown with placeholders");
    }

    #[test]
    fn speed_limit_indicator_draws_red_edge_border_only_when_enabled() {
        let cfg = OverlaySettings {
            boxes: Vec::new(),
            exceed_speed_limit_indicator: true,
            ..enabled()
        };
        let mut c = core(cfg, None);
        c.started_at = Instant::now();
        let mut s = truck_snapshot();
        s.caps.navigation = true;
        s.nav_speed_limit_ms = 80.0 / 3.6;
        give(&c, s);
        let frame = c.frame(BB, Input::default());
        assert!(frame.sprites.iter().any(|sprite| sprite.id == ALERT_TOP_ID));
        let border = frame.sprites.iter().find(|sprite| sprite.id == ALERT_TOP_ID).unwrap();
        assert_eq!(border.image.pixel(0, 0).unwrap().alpha(), ALERT_ALPHA);

        let mut cfg = enabled();
        cfg.boxes.clear();
        let mut c = core(cfg, None);
        let mut s = truck_snapshot();
        s.caps.navigation = true;
        s.nav_speed_limit_ms = 80.0 / 3.6;
        give(&c, s);
        let frame = c.frame(BB, Input::default());
        assert!(!frame.sprites.iter().any(|sprite| sprite.id == ALERT_TOP_ID));
    }

    #[test]
    fn one_box_per_item_at_its_position_and_only_changed_boxes_repaint() {
        let cfg = OverlaySettings {
            boxes: vec![
                BoxPos { id: "speed".into(), x: 100, y: 50, hidden: false, scale: 1.0, opacity: 0.8, timezone: "UTC".into(), show_label: true },
                BoxPos { id: "gear".into(), x: 1500, y: 900, hidden: false, scale: 1.0, opacity: 0.8, timezone: "UTC".into(), show_label: true },
            ],
            ..enabled()
        };
        let mut c = core(cfg, None);
        give(&c, truck_snapshot());
        let first = sprites(&mut c, BB, Input::default());
        let speed = items::index_of("speed").unwrap() as u32;
        let gear = items::index_of("gear").unwrap() as u32;
        assert_eq!(first.iter().map(|s| (s.0, s.2, s.3)).collect::<Vec<_>>(), vec![(speed, 100, 50), (gear, 1500, 900)]);

        // same data → same versions (no re-upload)
        let again = sprites(&mut c, BB, Input::default());
        assert_eq!(first, again);
        // speed changes → only the speed box gets a new version
        std::thread::sleep(DATA_INTERVAL);
        let mut s = truck_snapshot();
        s.speed_ms = 30.0;
        give(&c, s);
        let next = sprites(&mut c, BB, Input::default());
        assert_ne!(next[0].1, first[0].1, "speed repainted");
        assert_eq!(next[1].1, first[1].1, "gear untouched");

        // 4K → boxes twice as big
        let big = sprites(&mut c, (3840, 2160), Input::default());
        assert!(big[1].4 > first[1].4 * 19 / 10);
    }

    #[test]
    fn boxes_stay_on_screen() {
        let cfg = OverlaySettings {
            boxes: vec![BoxPos { id: "speed".into(), x: 5000, y: 5000, hidden: false, scale: 1.0, opacity: 0.8, timezone: "UTC".into(), show_label: true }],
            ..enabled()
        };
        let mut c = core(cfg, None);
        give(&c, truck_snapshot());
        let s = sprites(&mut c, (1280, 720), Input::default());
        assert_eq!((s[0].2 as u32 + s[0].4, s[0].3 as u32 + s[0].5), (1280, 720));
    }

    #[test]
    fn card_sliders_only_change_the_selected_box() {
        let cfg = OverlaySettings {
            boxes: vec![
                BoxPos { id: "speed".into(), x: 40, y: 40, hidden: false, scale: 1.0, opacity: 0.8, timezone: "UTC".into(), show_label: true },
                BoxPos { id: "gear".into(), x: 40, y: 80, hidden: false, scale: 0.9, opacity: 0.5, timezone: "UTC".into(), show_label: true },
            ],
            ..enabled()
        };
        let mut c = core(cfg, None);
        give(&c, truck_snapshot());
        c.frame(BB, Input { hotkey: true, ..Input::default() });
        c.frame(BB, Input::default());

        let editor_area = c.area_of(&Target::Editor(None)).unwrap();
        let select = c
            .editor
            .as_ref()
            .unwrap()
            .scene
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Select(items::index_of("speed").unwrap()))
            .unwrap()
            .0;
        let pointer = (editor_area.x + select.x + select.w / 2.0, editor_area.y + select.y + select.h / 2.0);
        c.frame(BB, at(pointer.0, pointer.1, false));
        c.frame(BB, at(pointer.0, pointer.1, true));
        c.frame(BB, at(pointer.0, pointer.1, false));
        assert_eq!(c.selected_item.as_deref(), Some("speed"));

        let editor_area = c.area_of(&Target::Editor(None)).unwrap();
        let label_toggle = c
            .editor
            .as_ref()
            .unwrap()
            .scene
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::LabelToggle(items::index_of("speed").unwrap()))
            .unwrap()
            .0;
        let label_pointer = (
            editor_area.x + label_toggle.x + label_toggle.w / 2.0,
            editor_area.y + label_toggle.y + label_toggle.h / 2.0,
        );
        c.frame(BB, at(label_pointer.0, label_pointer.1, false));
        c.frame(BB, at(label_pointer.0, label_pointer.1, true));
        c.frame(BB, at(label_pointer.0, label_pointer.1, false));
        assert!(!c.cfg.boxes.iter().find(|b| b.id == "speed").unwrap().show_label);
        assert!(c.cfg.boxes.iter().find(|b| b.id == "gear").unwrap().show_label);

        for kind in [SliderKind::Scale, SliderKind::Opacity] {
            let editor_area = c.area_of(&Target::Editor(None)).unwrap();
            let track = c
                .editor
                .as_ref()
                .unwrap()
                .scene
                .hits
                .iter()
                .find(|(_, hit)| *hit == Hit::Slider(items::index_of("speed").unwrap(), kind))
                .unwrap()
                .0;
            let y = editor_area.y + track.y + track.h / 2.0;
            let start_x = editor_area.x + track.x + track.w * 0.2;
            let end_x = editor_area.x + track.x + track.w * 0.99;
            c.frame(BB, at(start_x, y, false));
            c.frame(BB, at(start_x, y, true));
            c.frame(BB, at(end_x, y, true));
            c.frame(BB, at(end_x, y, false));
        }

        let speed = c.cfg.boxes.iter().find(|b| b.id == "speed").unwrap();
        let gear = c.cfg.boxes.iter().find(|b| b.id == "gear").unwrap();
        assert!(speed.scale > 2.9);
        assert!(speed.opacity > 0.98);
        assert_eq!((gear.scale, gear.opacity), (0.9, 0.5));
    }

    #[test]
    fn real_time_card_timezone_can_be_changed_in_edit_mode() {
        let cfg = OverlaySettings {
            boxes: vec![BoxPos {
                id: "real_time".into(),
                x: 40,
                y: 40,
                hidden: false,
                scale: 1.0,
                opacity: 0.8,
                timezone: "UTC".into(),
                show_label: true,
            }],
            ..enabled()
        };
        let mut c = core(cfg, None);
        give(&c, truck_snapshot());
        c.frame(BB, Input { hotkey: true, ..Input::default() });
        c.frame(BB, Input::default());

        let real_time = items::index_of("real_time").unwrap();
        let editor_area = c.area_of(&Target::Editor(None)).unwrap();
        let card = c
            .editor
            .as_ref()
            .unwrap()
            .scene
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Select(real_time))
            .unwrap()
            .0;
        let p = (editor_area.x + card.x + 5.0, editor_area.y + card.y + 5.0);
        c.frame(BB, at(p.0, p.1, false));
        c.frame(BB, at(p.0, p.1, true));
        c.frame(BB, at(p.0, p.1, false));
        assert_eq!(c.selected_item.as_deref(), Some("real_time"));

        let editor_area = c.area_of(&Target::Editor(None)).unwrap();
        let next = c
            .editor
            .as_ref()
            .unwrap()
            .scene
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Timezone(1))
            .unwrap()
            .0;
        let p = (editor_area.x + next.x + next.w / 2.0, editor_area.y + next.y + next.h / 2.0);
        c.frame(BB, at(p.0, p.1, false));
        c.frame(BB, at(p.0, p.1, true));
        c.frame(BB, at(p.0, p.1, false));

        assert_eq!(c.cfg.boxes[0].timezone, "America/Anchorage");
    }

    #[test]
    fn edit_mode_previews_boxes_and_drags_them_one_by_one() {
        let dir = std::env::temp_dir().join(format!("aigakira-core-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(settings::FILE_NAME);
        settings::load_or_create(Some(path.clone()));
        let mut c = core(enabled(), Some(path.clone()));
        // no truck at all: live mode shows nothing …
        assert!(sprites(&mut c, BB, Input::default()).is_empty());

        // … edit mode shows every box (preview) plus the control panel
        let hk = Input { hotkey: true, ..Input::default() };
        c.frame(BB, hk);
        c.frame(BB, hk); // holding does not toggle again
        assert!(c.edit);
        let s = sprites(&mut c, BB, at(1.0, 1.0, false));
        let ids: Vec<u32> = s.iter().map(|s| s.0).collect();
        assert_eq!(ids.len(), 14 + 2, "14 boxes + panel + cursor");
        assert_eq!(&ids[14..], &[EDITOR_ID, CURSOR_ID], "panel above boxes, cursor on top");

        // drag the second box (limit) by its middle; the first stays put
        let limit = c.cfg.position("limit").unwrap();
        let speed_before = c.cfg.position("speed").unwrap();
        let grab = (limit.0 as f32 + 10.0, limit.1 as f32 + 10.0);
        c.frame(BB, at(grab.0, grab.1, false));
        assert_eq!(c.hover, Some(Target::Box("limit".into())));
        c.frame(BB, at(grab.0, grab.1, true));
        c.frame(BB, at(grab.0 + 700.0, grab.1 + 300.0, true));
        c.frame(BB, at(grab.0 + 700.0, grab.1 + 300.0, false));
        assert_eq!(c.cfg.position("limit"), Some((limit.0 + 700, limit.1 + 300)));
        assert_eq!(c.cfg.position("speed"), Some(speed_before));

        // switch "throttle" on from the panel: its box appears beside the panel
        let i = items::index_of("throttle").unwrap();
        let (ed, _) = c.layout.iter().find(|(t, _)| matches!(t, Target::Editor(_))).map(|(t, a)| (*a, t.clone())).unwrap();
        let (r, _) = *c.editor.as_ref().unwrap().scene.hits.iter().find(|(_, h)| *h == Hit::Toggle(i)).unwrap();
        let p = (ed.x + r.x + 5.0, ed.y + r.y + 5.0);
        c.frame(BB, at(p.0, p.1, false));
        c.frame(BB, at(p.0, p.1, true));
        c.frame(BB, at(p.0, p.1, false));
        let pos = c.cfg.position("throttle").expect("shown");
        assert!(pos.0 as f32 >= ed.x + ed.w || (pos.0 as f32) < ed.x, "placed beside the panel, not under it");

        // drag the panel by its title
        c.frame(BB, at(ed.x + 6.0, ed.y + 6.0, false));
        c.frame(BB, at(ed.x + 6.0, ed.y + 6.0, true));
        c.frame(BB, at(ed.x - 100.0, ed.y - 50.0, true));
        c.frame(BB, at(ed.x - 100.0, ed.y - 50.0, false));
        // grabbed 6 px inside the corner, moved by (-106, -56) from there
        assert_eq!(c.cfg.editor, Some(((ed.x - 106.0).round() as i32, (ed.y - 56.0).round() as i32)));

        // leave edit mode: no panel/cursor, and everything is in the file
        c.frame(BB, hk);
        assert!(!c.edit);
        assert!(sprites(&mut c, BB, at(5.0, 5.0, false)).is_empty(), "no truck → hidden again");
        let mut w = Vec::new();
        let saved = settings::parse(&std::fs::read_to_string(&path).unwrap(), &mut w).overlay;
        assert_eq!(saved.position("limit"), Some((limit.0 + 700, limit.1 + 300)));
        assert!(saved.is_shown("throttle"));
        assert_eq!(saved.editor, c.cfg.editor);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn size_and_opacity_change_only_the_boxes() {
        let mut c = core(OverlaySettings { boxes: stacked(&["speed".into()], 40, 40, 1.0), ..enabled() }, None);
        give(&c, truck_snapshot());
        let hk = Input { hotkey: true, ..Input::default() };
        c.frame(BB, hk);
        let before = sprites(&mut c, BB, at(1.0, 1.0, false));
        let size = |s: &[(u32, u64, i32, i32, u32, u32)], id: u32| s.iter().find(|x| x.0 == id).map(|x| (x.4, x.5)).unwrap();
        let version = |s: &[(u32, u64, i32, i32, u32, u32)], id: u32| s.iter().find(|x| x.0 == id).map(|x| x.1).unwrap();
        let speed = items::index_of("speed").unwrap() as u32;

        c.on_button(Button::Bigger);
        c.on_button(Button::Bigger);
        let bigger = sprites(&mut c, BB, at(1.0, 1.0, false));
        assert!(size(&bigger, speed).0 > size(&before, speed).0, "box grew");
        assert_eq!(size(&bigger, EDITOR_ID), size(&before, EDITOR_ID), "control panel size unchanged");
        assert_eq!(size(&bigger, CURSOR_ID), size(&before, CURSOR_ID), "cursor size unchanged");

        let editor_v = version(&bigger, EDITOR_ID);
        let box_v = version(&bigger, speed);
        c.on_button(Button::Fainter);
        let fainter = sprites(&mut c, BB, at(1.0, 1.0, false));
        assert_ne!(version(&fainter, speed), box_v, "opacity change repaints the box in edit mode");
        assert_eq!(version(&fainter, EDITOR_ID), editor_v, "control panel not affected by opacity");
        let alpha = |c: &Core| c.boxes["speed"].image.pixel(2, 17).unwrap().alpha();
        let a1 = alpha(&c);
        c.on_button(Button::Fainter);
        c.on_button(Button::Fainter);
        sprites(&mut c, BB, at(1.0, 1.0, false));
        assert!(alpha(&c) < a1, "box background more transparent in the preview");
    }

    /// Clicks the control-panel row of `id`.
    fn click_item(c: &mut Core, id: &str) {
        let i = items::index_of(id).unwrap();
        let ed = c.layout.iter().find(|(t, _)| matches!(t, Target::Editor(_))).map(|(_, a)| *a).unwrap();
        let (r, _) = *c.editor.as_ref().unwrap().scene.hits.iter().find(|(_, h)| *h == Hit::Toggle(i)).unwrap();
        let p = (ed.x + r.x + 5.0, ed.y + r.y + 5.0);
        c.frame(BB, at(p.0, p.1, false));
        c.frame(BB, at(p.0, p.1, true));
        c.frame(BB, at(p.0, p.1, false));
    }

    #[test]
    fn unchecking_and_checking_again_keeps_the_position() {
        let mut c = core(enabled(), None);
        give(&c, truck_snapshot());
        c.cfg.set_position("gear", 1500, 900);
        let hk = Input { hotkey: true, ..Input::default() };
        c.frame(BB, hk);
        c.frame(BB, at(1.0, 1.0, false));
        click_item(&mut c, "gear");
        assert!(!c.cfg.is_shown("gear"), "unchecked");
        let gear = items::index_of("gear").unwrap() as u32;
        assert!(!sprites(&mut c, BB, at(1.0, 1.0, false)).iter().any(|s| s.0 == gear), "box gone");
        click_item(&mut c, "gear");
        assert!(c.cfg.is_shown("gear"), "checked again");
        let s = sprites(&mut c, BB, at(1.0, 1.0, false));
        let g = s.iter().find(|s| s.0 == gear).unwrap();
        assert_eq!((g.2, g.3), (1500, 900), "back where it was");
    }

    #[test]
    fn hide_all_hides_in_game_and_previews_faded_in_edit_mode() {
        let mut c = core(OverlaySettings { boxes: stacked(&["speed".into()], 40, 40, 1.0), ..enabled() }, None);
        give(&c, truck_snapshot());
        assert_eq!(sprites(&mut c, BB, Input::default()).len(), 1);
        let hk = Input { hotkey: true, ..Input::default() };
        c.frame(BB, hk);
        let normal = sprites(&mut c, BB, at(1.0, 1.0, false));
        let alpha = |c: &Core| c.boxes["speed"].image.data().chunks_exact(4).map(|p| u64::from(p[3])).sum::<u64>();
        let a_normal = alpha(&c);
        c.on_button(Button::HideAll);
        assert!(!c.cfg.visible);
        let ghosted = sprites(&mut c, BB, at(1.0, 1.0, false));
        assert_eq!(ghosted.len(), normal.len(), "still previewed in edit mode");
        assert!(alpha(&c) < a_normal / 2, "faded");
        // leave edit mode: nothing drawn in game, selection untouched
        c.frame(BB, Input::default());
        c.frame(BB, hk);
        assert!(!c.edit);
        assert!(sprites(&mut c, BB, Input::default()).is_empty());
        assert!(c.cfg.is_shown("speed"));
        // Show all brings them back
        c.cfg.visible = true;
        assert_eq!(sprites(&mut c, BB, Input::default()).len(), 1);
    }

    #[test]
    fn hiding_an_item_removes_its_box() {
        let mut c = core(OverlaySettings { boxes: stacked(&["speed".into(), "gear".into()], 40, 40, 1.0), ..enabled() }, None);
        give(&c, truck_snapshot());
        assert_eq!(sprites(&mut c, BB, Input::default()).len(), 2);
        c.cfg.toggle("gear", (0, 0));
        assert_eq!(sprites(&mut c, BB, Input::default()).len(), 1);
        assert!(!c.boxes.contains_key("gear"));
    }
}
