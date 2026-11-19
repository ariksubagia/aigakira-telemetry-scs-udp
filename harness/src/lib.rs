//! A fake SCS game for testing the plugin outside the game: implements the
//! telemetry SDK host side (`scs_telemetry_init` parameters, event and
//! channel registration) and drives the registered callbacks with values.

#![allow(clippy::missing_safety_doc)]

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::mem::MaybeUninit;
use std::sync::Mutex;

use scs_sdk_sys::*;

pub type InitFn = unsafe extern "system" fn(ScsU32, *const ScsTelemetryInitParams) -> ScsResult;
pub type ShutdownFn = unsafe extern "system" fn();

#[derive(Default)]
struct Registry {
    events: HashMap<ScsEvent, (usize, usize)>,
    channels: Vec<(String, ScsU32, ScsValueType, usize, usize)>,
}

static REG: Mutex<Option<Registry>> = Mutex::new(None);
pub static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "system" fn log(kind: ScsLogType, msg: ScsString) {
    let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned();
    let tag = match kind {
        SCS_LOG_TYPE_WARNING => "warning",
        SCS_LOG_TYPE_ERROR => "error",
        _ => "message",
    };
    println!("  [game.log {tag}] {text}");
    LOG.lock().unwrap().push(text);
}

unsafe extern "system" fn register_event(ev: ScsEvent, cb: ScsTelemetryEventCallback, ctx: ScsContext) -> ScsResult {
    REG.lock().unwrap().get_or_insert_default().events.insert(ev, (cb as usize, ctx as usize));
    SCS_RESULT_OK
}

unsafe extern "system" fn unregister_event(ev: ScsEvent) -> ScsResult {
    REG.lock().unwrap().get_or_insert_default().events.remove(&ev);
    SCS_RESULT_OK
}

unsafe extern "system" fn register_channel(
    name: ScsString,
    index: ScsU32,
    ty: ScsValueType,
    _flags: ScsU32,
    cb: ScsTelemetryChannelCallback,
    ctx: ScsContext,
) -> ScsResult {
    let n = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    REG.lock().unwrap().get_or_insert_default().channels.push((n, index, ty, cb as usize, ctx as usize));
    SCS_RESULT_OK
}

unsafe extern "system" fn unregister_channel(name: ScsString, index: ScsU32, _ty: ScsValueType) -> ScsResult {
    let n = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
    if let Some(r) = REG.lock().unwrap().as_mut() {
        r.channels.retain(|c| !(c.0 == n && c.1 == index));
    }
    SCS_RESULT_OK
}

/// Calls the plugin's `scs_telemetry_init` as ETS2 (telemetry schema 1.22).
pub unsafe fn init(f: InitFn) -> ScsResult {
    *REG.lock().unwrap() = Some(Registry::default());
    LOG.lock().unwrap().clear();
    let params = ScsTelemetryInitParamsV101 {
        common: ScsSdkInitParamsV100 {
            game_name: c"Euro Truck Simulator 2 (overlay harness)".as_ptr(),
            game_id: SCS_GAME_ID_EUT2.as_ptr().cast::<c_char>(),
            game_version: make_version(1, 22),
            padding: MaybeUninit::zeroed(),
            log,
        },
        register_for_event: register_event,
        unregister_from_event: unregister_event,
        register_for_channel: register_channel,
        unregister_from_channel: unregister_channel,
    };
    unsafe { f(SCS_TELEMETRY_VERSION_1_01, (&raw const params).cast()) }
}

fn value(ty: ScsValueType, data: ScsValueData) -> ScsValue {
    ScsValue { type_: ty, padding: MaybeUninit::zeroed(), value: data }
}

fn fire_event(ev: ScsEvent, info: *const c_void) {
    let entry = REG.lock().unwrap().as_ref().and_then(|r| r.events.get(&ev).copied());
    if let Some((cb, ctx)) = entry {
        let cb: ScsTelemetryEventCallback = unsafe { std::mem::transmute(cb) };
        unsafe { cb(ev, info, ctx as ScsContext) };
    }
}

fn send(name: &str, v: ScsValue) {
    let matches: Vec<_> = REG
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.channels.iter().filter(|c| c.0 == name && c.2 == v.type_).map(|c| (c.1, c.3, c.4)).collect())
        .unwrap_or_default();
    let cname = std::ffi::CString::new(name).unwrap();
    for (index, cb, ctx) in matches {
        let cb: ScsTelemetryChannelCallback = unsafe { std::mem::transmute(cb) };
        unsafe { cb(cname.as_ptr(), index, &v, ctx as ScsContext) };
    }
}

pub fn send_f32(name: &str, x: f32) {
    send(name, value(SCS_VALUE_TYPE_FLOAT, ScsValueData { value_float: ScsValueFloat { value: x } }));
}
pub fn send_bool(name: &str, x: bool) {
    send(name, value(SCS_VALUE_TYPE_BOOL, ScsValueData { value_bool: ScsValueBool { value: u8::from(x) } }));
}
pub fn send_s32(name: &str, x: i32) {
    send(name, value(SCS_VALUE_TYPE_S32, ScsValueData { value_s32: ScsValueS32 { value: x } }));
}

/// Truck configuration + simulation started, as when a profile is loaded.
pub fn load_truck() {
    let attrs = [
        ScsNamedValue {
            name: c"fuel.capacity".as_ptr(),
            index: SCS_U32_NIL,
            padding: MaybeUninit::zeroed(),
            value: value(SCS_VALUE_TYPE_FLOAT, ScsValueData { value_float: ScsValueFloat { value: 800.0 } }),
        },
        ScsNamedValue {
            name: c"rpm.limit".as_ptr(),
            index: SCS_U32_NIL,
            padding: MaybeUninit::zeroed(),
            value: value(SCS_VALUE_TYPE_FLOAT, ScsValueData { value_float: ScsValueFloat { value: 2500.0 } }),
        },
        ScsNamedValue {
            name: std::ptr::null(),
            index: 0,
            padding: MaybeUninit::zeroed(),
            value: value(SCS_VALUE_TYPE_INVALID, ScsValueData { value_u32: ScsValueU32 { value: 0 } }),
        },
    ];
    let cfg = ScsTelemetryConfiguration { id: c"truck".as_ptr(), attributes: attrs.as_ptr() };
    fire_event(SCS_TELEMETRY_EVENT_CONFIGURATION, (&raw const cfg).cast());
    fire_event(SCS_TELEMETRY_EVENT_STARTED, std::ptr::null());
}

/// One simulated game frame of telemetry.
pub fn game_frame(speed_ms: f32, right_blinker: bool) {
    send_f32("truck.speed", speed_ms);
    send_f32("truck.engine.rpm", 1450.0);
    send_s32("truck.engine.gear", 7);
    send_s32("truck.displayed.gear", 7);
    send_f32("truck.fuel.amount", 500.0);
    send_f32("truck.navigation.speed.limit", 80.0 / 3.6);
    send_bool("truck.rblinker", right_blinker);
    send_bool("truck.light.rblinker", right_blinker);
    send_bool("truck.engine.enabled", true);
    fire_event(SCS_TELEMETRY_EVENT_FRAME_END, std::ptr::null());
}

pub fn log_contains(s: &str) -> bool {
    LOG.lock().unwrap().iter().any(|l| l.contains(s))
}

/// Writes a binary PPM (P6) from RGBA rows for visual inspection.
pub fn write_ppm(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::write(path, out).unwrap();
}

/// Settings file the plugin uses: `<exe dir>/plugins/aigakira_telemetry.toml`.
pub fn settings_path() -> std::path::PathBuf {
    std::env::current_exe().unwrap().parent().unwrap().join("plugins").join("aigakira_telemetry.toml")
}

/// Number of pixels whose colour differs from `bg` (i.e. covered by overlay).
pub fn covered(rgba: &[u8], bg: [u8; 3]) -> usize {
    rgba.chunks_exact(4).filter(|p| p[..3] != bg).count()
}

/// Average colour of a rectangle (RGBA rows, `stride` bytes per row).
pub fn avg(rgba: &[u8], stride: usize, x0: u32, y0: u32, x1: u32, y1: u32) -> [f32; 3] {
    let mut sum = [0f64; 3];
    let mut n = 0f64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y as usize * stride + x as usize * 4;
            for c in 0..3 {
                sum[c] += f64::from(rgba[i + c]);
            }
            n += 1.0;
        }
    }
    [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
}
