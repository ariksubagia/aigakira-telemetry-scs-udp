#![forbid(unsafe_code)]
//! The plugin's settings file, `aigakira_telemetry.toml`, next to the plugin
//! in the game's `bin/<platform>/plugins/` folder.
//!
//! It is created with comments and defaults on the first start (overlay
//! on). Saving from the in-game edit mode only rewrites the
//! `[overlay]` values, so the user's comments and other edits survive.
//!
//! Every shown telemetry item is its own box with its own position:
//! `[overlay.boxes]` holds `id = { x = …, y = … }` lines. Files from v0.3.x
//! (one panel: `x`, `y`, `items = [...]`) are read too; their items become
//! boxes stacked at the old panel position, and the next save writes the new
//! form.

use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use crate::overlay::items::{self, ITEMS};

pub const FILE_NAME: &str = "aigakira_telemetry.toml";

/// Vertical distance between stacked boxes at scale 1 (1080p pixels).
pub const STACK_STEP: i32 = 40;

#[derive(Clone, Debug, PartialEq)]
pub struct BoxPos {
    pub id: String,
    /// Top-left corner in screen (back buffer) pixels.
    pub x: i32,
    pub y: i32,
    /// Unchecked in edit mode: not drawn, but its position is kept so it
    /// comes back in the same place when checked again.
    pub hidden: bool,
    pub scale: f32,
    pub opacity: f32,
    pub timezone: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySettings {
    pub enabled: bool,
    /// "Hide all": false hides every box in game (selection and positions
    /// are kept).
    pub visible: bool,
    /// Size multiplier relative to a 1080p screen, 0.5..=3.
    pub scale: f32,
    /// Box background opacity outside edit mode, 0.15..=1.
    pub opacity: f32,
    pub hide_when_idle: bool,
    pub exceed_speed_limit_indicator: bool,
    /// Shown boxes, in catalogue order.
    pub boxes: Vec<BoxPos>,
    /// Where the edit-mode control panel was last left (None = centred).
    pub editor: Option<(i32, i32)>,
}

/// The default items stacked downwards from `(x, y)`.
pub fn stacked(ids: &[String], x: i32, y: i32, scale: f32) -> Vec<BoxPos> {
    let step = (STACK_STEP as f32 * scale).round() as i32;
    ids.iter()
        .enumerate()
        .map(|(i, id)| BoxPos {
            id: id.clone(),
            x,
            y: y + step * i as i32,
            hidden: false,
            scale,
            opacity: 0.8,
            timezone: "UTC".into(),
        })
        .collect()
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            visible: true,
            scale: 1.0,
            opacity: 0.8,
            hide_when_idle: true,
            exceed_speed_limit_indicator: false,
            boxes: stacked(&items::default_ids(), 40, 40, 1.0),
            editor: None,
        }
    }
}

impl OverlaySettings {
    pub fn sanitize(&mut self) {
        self.scale = if self.scale.is_finite() { self.scale.clamp(0.5, 3.0) } else { 1.0 };
        self.opacity = if self.opacity.is_finite() { self.opacity.clamp(0.15, 1.0) } else { 0.8 };
        let mut seen: Vec<String> = Vec::new();
        self.boxes.retain(|b| {
            let ok = items::index_of(&b.id).is_some() && !seen.contains(&b.id);
            seen.push(b.id.clone());
            ok
        });
        for b in &mut self.boxes {
            b.x = b.x.max(0);
            b.y = b.y.max(0);
            b.scale = if b.scale.is_finite() { b.scale.clamp(0.5, 3.0) } else { self.scale };
            b.opacity = if b.opacity.is_finite() { b.opacity.clamp(0.15, 1.0) } else { self.opacity };
            if !items::valid_timezone(&b.timezone) {
                b.timezone = "UTC".into();
            }
        }
        self.boxes.sort_by_key(|b| items::index_of(&b.id).unwrap_or(usize::MAX));
        if let Some((x, y)) = &mut self.editor {
            *x = (*x).max(0);
            *y = (*y).max(0);
        }
    }

    /// Checked (drawn unless "hide all" is on).
    pub fn is_shown(&self, id: &str) -> bool {
        self.boxes.iter().any(|b| b.id == id && !b.hidden)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn position(&self, id: &str) -> Option<(i32, i32)> {
        self.boxes.iter().find(|b| b.id == id).map(|b| (b.x, b.y))
    }

    pub fn set_position(&mut self, id: &str, x: i32, y: i32) {
        if let Some(b) = self.boxes.iter_mut().find(|b| b.id == id) {
            (b.x, b.y) = (x.max(0), y.max(0));
        }
    }

    pub fn ensure_box(&mut self, id: &str, at: (i32, i32)) {
        if self.boxes.iter().any(|b| b.id == id) {
            return;
        }
        self.boxes.push(BoxPos {
            id: id.to_owned(),
            x: at.0.max(0),
            y: at.1.max(0),
            hidden: true,
            scale: self.scale,
            opacity: self.opacity,
            timezone: "UTC".into(),
        });
        self.sanitize();
    }

    pub fn set_box_scale(&mut self, id: &str, scale: f32) {
        if let Some(b) = self.boxes.iter_mut().find(|b| b.id == id) {
            b.scale = scale;
            self.sanitize();
        }
    }

    pub fn set_box_opacity(&mut self, id: &str, opacity: f32) {
        if let Some(b) = self.boxes.iter_mut().find(|b| b.id == id) {
            b.opacity = opacity;
            self.sanitize();
        }
    }

    pub fn set_box_timezone(&mut self, id: &str, timezone: &str) {
        if items::valid_timezone(timezone) {
            if let Some(b) = self.boxes.iter_mut().find(|b| b.id == id) {
                b.timezone = timezone.to_owned();
            }
        }
    }

    /// Hides a shown item, or shows it again: where it was before, or at
    /// `at` the first time.
    pub fn toggle(&mut self, id: &str, at: (i32, i32)) {
        match self.boxes.iter_mut().find(|b| b.id == id) {
            Some(b) => b.hidden = !b.hidden,
            None => {
                self.boxes.push(BoxPos {
                    id: id.to_owned(),
                    x: at.0,
                    y: at.1,
                    hidden: false,
                    scale: self.scale,
                    opacity: self.opacity,
                    timezone: "UTC".into(),
                });
                self.sanitize();
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UdpSettings {
    pub enabled: bool,
    /// `host:port` receivers; joined with `,` for [`crate::config::Config`].
    pub targets: Vec<String>,
    pub hz: u32,
}

impl Default for UdpSettings {
    fn default() -> Self {
        Self { enabled: true, targets: vec!["127.0.0.1:45454".into()], hz: 30 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub overlay: OverlaySettings,
    pub udp: UdpSettings,
}

pub struct Loaded {
    pub settings: Settings,
    pub path: Option<PathBuf>,
    /// The file did not exist and was written with defaults.
    pub created: bool,
    pub warnings: Vec<String>,
}

/// `<folder of the game executable>/plugins/aigakira_telemetry.toml`, i.e.
/// next to this plugin (`bin/win_x64/plugins/` or `bin/linux_x64/plugins/`).
pub fn default_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("plugins").join(FILE_NAME))
}

const BOXES_COMMENT: &str = "\
# One box per telemetry item: `id = { x = …, y = …, scale = …, opacity = …, timezone = … }`.
# In edit mode, select a card to adjust only that box. `hidden = true` hides it
# but keeps its position and style. The Real time card's timezone can be cycled
# through common IANA zones in edit mode. Easier: use edit mode in game. Items:
";

fn boxes_comment() -> String {
    let mut c = String::from("\n");
    c.push_str(BOXES_COMMENT);
    for i in ITEMS {
        c.push_str(&format!("#   {:<15} {}\n", i.id, i.label));
    }
    c
}

fn boxes_table(boxes: &[BoxPos]) -> Table {
    let mut t = Table::new();
    for b in boxes {
        let mut pos = InlineTable::new();
        pos.insert("x", i64::from(b.x).into());
        pos.insert("y", i64::from(b.y).into());
        pos.insert("scale", ((f64::from(b.scale) * 100.0).round() / 100.0).into());
        pos.insert("opacity", ((f64::from(b.opacity) * 100.0).round() / 100.0).into());
        pos.insert("timezone", b.timezone.as_str().into());
        if b.hidden {
            pos.insert("hidden", true.into());
        }
        t.insert(&b.id, value(pos));
    }
    t.decor_mut().set_prefix(boxes_comment());
    t
}

pub fn template() -> String {
    let d = Settings::default();
    let mut boxes = String::new();
    for b in &d.overlay.boxes {
        boxes.push_str(&format!("{} = {{ x = {}, y = {} }}\n", b.id, b.x, b.y));
    }
    format!(
        r#"# aigakira-telemetry-scs-udp settings
# Created automatically on the first start. Edit and save; then restart the
# game for `enabled` and [udp] changes. The other [overlay] values are picked
# up within a second while the game runs.

[overlay]
# In-game telemetry boxes. On by default; set to false and restart the game
# to turn them off (nothing is hooked into the game then).
# In game: Ctrl+Shift+Backspace toggles edit mode. The mouse then moves the
# overlay's own cursor (the game ignores it until you leave edit mode): drag
# any box to move it, and use the control panel to show/hide items. Edit mode
# saves its changes back into this file.
enabled = {enabled}

# Default box size, 0.5 to 3.0; per-box sliders override this value.
scale = {scale:.1}

# Default box background opacity, 0.15 to 1.0; per-box sliders override this.
opacity = {opacity:.1}

# Hide the boxes while no truck is active (main menu, profile selection).
# Edit mode always shows them.
hide_when_idle = {hide}

# "Hide all": false hides every box in game but keeps which ones are checked
# and where they are (the Hide all / Show all button in edit mode).
visible = {visible}

# Flash a red screen border when speed exceeds the active speed limit.
exceed_speed_limit_indicator = {speed_alert}
{boxes_comment}[overlay.boxes]
{boxes}
[udp]
# Stream telemetry as JSON over UDP (for the Touch Portal plugin or
# tools/udp_listen.py).
enabled = {udp_enabled}

# Receivers as "host:port". The AIGAKIRA_UDP_TARGET environment variable
# (comma-separated) overrides this.
targets = {targets}

# Datagrams per second, 1 to 120. AIGAKIRA_UDP_HZ overrides this.
hz = {hz}
"#,
        enabled = d.overlay.enabled,
        scale = d.overlay.scale,
        opacity = d.overlay.opacity,
        hide = d.overlay.hide_when_idle,
        visible = d.overlay.visible,
        speed_alert = d.overlay.exceed_speed_limit_indicator,
        boxes_comment = boxes_comment(),
        udp_enabled = d.udp.enabled,
        targets = string_array(&d.udp.targets),
        hz = d.udp.hz,
    )
}

fn string_array(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

/// Loads the file, creating it from [`template`] if it does not exist.
/// Never fails: problems become warnings and defaults are used.
pub fn load_or_create(path: Option<PathBuf>) -> Loaded {
    let mut warnings = Vec::new();
    let Some(path) = path else {
        warnings.push("cannot determine the plugin folder; using default settings".into());
        return Loaded { settings: Settings::default(), path: None, created: false, warnings };
    };
    let mut created = false;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let t = template();
            match write_atomic(&path, &t) {
                Ok(()) => created = true,
                Err(e) => warnings.push(format!("cannot create {}: {e}", path.display())),
            }
            t
        }
        Err(e) => {
            warnings.push(format!("cannot read {}: {e}; using defaults", path.display()));
            return Loaded { settings: Settings::default(), path: Some(path), created, warnings };
        }
    };
    let settings = parse(&text, &mut warnings);
    Loaded { settings, path: Some(path), created, warnings }
}

fn int_of(item: Option<&Item>) -> Option<i32> {
    item.and_then(Item::as_integer).and_then(|v| i32::try_from(v).ok())
}

/// `x`/`y` of an inline table (`{ x = 1, y = 2 }`) or a sub-table.
fn xy(item: &Item) -> Option<(i32, i32)> {
    if let Some(t) = item.as_inline_table() {
        let get = |k: &str| t.get(k).and_then(toml_edit::Value::as_integer).and_then(|v| i32::try_from(v).ok());
        return Some((get("x")?, get("y")?));
    }
    let t = item.as_table()?;
    Some((int_of(t.get("x"))?, int_of(t.get("y"))?))
}

/// Optional `hidden = true` of a box entry.
fn hidden_of(item: &Item) -> bool {
    let v = match item.as_inline_table() {
        Some(t) => t.get("hidden").and_then(toml_edit::Value::as_bool),
        None => item.as_table().and_then(|t| t.get("hidden")).and_then(Item::as_bool),
    };
    v.unwrap_or(false)
}

fn number_of(item: &Item, key: &str) -> Option<f32> {
    let value = if let Some(table) = item.as_inline_table() {
        table.get(key).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
    } else {
        item.as_table()
            .and_then(|table| table.get(key))
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
    }? as f32;
    value.is_finite().then_some(value)
}

fn string_of(item: &Item, key: &str) -> Option<String> {
    if let Some(table) = item.as_inline_table() {
        table.get(key).and_then(toml_edit::Value::as_str).map(str::to_owned)
    } else {
        item.as_table().and_then(|table| table.get(key)).and_then(Item::as_str).map(str::to_owned)
    }
}

/// Parses settings text; invalid or missing values fall back to defaults
/// with a warning.
pub fn parse(text: &str, warnings: &mut Vec<String>) -> Settings {
    let mut s = Settings::default();
    let doc = match DocumentMut::from_str(text) {
        Ok(d) => d,
        Err(e) => {
            warnings.push(format!("{FILE_NAME} is not valid TOML ({}); using defaults", e.to_string().trim()));
            return s;
        }
    };
    let get = |table: &str, key: &str| doc.get(table).and_then(|t| t.get(key));
    let mut bad = |table: &str, key: &str, want: &str| warnings.push(format!("{FILE_NAME}: {table}.{key} must be {want}; using the default"));

    macro_rules! read {
        ($table:literal, $key:literal, $conv:expr, $want:literal, $slot:expr) => {
            if let Some(item) = get($table, $key) {
                match $conv(item) {
                    Some(v) => $slot = v,
                    None => bad($table, $key, $want),
                }
            }
        };
    }
    let as_bool = |i: &Item| i.as_bool();
    let as_f32 = |i: &Item| i.as_float().or_else(|| i.as_integer().map(|v| v as f64)).map(|v| v as f32);
    let as_strings = |i: &Item| {
        i.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect::<Vec<_>>())
    };
    let as_u32 = |i: &Item| i.as_integer().and_then(|v| u32::try_from(v).ok());

    read!("overlay", "enabled", as_bool, "true or false", s.overlay.enabled);
    read!("overlay", "scale", as_f32, "a number", s.overlay.scale);
    read!("overlay", "opacity", as_f32, "a number", s.overlay.opacity);
    read!("overlay", "hide_when_idle", as_bool, "true or false", s.overlay.hide_when_idle);
    read!("overlay", "visible", as_bool, "true or false", s.overlay.visible);
    read!("overlay", "exceed_speed_limit_indicator", as_bool, "true or false", s.overlay.exceed_speed_limit_indicator);
    for b in &mut s.overlay.boxes {
        b.scale = s.overlay.scale;
        b.opacity = s.overlay.opacity;
    }
    read!("udp", "enabled", as_bool, "true or false", s.udp.enabled);
    read!("udp", "targets", as_strings, "a list of \"host:port\" strings", s.udp.targets);
    read!("udp", "hz", as_u32, "a whole number", s.udp.hz);
    if let Some(e) = get("overlay", "editor") {
        match xy(e) {
            Some(p) => s.overlay.editor = Some(p),
            None => bad("overlay", "editor", "{ x = …, y = … }"),
        }
    }

    // Boxes: [overlay.boxes], or the v0.3 single-panel form.
    if let Some(boxes) = get("overlay", "boxes") {
        let mut list = Vec::new();
        match boxes.as_table_like() {
            Some(t) => {
                for (id, item) in t.iter() {
                    if items::index_of(id).is_none() {
                        warnings.push(format!("{FILE_NAME}: unknown overlay item {id:?} ignored"));
                        continue;
                    }
                    match xy(item) {
                        Some((x, y)) => list.push(BoxPos {
                            id: id.to_owned(),
                            x,
                            y,
                            hidden: hidden_of(item),
                            scale: number_of(item, "scale").unwrap_or(s.overlay.scale),
                            opacity: number_of(item, "opacity").unwrap_or(s.overlay.opacity),
                            timezone: string_of(item, "timezone").unwrap_or_else(|| "UTC".into()),
                        }),
                        None => warnings.push(format!("{FILE_NAME}: overlay.boxes.{id} must be {{ x = …, y = … }}; ignored")),
                    }
                }
            }
            None => bad("overlay", "boxes", "a table of `id = { x = …, y = … }`"),
        }
        s.overlay.boxes = list;
    } else if let Some(old) = get("overlay", "items") {
        let ids = as_strings(old).unwrap_or_default();
        let (x, y) = (int_of(get("overlay", "x")).unwrap_or(40), int_of(get("overlay", "y")).unwrap_or(40));
        for id in ids.iter().filter(|id| items::index_of(id).is_none()) {
            warnings.push(format!("{FILE_NAME}: unknown overlay item {id:?} ignored"));
        }
        let ids: Vec<String> = ids.into_iter().filter(|id| items::index_of(id).is_some()).collect();
        s.overlay.boxes = stacked(&ids, x, y, s.overlay.scale.clamp(0.5, 3.0));
        for b in &mut s.overlay.boxes {
            b.opacity = s.overlay.opacity;
        }
    }
    s.overlay.sanitize();
    s
}

/// Writes the `[overlay]` values into the file, keeping everything else
/// (comments, `[udp]`, unknown keys) as the user left it. Converts a v0.3
/// file (`x`, `y`, `items`) to `[overlay.boxes]`.
pub fn save_overlay(path: &Path, o: &OverlaySettings) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => template(),
        Err(e) => return Err(e),
    };
    let mut doc = DocumentMut::from_str(&text)
        .or_else(|_| DocumentMut::from_str(&template()))
        .map_err(io::Error::other)?;
    if !doc.contains_table("overlay") {
        doc["overlay"] = toml_edit::table();
    }
    let t = doc["overlay"].as_table_mut().ok_or_else(|| io::Error::other("[overlay] is not a table"))?;
    t["enabled"] = value(o.enabled);
    t["scale"] = value((f64::from(o.scale) * 10.0).round() / 10.0);
    t["opacity"] = value((f64::from(o.opacity) * 100.0).round() / 100.0);
    t["hide_when_idle"] = value(o.hide_when_idle);
    t["visible"] = value(o.visible);
    t["exceed_speed_limit_indicator"] = value(o.exceed_speed_limit_indicator);
    for old in ["x", "y", "items"] {
        t.remove(old);
    }
    match o.editor {
        Some((x, y)) => {
            let mut pos = InlineTable::new();
            pos.insert("x", i64::from(x).into());
            pos.insert("y", i64::from(y).into());
            t["editor"] = value(pos);
        }
        None => {
            t.remove("editor");
        }
    }
    let mut boxes = boxes_table(&o.boxes);
    if let Some(old) = t.get("boxes").and_then(Item::as_table) {
        boxes.decor_mut().clone_from(old.decor());
    }
    t.insert("boxes", Item::Table(boxes));
    write_atomic(path, &doc.to_string())
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aigakira-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE_NAME)
    }

    /// `cargo test print_template -- --ignored --nocapture` shows the file
    /// a first start creates.
    #[test]
    #[ignore]
    fn print_template() {
        print!("{}", template());
    }

    #[test]
    fn first_run_creates_commented_file_with_overlay_on() {
        let path = tmp("create");
        let l = load_or_create(Some(path.clone()));
        assert!(l.created);
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.settings, Settings::default());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[overlay]\n# In-game telemetry boxes"));
        assert!(l.settings.overlay.enabled, "overlay on by default");
        assert!(text.contains("#   speed"), "lists the available items");
        assert!(text.contains("speed = { x = 40, y = 40 }"));
        let l2 = load_or_create(Some(path.clone()));
        assert!(!l2.created);
        assert_eq!(l2.settings, l.settings);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_keeps_comments_and_other_sections() {
        let path = tmp("save");
        load_or_create(Some(path.clone()));
        let edited = std::fs::read_to_string(&path).unwrap().replace("hz = 30", "hz = 60  # mine");
        std::fs::write(&path, edited).unwrap();

        let mut o = OverlaySettings {
            enabled: true,
            scale: 1.25,
            editor: Some((700, 300)),
            exceed_speed_limit_indicator: true,
            ..OverlaySettings::default()
        };
        o.toggle("throttle", (900, 500));
        o.set_position("speed", 123, 7);
        o.set_box_scale("speed", 1.7);
        o.set_box_opacity("speed", 0.4);
        o.set_box_timezone("speed", "America/New_York");
        save_overlay(&path, &o).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# In-game telemetry boxes"), "comments kept");
        assert!(text.contains("#   speed"), "box list comment kept");
        assert!(text.contains("hz = 60  # mine"), "udp untouched");
        let mut w = Vec::new();
        let s = parse(&text, &mut w);
        assert!(w.is_empty(), "{w:?}");
        assert!(s.overlay.enabled);
        assert_eq!(s.overlay.position("speed"), Some((123, 7)));
        assert_eq!(s.overlay.position("throttle"), Some((900, 500)));
        assert_eq!(s.overlay.editor, Some((700, 300)));
        assert!(s.overlay.exceed_speed_limit_indicator);
        assert_eq!(s.udp.hz, 60);
        assert_eq!(s.overlay.boxes, o.boxes);
        assert_eq!(s.overlay.boxes[0].timezone, "America/New_York");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn unchecked_boxes_keep_their_place_and_hide_all_is_saved() {
        let path = tmp("hidden");
        load_or_create(Some(path.clone()));
        let mut o = OverlaySettings { enabled: true, ..OverlaySettings::default() };
        o.set_position("gear", 777, 555);
        o.toggle("gear", (0, 0)); // uncheck
        assert!(!o.is_shown("gear"));
        assert_eq!(o.position("gear"), Some((777, 555)), "position kept while hidden");
        o.visible = false;
        save_overlay(&path, &o).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("gear = { x = 777, y = 555, scale = 1.0, opacity = 0.8, timezone = \"UTC\", hidden = true }"),
            "{text}"
        );
        assert!(text.contains("visible = false"));
        let mut w = Vec::new();
        let mut back = parse(&text, &mut w).overlay;
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, o);
        back.toggle("gear", (0, 0)); // check again
        assert!(back.is_shown("gear"));
        assert_eq!(back.position("gear"), Some((777, 555)), "comes back where it was");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn v03_single_panel_file_is_converted() {
        let path = tmp("migrate");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = "[overlay]\n# keep me\nenabled = true\nx = 300\ny = 200\nscale = 1.0\nopacity = 0.5\nitems = [\"speed\", \"gear\", \"bogus\"]\n\n[udp]\nhz = 30\n";
        std::fs::write(&path, old).unwrap();
        let mut w = Vec::new();
        let s = parse(old, &mut w);
        assert_eq!(w.len(), 1, "unknown item warned: {w:?}");
        assert_eq!(s.overlay.position("speed"), Some((300, 200)));
        assert_eq!(s.overlay.position("gear"), Some((300, 200 + STACK_STEP)));
        assert_eq!(s.overlay.boxes[0].opacity, 0.5);
        save_overlay(&path, &s.overlay).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("items ="), "old keys removed:\n{text}");
        assert!(text.contains("[overlay.boxes]"));
        assert!(text.contains("# keep me"));
        let mut w = Vec::new();
        assert_eq!(parse(&text, &mut w).overlay, s.overlay);
        assert!(w.is_empty(), "{w:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn bad_values_warn_and_fall_back() {
        let mut w = Vec::new();
        let s = parse(
            "[overlay]\nenabled = \"yes\"\nscale = 9\n[overlay.boxes]\nspeed = { x = 1, y = 2 }\nbogus = { x = 1, y = 2 }\ngear = 5\n[udp]\nhz = -1\n",
            &mut w,
        );
        assert!(s.overlay.enabled, "invalid value → the default (on)");
        assert_eq!(s.overlay.scale, 3.0);
        assert_eq!(
            s.overlay.boxes,
            vec![BoxPos { id: "speed".into(), x: 1, y: 2, hidden: false, scale: 3.0, opacity: 0.8, timezone: "UTC".into() }]
        );
        assert_eq!(s.udp.hz, 30);
        assert_eq!(w.len(), 4, "{w:?}");

        let mut w = Vec::new();
        assert_eq!(parse("not [valid", &mut w), Settings::default());
        assert_eq!(w.len(), 1);
    }
}
