//! Edit-mode mouse for the Windows build.
//!
//! ETS2/ATS read the mouse through DirectInput 8 and draw their own cursor,
//! so the Windows cursor position does not follow what the player sees, and
//! clicks would also go to the game. While the overlay is in edit mode this
//! module takes the game's own mouse input instead: `GetDeviceState` and
//! `GetDeviceData` of DirectInput mouse devices are hooked (vtable slots 9
//! and 10, found through a throw-away device, like the swap chain), the
//! movement and left button are handed to the overlay, and the game receives
//! "no movement, no buttons" until edit mode ends. Outside edit mode the
//! hooks only pass the calls through.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Devices::HumanInterfaceDevice::{
    DI8DEVTYPE_MOUSE, DIDEVCAPS, DIDEVICEOBJECTDATA, DIRECTINPUT_VERSION, DirectInput8Create, GUID_SysMouse, IDirectInput8A,
    IDirectInput8W, IDirectInputDevice8A, IDirectInputDevice8W,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::core::{HRESULT, Interface, PCWSTR};

const SLOT_GET_DEVICE_STATE: usize = 9;
const SLOT_GET_DEVICE_DATA: usize = 10;
const SLOT_GET_CAPABILITIES: usize = 3;
const DIGDD_PEEK: u32 = 1;

type GetStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> HRESULT;
type GetDataFn = unsafe extern "system" fn(*mut c_void, u32, *mut DIDEVICEOBJECTDATA, *mut u32, u32) -> HRESULT;
type GetCapsFn = unsafe extern "system" fn(*mut c_void, *mut DIDEVCAPS) -> HRESULT;

/// Set by the renderer every frame: true while the overlay is in edit mode.
static EDITING: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static DX: AtomicI32 = AtomicI32::new(0);
static DY: AtomicI32 = AtomicI32::new(0);
static LEFT: AtomicBool = AtomicBool::new(false);
/// Milliseconds (unix) of the last mouse read seen while editing.
static LAST_SEEN_MS: AtomicU64 = AtomicU64::new(0);

/// One patched vtable slot: (vtable address, slot, original, ours).
static PATCHES: Mutex<Vec<(usize, usize, usize, usize)>> = Mutex::new(Vec::new());
static ORIG_STATE_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_STATE_A: AtomicUsize = AtomicUsize::new(0);
static ORIG_DATA_W: AtomicUsize = AtomicUsize::new(0);
static ORIG_DATA_A: AtomicUsize = AtomicUsize::new(0);
/// device pointer → is a mouse
static KIND: Mutex<Option<HashMap<usize, bool>>> = Mutex::new(None);

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn set_editing(on: bool) {
    if EDITING.swap(on, Ordering::AcqRel) != on {
        DX.store(0, Ordering::Release);
        DY.store(0, Ordering::Release);
        LEFT.store(false, Ordering::Release);
    }
}

/// Mouse input taken from the game since the last call: (dx, dy, left button).
/// `None` if the hooks are not active or the game has not read a DirectInput
/// mouse recently (then the caller falls back to the Windows cursor).
pub(crate) fn take() -> Option<(i32, i32, bool)> {
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    let seen = LAST_SEEN_MS.load(Ordering::Acquire);
    if seen == 0 || now_ms().saturating_sub(seen) > 500 {
        return None;
    }
    Some((DX.swap(0, Ordering::AcqRel), DY.swap(0, Ordering::AcqRel), LEFT.load(Ordering::Acquire)))
}

unsafe fn patch(vtable: usize, slot: usize, hook: usize) -> Result<usize, String> {
    let entry = (vtable as *mut usize).wrapping_add(slot);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe {
        VirtualProtect(entry as *const c_void, size_of::<usize>(), PAGE_READWRITE, &mut old).map_err(|e| format!("VirtualProtect: {e}"))?;
        let orig = entry.read_volatile();
        entry.write_volatile(hook);
        let _ = VirtualProtect(entry as *const c_void, size_of::<usize>(), old, &mut old);
        PATCHES.lock().unwrap_or_else(|p| p.into_inner()).push((vtable, slot, orig, hook));
        Ok(orig)
    }
}

/// Hooks the DirectInput 8 mouse device methods (W and, if separate, A).
pub(crate) fn install() -> Result<(), String> {
    unsafe {
        let hinst = GetModuleHandleW(PCWSTR::null()).map_err(|e| format!("GetModuleHandle: {e}"))?;
        // Unicode interface
        let mut raw: *mut c_void = std::ptr::null_mut();
        DirectInput8Create(hinst.into(), DIRECTINPUT_VERSION, &IDirectInput8W::IID, &mut raw, None)
            .map_err(|e| format!("DirectInput8Create: {e}"))?;
        let di = IDirectInput8W::from_raw(raw);
        let mut dev: Option<IDirectInputDevice8W> = None;
        di.CreateDevice(&GUID_SysMouse, &mut dev, None).map_err(|e| format!("CreateDevice(mouse): {e}"))?;
        let dev = dev.ok_or("no mouse device")?;
        let vt_w = *(dev.as_raw() as *const usize);
        ORIG_STATE_W.store(patch(vt_w, SLOT_GET_DEVICE_STATE, state_w as *const () as usize)?, Ordering::Release);
        ORIG_DATA_W.store(patch(vt_w, SLOT_GET_DEVICE_DATA, data_w as *const () as usize)?, Ordering::Release);
        drop(dev);
        drop(di);

        // ANSI interface, in case the game uses it and it has its own vtable
        let mut raw: *mut c_void = std::ptr::null_mut();
        if DirectInput8Create(hinst.into(), DIRECTINPUT_VERSION, &IDirectInput8A::IID, &mut raw, None).is_ok() && !raw.is_null() {
            let di = IDirectInput8A::from_raw(raw);
            let mut dev: Option<IDirectInputDevice8A> = None;
            if di.CreateDevice(&GUID_SysMouse, &mut dev, None).is_ok() {
                if let Some(dev) = dev {
                    let vt_a = *(dev.as_raw() as *const usize);
                    if vt_a != vt_w {
                        if let Ok(o) = patch(vt_a, SLOT_GET_DEVICE_STATE, state_a as *const () as usize) {
                            ORIG_STATE_A.store(o, Ordering::Release);
                        }
                        if let Ok(o) = patch(vt_a, SLOT_GET_DEVICE_DATA, data_a as *const () as usize) {
                            ORIG_DATA_A.store(o, Ordering::Release);
                        }
                    }
                }
            }
        }
    }
    ACTIVE.store(true, Ordering::Release);
    Ok(())
}

pub(crate) fn uninstall() {
    ACTIVE.store(false, Ordering::Release);
    EDITING.store(false, Ordering::Release);
    let patches = std::mem::take(&mut *PATCHES.lock().unwrap_or_else(|p| p.into_inner()));
    for (vtable, slot, orig, ours) in patches {
        let entry = (vtable as *mut usize).wrapping_add(slot);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            if VirtualProtect(entry as *const c_void, size_of::<usize>(), PAGE_READWRITE, &mut old).is_ok() {
                if entry.read_volatile() == ours {
                    entry.write_volatile(orig);
                }
                let _ = VirtualProtect(entry as *const c_void, size_of::<usize>(), old, &mut old);
            }
        }
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    for _ in 0..2 {
        while IN_FLIGHT.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Whether `this` is a mouse device (cached; GetCapabilities has the same
/// layout in the A and W interfaces).
unsafe fn is_mouse(this: *mut c_void) -> bool {
    let key = this as usize;
    let mut kind = KIND.lock().unwrap_or_else(|p| p.into_inner());
    let map = kind.get_or_insert_with(HashMap::new);
    if let Some(&m) = map.get(&key) {
        return m;
    }
    let mut caps = DIDEVCAPS { dwSize: size_of::<DIDEVCAPS>() as u32, ..Default::default() };
    let mouse = unsafe {
        let vtable = *(this as *const *const usize);
        let get_caps: GetCapsFn = std::mem::transmute(*vtable.add(SLOT_GET_CAPABILITIES));
        get_caps(this, &mut caps).is_ok() && caps.dwDevType & 0xFF == DI8DEVTYPE_MOUSE
    };
    if map.len() > 64 {
        map.clear();
    }
    map.insert(key, mouse);
    mouse
}

/// `DIMOUSESTATE` (16 bytes) / `DIMOUSESTATE2` (20): lX, lY, lZ, buttons…
unsafe fn take_state(this: *mut c_void, cb: u32, data: *mut c_void) {
    if !EDITING.load(Ordering::Acquire) || data.is_null() || !(cb == 16 || cb == 20) {
        return;
    }
    unsafe {
        if !is_mouse(this) {
            return;
        }
        let axes = data as *mut i32;
        DX.fetch_add(axes.read_unaligned(), Ordering::AcqRel);
        DY.fetch_add(axes.add(1).read_unaligned(), Ordering::AcqRel);
        LEFT.store(*(data as *const u8).add(12) & 0x80 != 0, Ordering::Release);
        std::ptr::write_bytes(data as *mut u8, 0, cb as usize);
    }
    LAST_SEEN_MS.store(now_ms(), Ordering::Release);
}

/// Buffered mouse events: dwOfs 0 = X, 4 = Y, 12 = button 0.
unsafe fn take_events(this: *mut c_void, cb_obj: u32, events: *mut DIDEVICEOBJECTDATA, count: *mut u32, flags: u32) {
    if !EDITING.load(Ordering::Acquire) || events.is_null() || count.is_null() || flags & DIGDD_PEEK != 0 || cb_obj < 8 {
        return;
    }
    unsafe {
        if !is_mouse(this) {
            return;
        }
        for i in 0..*count as usize {
            let p = (events as *const u8).add(i * cb_obj as usize) as *const u32;
            let (ofs, val) = (p.read_unaligned(), p.add(1).read_unaligned());
            match ofs {
                0 => {
                    DX.fetch_add(val as i32, Ordering::AcqRel);
                }
                4 => {
                    DY.fetch_add(val as i32, Ordering::AcqRel);
                }
                12 => LEFT.store(val & 0x80 != 0, Ordering::Release),
                _ => {}
            }
        }
        *count = 0; // the game sees no mouse events while editing
    }
    LAST_SEEN_MS.store(now_ms(), Ordering::Release);
}

macro_rules! hooks {
    ($state:ident, $data:ident, $orig_state:ident, $orig_data:ident) => {
        unsafe extern "system" fn $state(this: *mut c_void, cb: u32, data: *mut c_void) -> HRESULT {
            IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
            let orig: GetStateFn = unsafe { std::mem::transmute($orig_state.load(Ordering::Acquire)) };
            let hr = unsafe { orig(this, cb, data) };
            if hr.is_ok() && ACTIVE.load(Ordering::Acquire) {
                let _ = std::panic::catch_unwind(|| unsafe { take_state(this, cb, data) });
            }
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            hr
        }

        unsafe extern "system" fn $data(this: *mut c_void, cb: u32, ev: *mut DIDEVICEOBJECTDATA, n: *mut u32, flags: u32) -> HRESULT {
            IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
            let orig: GetDataFn = unsafe { std::mem::transmute($orig_data.load(Ordering::Acquire)) };
            let hr = unsafe { orig(this, cb, ev, n, flags) };
            if hr.is_ok() && ACTIVE.load(Ordering::Acquire) {
                let _ = std::panic::catch_unwind(|| unsafe { take_events(this, cb, ev, n, flags) });
            }
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
            hr
        }
    };
}

hooks!(state_w, data_w, ORIG_STATE_W, ORIG_DATA_W);
hooks!(state_a, data_a, ORIG_STATE_A, ORIG_DATA_A);
