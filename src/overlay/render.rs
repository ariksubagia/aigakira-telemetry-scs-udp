#![forbid(unsafe_code)]
//! Layout and software rendering of the overlay panel.
//!
//! [`build`] turns the current state into a [`Scene`]: draw ops and hit
//! regions in screen pixels (the size factor `k` already includes the
//! resolution). [`paint`] rasterises it into a premultiplied RGBA pixmap that
//! the D3D11 / OpenGL backends upload as a texture. Repainting only happens
//! when the scene changes.

use std::collections::HashMap;

use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use crate::overlay::items::{ITEMS, Show, Tone};

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

pub struct Fonts {
    regular: fontdue::Font,
    bold: fontdue::Font,
    cache: HashMap<(bool, char, u32), (fontdue::Metrics, Vec<u8>)>,
}

static REGULAR: &[u8] = include_bytes!("../../fonts/NotoSans-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../../fonts/NotoSans-Bold.ttf");

impl Fonts {
    /// Noto Sans, embedded in the plugin (SIL OFL 1.1, see fonts/OFL.txt), so
    /// the Windows build under Proton needs no system fonts.
    pub fn embedded() -> Result<Self, String> {
        let settings = fontdue::FontSettings::default();
        let regular = fontdue::Font::from_bytes(REGULAR, settings).map_err(|e| format!("font: {e}"))?;
        let bold = fontdue::Font::from_bytes(BOLD, fontdue::FontSettings::default()).map_err(|e| format!("font: {e}"))?;
        Ok(Self { regular, bold, cache: HashMap::new() })
    }

    fn font(&self, bold: bool) -> &fontdue::Font {
        if bold { &self.bold } else { &self.regular }
    }

    /// Text advance width in logical pixels.
    pub fn measure(&self, text: &str, px: f32, bold: bool) -> f32 {
        let font = self.font(bold);
        text.chars().map(|c| font.metrics(c, px).advance_width).sum()
    }

    /// (ascent, descent) in logical pixels; descent is negative.
    fn line(&self, px: f32) -> (f32, f32) {
        self.regular
            .horizontal_line_metrics(px)
            .map_or((px * 0.8, -px * 0.2), |m| (m.ascent, m.descent))
    }

    fn glyph(&mut self, bold: bool, c: char, px: f32) -> &(fontdue::Metrics, Vec<u8>) {
        let key = (bold, c, (px * 64.0).round() as u32);
        if !self.cache.contains_key(&key) {
            if self.cache.len() > 4096 {
                self.cache.clear();
            }
            let g = self.font(bold).rasterize(c, px);
            self.cache.insert(key, g);
        }
        &self.cache[&key]
    }

    /// Draws text with its baseline at `(x, baseline)` in *device* pixels.
    fn draw(&mut self, pm: &mut Pixmap, text: &str, x: f32, baseline: f32, px: f32, bold: bool, rgba: [u8; 4]) {
        let (w, h) = (pm.width() as i32, pm.height() as i32);
        let mut pen = x;
        for c in text.chars() {
            let (m, cov) = {
                let g = self.glyph(bold, c, px);
                (g.0, g.1.clone())
            };
            let left = (pen + m.xmin as f32).round() as i32;
            let top = (baseline - (m.height as i32 + m.ymin) as f32).round() as i32;
            let data = pm.data_mut();
            for gy in 0..m.height as i32 {
                let y = top + gy;
                if y < 0 || y >= h {
                    continue;
                }
                for gx in 0..m.width as i32 {
                    let xx = left + gx;
                    if xx < 0 || xx >= w {
                        continue;
                    }
                    let c = u32::from(cov[(gy * m.width as i32 + gx) as usize]);
                    if c == 0 {
                        continue;
                    }
                    let a = u32::from(rgba[3]) * c / 255; // source alpha
                    let i = ((y * w + xx) * 4) as usize;
                    for ch in 0..3 {
                        let src = u32::from(rgba[ch]) * a / 255; // premultiplied
                        let dst = u32::from(data[i + ch]);
                        data[i + ch] = (src + dst * (255 - a) / 255).min(255) as u8;
                    }
                    let da = u32::from(data[i + 3]);
                    data[i + 3] = (a + da * (255 - a) / 255).min(255) as u8;
                }
            }
            pen += m.advance_width;
        }
    }
}

// ---------------------------------------------------------------------------
// Scene
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Smaller,
    Bigger,
    Fainter,
    Stronger,
    IdleMode,
    HideAll,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Toggle(usize),
    Button(Button),
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Fill { r: Rect, radius: f32, rgba: [u8; 4] },
    Outline { r: Rect, radius: f32, width: f32, rgba: [u8; 4] },
    Text { x: f32, y: f32, h: f32, px: f32, bold: bool, rgba: [u8; 4], text: String },
    Triangle { pts: [(f32, f32); 3], rgba: [u8; 4] },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub w: u32,
    pub h: u32,
    pub hits: Vec<(Rect, Hit)>,
    /// Width used by the value column; fed back as a floor so the panel
    /// grows but does not jitter with changing values.
    pub value_w: f32,
    /// Overall strength when painted (1 = normal; less = faded preview).
    fade: f32,
    ops: Vec<Op>,
}

impl Scene {
    pub fn empty() -> Self {
        Self { w: 0, h: 0, hits: Vec::new(), value_w: 0.0, fade: 1.0, ops: Vec::new() }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }
}

/// One telemetry box.
pub struct BoxModel<'a> {
    /// Size factor: user scale × screen height / 1080.
    pub k: f32,
    pub opacity: f32,
    pub label: &'a str,
    pub show: Show,
    /// Edit mode: outlined, and highlighted when `hover`.
    pub edit: bool,
    pub hover: bool,
    /// Edit-mode preview of a box that "Hide all" hides in game: faded.
    pub ghost: bool,
    /// Sticky minimum value width, so the box does not jitter.
    pub value_floor: f32,
}

/// The edit-mode control panel.
pub struct EditorModel<'a> {
    pub k: f32,
    pub hide_when_idle: bool,
    /// false = "Hide all" is on.
    pub visible: bool,
    /// Per `ITEMS` entry: is its box shown.
    pub shown: &'a [bool],
    pub status: &'a str,
    pub hover: Option<Hit>,
}

const TEXT: [u8; 4] = [232, 237, 243, 255];
const DIM: [u8; 4] = [138, 150, 166, 255];
const GOOD: [u8; 4] = [95, 211, 141, 255];
const WARN: [u8; 4] = [255, 184, 77, 255];
const ALERT: [u8; 4] = [255, 93, 93, 255];
const ACCENT: [u8; 4] = [78, 161, 255, 255];
const TRACK: [u8; 4] = [255, 255, 255, 34];
const BLINK_OFF: [u8; 4] = [255, 255, 255, 40];
const BLINK_ON: [u8; 4] = [96, 220, 96, 255];
const HOVER: [u8; 4] = [255, 255, 255, 22];

fn tone(t: Tone) -> [u8; 4] {
    match t {
        Tone::Normal => TEXT,
        Tone::Dim => DIM,
        Tone::Good => GOOD,
        Tone::Warn => WARN,
        Tone::Alert => ALERT,
    }
}

struct Builder<'f> {
    fonts: &'f Fonts,
    k: f32,
    ops: Vec<Op>,
    hits: Vec<(Rect, Hit)>,
}

impl Builder<'_> {
    fn px(&self) -> f32 {
        15.0 * self.k
    }
    fn row_h(&self) -> f32 {
        24.0 * self.k
    }
    fn text(&mut self, x: f32, y: f32, h: f32, px: f32, bold: bool, rgba: [u8; 4], text: impl Into<String>) {
        self.ops.push(Op::Text { x, y, h, px, bold, rgba, text: text.into() });
    }

    /// Width a value needs.
    fn value_width(&self, show: &Show) -> f32 {
        let px = self.px();
        match show {
            Show::Text(t, _) => self.fonts.measure(t, px, true),
            Show::Bar { text, .. } => 70.0 * self.k + 8.0 * self.k + self.fonts.measure(text, px, true),
            Show::Blinkers { .. } => 56.0 * self.k,
        }
    }

    fn value(&mut self, x: f32, y: f32, show: &Show) {
        let (px, h, k) = (self.px(), self.row_h(), self.k);
        match show {
            Show::Text(t, tn) => self.text(x, y, h, px, true, tone(*tn), t.clone()),
            Show::Bar { frac, text, tone: tn } => {
                let bw = 70.0 * k;
                let bh = 7.0 * k;
                let by = y + (h - bh) / 2.0;
                let track = Rect { x, y: by, w: bw, h: bh };
                self.ops.push(Op::Fill { r: track, radius: bh / 2.0, rgba: TRACK });
                if *frac > 0.0 {
                    let fill_rgba = if *tn == Tone::Normal { ACCENT } else { tone(*tn) };
                    let r = Rect { w: (bw * frac).max(bh), ..track };
                    self.ops.push(Op::Fill { r, radius: bh / 2.0, rgba: fill_rgba });
                }
                self.text(x + bw + 8.0 * k, y, h, px, true, tone(*tn), text.clone());
            }
            Show::Blinkers { left, right } => {
                let s = 9.0 * k;
                let cy = y + h / 2.0;
                let lx = x + 2.0 * k;
                let rx = x + 54.0 * k;
                self.ops.push(Op::Triangle {
                    pts: [(lx, cy), (lx + 1.6 * s, cy - s), (lx + 1.6 * s, cy + s)],
                    rgba: if *left { BLINK_ON } else { BLINK_OFF },
                });
                self.ops.push(Op::Triangle {
                    pts: [(rx, cy), (rx - 1.6 * s, cy - s), (rx - 1.6 * s, cy + s)],
                    rgba: if *right { BLINK_ON } else { BLINK_OFF },
                });
            }
        }
    }

    fn button(&mut self, x: f32, y: f32, label: &str, b: Button, primary: bool, hover: bool) -> f32 {
        let (k, px) = (self.k, 13.0 * self.k);
        let w = self.fonts.measure(label, px, true) + 20.0 * k;
        let h = 26.0 * k;
        let r = Rect { x, y, w, h };
        let bg = match (primary, hover) {
            (true, false) => ACCENT,
            (true, true) => [120, 185, 255, 255],
            (false, false) => [255, 255, 255, 28],
            (false, true) => [255, 255, 255, 60],
        };
        self.ops.push(Op::Fill { r, radius: 6.0 * k, rgba: bg });
        let fg = if primary { [10, 18, 30, 255] } else { TEXT };
        self.text(x + 10.0 * k, y, h, px, true, fg, label);
        self.hits.push((r, Hit::Button(b)));
        w
    }
}

/// Height of one box at k = 1 (see `settings::STACK_STEP` for spacing).
pub const BOX_H: f32 = 34.0;

pub fn build_box(m: &BoxModel<'_>, fonts: &Fonts) -> Scene {
    let mut b = Builder { fonts, k: m.k, ops: Vec::new(), hits: Vec::new() };
    let (k, px, row_h) = (b.k, b.px(), b.row_h());
    let (pad_x, gap) = (10.0 * k, 12.0 * k);
    let h = BOX_H * k;
    let label_w = fonts.measure(m.label, px, false);
    let value_w = b.value_width(&m.show).max(m.value_floor).max(24.0 * k);
    let w = pad_x * 2.0 + label_w + gap + value_w;
    // The real opacity also in edit mode, so changes are visible right away.
    let alpha = (m.opacity * 255.0).round() as u8;
    let all = Rect { x: 0.0, y: 0.0, w, h };
    b.ops.push(Op::Fill { r: all, radius: 8.0 * k, rgba: [14, 17, 23, alpha] });
    if m.edit {
        let (width, rgba) = if m.hover { (2.5 * k, [140, 195, 255, 255]) } else { (1.5 * k, ACCENT) };
        let inset = width / 2.0;
        let r = Rect { x: inset, y: inset, w: w - width, h: h - width };
        b.ops.push(Op::Outline { r, radius: 8.0 * k, width, rgba });
    }
    let y = (h - row_h) / 2.0;
    b.text(pad_x, y, row_h, px, false, DIM, m.label);
    b.value(pad_x + label_w + gap, y, &m.show);
    let fade = if m.ghost { 0.4 } else { 1.0 };
    Scene { w: w.ceil() as u32, h: h.ceil() as u32, hits: Vec::new(), value_w, fade, ops: b.ops }
}

pub fn build_editor(m: &EditorModel<'_>, fonts: &Fonts) -> Scene {
    let mut b = Builder { fonts, k: m.k, ops: Vec::new(), hits: Vec::new() };
    let (k, px) = (b.k, b.px());
    let row_h = 22.0 * k;
    let pad = 12.0 * k;
    let check = 15.0 * k;
    let cols = 3;
    let per_col = ITEMS.len().div_ceil(cols);
    let label_w = ITEMS.iter().map(|i| fonts.measure(i.label, px, false)).fold(0.0, f32::max);
    let col_w = check + 8.0 * k + label_w + 18.0 * k;

    let title_px = 17.0 * k;
    let small = 12.5 * k;
    let hint1 = "Drag any box to move it · click an item below to show or hide its box (it keeps its place)";
    let hint2 = format!("Ctrl+Shift+Backspace or Done to finish · drag this panel by its title · {}", m.status);
    let mut y = pad;
    b.text(pad, y, 22.0 * k, title_px, true, ACCENT, "Telemetry overlay · edit mode");
    y += 24.0 * k;
    b.text(pad, y, 18.0 * k, small, false, DIM, hint1);
    y += 19.0 * k;
    b.text(pad, y, 18.0 * k, small, false, DIM, hint2.clone());
    y += 26.0 * k;

    let mut x = pad;
    let idle_label = if m.hide_when_idle { "No truck: hide" } else { "No truck: show" };
    for (label, btn) in [
        ("Size −", Button::Smaller),
        ("Size +", Button::Bigger),
        ("Opacity −", Button::Fainter),
        ("Opacity +", Button::Stronger),
        (idle_label, Button::IdleMode),
    ] {
        x += b.button(x, y, label, btn, false, m.hover == Some(Hit::Button(btn))) + 6.0 * k;
    }
    // Hide all / Show all: highlighted while everything is hidden
    let (all_label, all_primary) = if m.visible { ("Hide all", false) } else { ("Show all", true) };
    x += b.button(x, y, all_label, Button::HideAll, all_primary, m.hover == Some(Hit::Button(Button::HideAll))) + 6.0 * k;
    let done_w = fonts.measure("Done", 13.0 * k, true) + 20.0 * k;
    let w = [
        pad * 2.0 + col_w * cols as f32,
        x + done_w + pad,
        pad * 2.0 + fonts.measure(hint1, small, false),
        pad * 2.0 + fonts.measure(&hint2, small, false),
    ]
    .into_iter()
    .fold(0.0, f32::max);
    b.button(w - pad - done_w, y, "Done", Button::Done, true, m.hover == Some(Hit::Button(Button::Done)));
    y += 26.0 * k + 10.0 * k;

    let grid_top = y;
    for (i, item) in ITEMS.iter().enumerate() {
        let (col, row) = (i / per_col, i % per_col);
        let x0 = pad + col as f32 * col_w;
        let y0 = grid_top + row as f32 * row_h;
        let on = m.shown.get(i).copied().unwrap_or(false);
        let r = Rect { x: x0 - 4.0 * k, y: y0, w: col_w - 4.0 * k, h: row_h };
        if m.hover == Some(Hit::Toggle(i)) {
            b.ops.push(Op::Fill { r, radius: 5.0 * k, rgba: HOVER });
        }
        let cb = Rect { x: x0, y: y0 + (row_h - check) / 2.0, w: check, h: check };
        if on {
            b.ops.push(Op::Fill { r: cb, radius: 4.0 * k, rgba: ACCENT });
            let (cx, cy, s) = (cb.x, cb.y, check);
            b.ops.push(Op::Triangle {
                pts: [(cx + 0.22 * s, cy + 0.52 * s), (cx + 0.42 * s, cy + 0.74 * s), (cx + 0.42 * s, cy + 0.56 * s)],
                rgba: [10, 18, 30, 255],
            });
            b.ops.push(Op::Triangle {
                pts: [(cx + 0.42 * s, cy + 0.74 * s), (cx + 0.80 * s, cy + 0.28 * s), (cx + 0.40 * s, cy + 0.56 * s)],
                rgba: [10, 18, 30, 255],
            });
        } else {
            b.ops.push(Op::Outline { r: cb, radius: 4.0 * k, width: 1.5 * k, rgba: DIM });
        }
        b.text(x0 + check + 8.0 * k, y0, row_h, px, false, if on { TEXT } else { DIM }, item.label);
        b.hits.push((r, Hit::Toggle(i)));
    }
    let h = grid_top + per_col as f32 * row_h + pad;

    let mut ops = vec![
        Op::Fill { r: Rect { x: 0.0, y: 0.0, w, h }, radius: 10.0 * k, rgba: [14, 17, 23, 242] },
        Op::Outline { r: Rect { x: 1.0, y: 1.0, w: w - 2.0, h: h - 2.0 }, radius: 10.0 * k, width: 2.0, rgba: ACCENT },
    ];
    ops.append(&mut b.ops);
    Scene { w: w.ceil() as u32, h: h.ceil() as u32, hits: b.hits, value_w: 0.0, fade: 1.0, ops }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

fn rounded(r: Rect, radius: f32) -> Option<Path> {
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let c = rad * 0.552_284_8; // cubic circle approximation
    let mut p = PathBuilder::new();
    p.move_to(x0 + rad, y0);
    p.line_to(x1 - rad, y0);
    p.cubic_to(x1 - rad + c, y0, x1, y0 + rad - c, x1, y0 + rad);
    p.line_to(x1, y1 - rad);
    p.cubic_to(x1, y1 - rad + c, x1 - rad + c, y1, x1 - rad, y1);
    p.line_to(x0 + rad, y1);
    p.cubic_to(x0 + rad - c, y1, x0, y1 - rad + c, x0, y1 - rad);
    p.line_to(x0, y0 + rad);
    p.cubic_to(x0, y0 + rad - c, x0 + rad - c, y0, x0 + rad, y0);
    p.close();
    p.finish()
}

fn paint_of(rgba: [u8; 4]) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]));
    p.anti_alias = true;
    p
}

/// Rasterises a scene into a premultiplied RGBA pixmap of `bw × bh` device
/// pixels, `scale` device pixels per logical pixel.
pub fn paint(scene: &Scene, fonts: &mut Fonts, scale: f32, bw: u32, bh: u32) -> Option<Pixmap> {
    let mut pm = paint_ops(scene, fonts, scale, bw, bh)?;
    if scene.fade < 1.0 {
        // premultiplied: scaling every channel fades the whole image
        let f = (scene.fade.clamp(0.0, 1.0) * 256.0) as u32;
        for b in pm.data_mut() {
            *b = ((u32::from(*b) * f) >> 8) as u8;
        }
    }
    Some(pm)
}

fn paint_ops(scene: &Scene, fonts: &mut Fonts, scale: f32, bw: u32, bh: u32) -> Option<Pixmap> {
    let mut pm = Pixmap::new(bw.max(1), bh.max(1))?;
    let ts = Transform::from_scale(scale, scale);
    for op in &scene.ops {
        match op {
            Op::Fill { r, radius, rgba } => {
                if let Some(path) = rounded(*r, *radius) {
                    pm.fill_path(&path, &paint_of(*rgba), FillRule::Winding, ts, None);
                }
            }
            Op::Outline { r, radius, width, rgba } => {
                if let Some(path) = rounded(*r, *radius) {
                    let stroke = Stroke { width: *width, ..Stroke::default() };
                    pm.stroke_path(&path, &paint_of(*rgba), &stroke, ts, None);
                }
            }
            Op::Triangle { pts, rgba } => {
                let mut p = PathBuilder::new();
                p.move_to(pts[0].0, pts[0].1);
                p.line_to(pts[1].0, pts[1].1);
                p.line_to(pts[2].0, pts[2].1);
                p.close();
                if let Some(path) = p.finish() {
                    pm.fill_path(&path, &paint_of(*rgba), FillRule::Winding, ts, None);
                }
            }
            Op::Text { x, y, h, px, bold, rgba, text } => {
                let (asc, desc) = fonts.line(*px);
                let baseline = y + (h - (asc - desc)) / 2.0 + asc;
                // Snap the baseline to the device grid for crisp text.
                let dev_baseline = (baseline * scale).round();
                fonts.draw(&mut pm, text, x * scale, dev_baseline, px * scale, *bold, *rgba);
            }
        }
    }
    Some(pm)
}

/// The edit-mode mouse cursor: a white arrow with a dark outline, hot spot
/// at (1, 1). Drawn on top of everything, because the game's own cursor is
/// part of the frame *below* the overlay.
pub fn cursor_pixmap(k: f32) -> Option<Pixmap> {
    let k = k.max(0.5);
    let size = (24.0 * k).ceil() as u32;
    let mut pm = Pixmap::new(size, size)?;
    let pts = [(0.0, 0.0), (0.0, 17.0), (4.5, 13.2), (7.6, 20.0), (10.4, 18.8), (7.4, 12.2), (12.6, 12.2)];
    let mut p = PathBuilder::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        let (x, y) = (1.0 + x * k, 1.0 + y * k);
        if i == 0 { p.move_to(x, y) } else { p.line_to(x, y) }
    }
    p.close();
    let path = p.finish()?;
    pm.fill_path(&path, &paint_of([255, 255, 255, 255]), FillRule::Winding, Transform::identity(), None);
    let stroke = Stroke { width: 1.4 * k, line_join: tiny_skia::LineJoin::Round, ..Stroke::default() };
    pm.stroke_path(&path, &paint_of([10, 14, 20, 255]), &stroke, Transform::identity(), None);
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::items::{index_of, tests::sample};

    fn boxm(id: &str, edit: bool, hover: bool, k: f32) -> BoxModel<'static> {
        let item = &ITEMS[index_of(id).unwrap()];
        BoxModel { k, opacity: 0.8, label: item.label, show: (item.show)(&sample()), edit, hover, ghost: false, value_floor: 0.0 }
    }

    fn texts(s: &Scene) -> Vec<&str> {
        s.ops.iter().filter_map(|o| if let Op::Text { text, .. } = o { Some(text.as_str()) } else { None }).collect()
    }

    #[test]
    fn box_shows_label_and_value() {
        let fonts = Fonts::embedded().unwrap();
        let s = build_box(&boxm("speed", false, false, 1.0), &fonts);
        assert_eq!(texts(&s), vec!["Speed", "87 km/h"]);
        assert!(s.hits.is_empty());
        assert_eq!(s.h as f32, BOX_H);
        let big = build_box(&boxm("speed", false, false, 2.0), &fonts);
        assert!(big.w > s.w * 19 / 10 && big.h == s.h * 2);
        // edit outline and hover change the image
        let e = build_box(&boxm("speed", true, false, 1.0), &fonts);
        let eh = build_box(&boxm("speed", true, true, 1.0), &fonts);
        assert_ne!(s, e);
        assert_ne!(e, eh);
        // a ghost (Hide all preview) is the same box, fainter
        let mut fonts2 = Fonts::embedded().unwrap();
        let normal = paint(&e, &mut fonts2, 1.0, e.w, e.h).unwrap();
        let ghost_scene = build_box(&BoxModel { ghost: true, ..boxm("speed", true, false, 1.0) }, &fonts);
        let ghost = paint(&ghost_scene, &mut fonts2, 1.0, ghost_scene.w, ghost_scene.h).unwrap();
        let alpha_sum = |p: &Pixmap| p.data().chunks_exact(4).map(|c| u64::from(c[3])).sum::<u64>();
        assert!(alpha_sum(&ghost) < alpha_sum(&normal) / 2);
        // the value floor keeps the width from shrinking
        let wide = build_box(&BoxModel { value_floor: 200.0, ..boxm("speed", false, false, 1.0) }, &fonts);
        assert!(wide.w > s.w);
    }

    #[test]
    fn editor_hits_cover_every_item_and_button() {
        let fonts = Fonts::embedded().unwrap();
        let shown: Vec<bool> = ITEMS.iter().map(|i| i.default_on).collect();
        let m = EditorModel { k: 1.0, hide_when_idle: true, visible: true, shown: &shown, status: "ETS2 1.22", hover: None };
        let s = build_editor(&m, &fonts);
        for i in 0..ITEMS.len() {
            assert!(s.hits.iter().any(|(_, h)| *h == Hit::Toggle(i)), "item {i} not clickable");
        }
        for b in [Button::Smaller, Button::Bigger, Button::Fainter, Button::Stronger, Button::IdleMode, Button::HideAll, Button::Done] {
            let (r, _) = s.hits.iter().find(|(_, h)| *h == Hit::Button(b)).unwrap();
            assert!(r.x + r.w <= s.w as f32 && r.y + r.h <= s.h as f32, "{b:?} outside panel");
            assert_eq!(s.hit(r.x + 1.0, r.y + 1.0), Some(Hit::Button(b)));
        }
        assert_eq!(s.hit(5.0, 5.0), None, "title area = drag handle");
        let hovered = build_editor(&EditorModel { hover: Some(Hit::Toggle(0)), ..m }, &fonts);
        assert_ne!(s, hovered);
    }

    #[test]
    fn cursor_sprite() {
        let c = cursor_pixmap(1.0).unwrap();
        let px = |x: u32, y: u32| c.pixel(x, y).unwrap();
        assert!(px(3, 8).alpha() == 255 && px(3, 8).red() > 200, "white body");
        assert_eq!(px(20, 2).alpha(), 0, "transparent outside the arrow");
        assert!(cursor_pixmap(2.0).unwrap().width() > c.width());
    }

    /// Writes PNG snapshots to target/overlay-snapshots for visual review.
    #[test]
    fn paint_snapshots() {
        let mut fonts = Fonts::embedded().unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/overlay-snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, id, edit, hover) in [("box-speed", "speed", false, false), ("box-rpm-edit", "rpm", true, true), ("box-blinkers", "blinkers", false, false)] {
            let s = build_box(&boxm(id, edit, hover, 1.0), &fonts);
            paint(&s, &mut fonts, 1.0, s.w, s.h).unwrap().save_png(dir.join(format!("{name}.png"))).unwrap();
        }
        let shown: Vec<bool> = ITEMS.iter().map(|i| i.default_on).collect();
        let m = EditorModel { k: 1.0, hide_when_idle: true, visible: true, shown: &shown, status: "ETS2 1.22", hover: Some(Hit::Toggle(4)) };
        let s = build_editor(&m, &fonts);
        paint(&s, &mut fonts, 1.0, s.w, s.h).unwrap().save_png(dir.join("editor.png")).unwrap();
    }
}
