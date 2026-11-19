//! OpenGL/GLX backend (Linux build: the native game).
//!
//! The native game loads libGL with `dlopen` and resolves `glXSwapBuffers`
//! at runtime, long before telemetry plugins are loaded, so there is no
//! import table to redirect. Instead the first 14 bytes of every
//! `glXSwapBuffers` entry point the game could have resolved (libGL's and,
//! with libglvnd, libGLX's) are replaced by an absolute jump to a hook. The
//! hook draws the panel into the back buffer of the current context, then
//! restores the original bytes, calls the real function and re-applies the
//! jump ("hot patching"; swaps of one window only happen on one thread).
//!
//! Input comes from a separate XCB connection (pointer + keymap queries).
//! XCB reports errors per request; Xlib's default error handler would exit
//! the game.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{Core, Input, Shared};

const PATCH_LEN: usize = 14;
const MAX_SLOTS: usize = 3;
const GLX_WIDTH: c_int = 0x801D;
const GLX_HEIGHT: c_int = 0x801E;

type SwapFn = unsafe extern "C" fn(*mut c_void, c_ulong);
type GetProcFn = unsafe extern "C" fn(*const u8) -> *const c_void;
type QueryDrawableFn = unsafe extern "C" fn(*mut c_void, c_ulong, c_int, *mut c_uint);
type CurrentContextFn = unsafe extern "C" fn() -> *mut c_void;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static STATE: Mutex<Option<Renderer>> = Mutex::new(None);

struct Slot {
    addr: AtomicUsize,
    bytes: Mutex<([u8; PATCH_LEN], [u8; PATCH_LEN])>, // (original, jump)
}

impl Slot {
    const fn new() -> Self {
        Self { addr: AtomicUsize::new(0), bytes: Mutex::new(([0; PATCH_LEN], [0; PATCH_LEN])) }
    }
}

static SLOTS: [Slot; MAX_SLOTS] = [Slot::new(), Slot::new(), Slot::new()];

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Hook {
    shared: Arc<Shared>,
}

// ---------------------------------------------------------------------------
// dynamic symbols
// ---------------------------------------------------------------------------

unsafe fn dl_noload(name: &CStr) -> *mut c_void {
    unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) }
}

unsafe fn sym(lib: *mut c_void, name: &CStr) -> *mut c_void {
    if lib.is_null() { std::ptr::null_mut() } else { unsafe { libc::dlsym(lib, name.as_ptr()) } }
}

struct Glx {
    get_proc: GetProcFn,
    query_drawable: Option<QueryDrawableFn>,
    current_context: CurrentContextFn,
}

static GLX: Mutex<Option<Glx>> = Mutex::new(None);

impl Hook {
    pub(crate) fn install(core: Core) -> Result<Self, String> {
        let shared = Arc::clone(&core.shared);
        unsafe {
            let libgl = dl_noload(c"libGL.so.1");
            if libgl.is_null() {
                return Err("the game has not loaded OpenGL (libGL.so.1); the native overlay needs the OpenGL renderer".into());
            }
            let libglx = dl_noload(c"libGLX.so.0");
            let get_proc = sym(libgl, c"glXGetProcAddressARB");
            let current = sym(libgl, c"glXGetCurrentContext");
            if get_proc.is_null() || current.is_null() {
                return Err("libGL lacks glXGetProcAddressARB/glXGetCurrentContext".into());
            }
            let get_proc: GetProcFn = std::mem::transmute(get_proc);
            let query = sym(libgl, c"glXQueryDrawable");
            *GLX.lock().unwrap_or_else(PoisonError::into_inner) = Some(Glx {
                get_proc,
                query_drawable: (!query.is_null()).then(|| std::mem::transmute::<*mut c_void, QueryDrawableFn>(query)),
                current_context: std::mem::transmute::<*mut c_void, CurrentContextFn>(current),
            });

            // Every address the game may call glXSwapBuffers through.
            let mut targets: Vec<usize> = Vec::new();
            for p in [
                sym(libgl, c"glXSwapBuffers"),
                get_proc(c"glXSwapBuffers".as_ptr().cast()) as *mut c_void,
                sym(libglx, c"glXSwapBuffers"),
            ] {
                let a = p as usize;
                if a != 0 && !targets.contains(&a) {
                    targets.push(a);
                }
            }
            if targets.is_empty() {
                return Err("glXSwapBuffers not found".into());
            }

            *STATE.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Renderer { core, gl: None, input: None, failed: false, warned_pointer: false });
            let hooks: [usize; MAX_SLOTS] = [hook0 as *const () as usize, hook1 as *const () as usize, hook2 as *const () as usize];
            let mut patched = 0;
            for (i, &addr) in targets.iter().take(MAX_SLOTS).enumerate() {
                match patch(&SLOTS[i], addr, hooks[i]) {
                    Ok(()) => patched += 1,
                    Err(e) => shared.log(format!("cannot hook glXSwapBuffers at {addr:#x}: {e}")),
                }
            }
            if patched == 0 {
                *STATE.lock().unwrap_or_else(PoisonError::into_inner) = None;
                return Err("could not hook glXSwapBuffers".into());
            }
        }
        ACTIVE.store(true, Ordering::Release);
        Ok(Self { shared })
    }

    pub(crate) fn uninstall(&self) {
        ACTIVE.store(false, Ordering::Release);
        for slot in &SLOTS {
            let addr = slot.addr.swap(0, Ordering::AcqRel);
            if addr != 0 {
                let bytes = slot.bytes.lock().unwrap_or_else(PoisonError::into_inner);
                unsafe {
                    let _ = write_code(addr, &bytes.0);
                }
            }
        }
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while IN_FLIGHT.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if IN_FLIGHT.load(Ordering::Acquire) != 0 {
            self.shared.log("overlay: a frame was still inside the hook at shutdown");
        }
        // GL objects belong to the game's context and cannot be deleted from
        // this thread; they are freed with the context when the game exits.
        if let Some(mut r) = STATE.lock().unwrap_or_else(PoisonError::into_inner).take() {
            if let Some(i) = r.input.take() {
                i.close();
            }
            std::mem::forget(r.gl.take());
        }
    }
}

// ---------------------------------------------------------------------------
// hot patching
// ---------------------------------------------------------------------------

/// Writes `bytes` over code at `addr` (temporarily making the pages writable).
unsafe fn write_code(addr: usize, bytes: &[u8]) -> Result<(), String> {
    unsafe {
        let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        let start = addr & !(page - 1);
        let len = (addr + bytes.len()).div_ceil(page) * page - start;
        let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
        if libc::mprotect(start as *mut c_void, len, rwx) != 0 {
            return Err(format!("mprotect: {}", std::io::Error::last_os_error()));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len());
        libc::mprotect(start as *mut c_void, len, libc::PROT_READ | libc::PROT_EXEC);
        Ok(())
    }
}

unsafe fn patch(slot: &Slot, addr: usize, hook: usize) -> Result<(), String> {
    // jmp qword ptr [rip+0]; .quad hook
    let mut jump = [0u8; PATCH_LEN];
    jump[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    jump[6..].copy_from_slice(&(hook as u64).to_le_bytes());
    let mut orig = [0u8; PATCH_LEN];
    unsafe { std::ptr::copy_nonoverlapping(addr as *const u8, orig.as_mut_ptr(), PATCH_LEN) };
    let mut bytes = slot.bytes.lock().unwrap_or_else(PoisonError::into_inner);
    *bytes = (orig, jump);
    unsafe { write_code(addr, &jump)? };
    slot.addr.store(addr, Ordering::Release);
    Ok(())
}

unsafe extern "C" fn hook0(dpy: *mut c_void, drawable: c_ulong) {
    unsafe { swap_hook(0, dpy, drawable) }
}
unsafe extern "C" fn hook1(dpy: *mut c_void, drawable: c_ulong) {
    unsafe { swap_hook(1, dpy, drawable) }
}
unsafe extern "C" fn hook2(dpy: *mut c_void, drawable: c_ulong) {
    unsafe { swap_hook(2, dpy, drawable) }
}

unsafe fn swap_hook(i: usize, dpy: *mut c_void, drawable: c_ulong) {
    IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    if ACTIVE.load(Ordering::Acquire) && !IN_HOOK.get() {
        IN_HOOK.set(true);
        if catch_unwind(AssertUnwindSafe(|| unsafe { draw(dpy, drawable) })).is_err() {
            ACTIVE.store(false, Ordering::Release);
        }
        IN_HOOK.set(false);
    }
    let slot = &SLOTS[i];
    let addr = slot.addr.load(Ordering::Acquire);
    if addr != 0 {
        let bytes = slot.bytes.lock().unwrap_or_else(PoisonError::into_inner);
        let was = IN_HOOK.replace(true); // libGL stub → libGLX: draw only once
        unsafe {
            let _ = write_code(addr, &bytes.0);
            let f: SwapFn = std::mem::transmute(addr);
            f(dpy, drawable);
            if slot.addr.load(Ordering::Acquire) != 0 {
                let _ = write_code(addr, &bytes.1);
            }
        }
        IN_HOOK.set(was);
    }
    IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
}

unsafe fn draw(dpy: *mut c_void, drawable: c_ulong) {
    let Ok(mut guard) = STATE.try_lock() else { return };
    let Some(r) = guard.as_mut() else { return };
    if r.failed {
        return;
    }
    if let Err(e) = unsafe { r.draw(dpy, drawable) } {
        r.failed = true;
        r.core.shared.log(format!("overlay disabled for this session: {e}"));
    }
}

// ---------------------------------------------------------------------------
// X11 input via XCB
// ---------------------------------------------------------------------------

#[repr(C)]
struct Cookie {
    sequence: c_uint,
}

#[repr(C)]
struct PointerReply {
    response_type: u8,
    same_screen: u8,
    sequence: u16,
    length: u32,
    root: u32,
    child: u32,
    root_x: i16,
    root_y: i16,
    win_x: i16,
    win_y: i16,
    mask: u16,
    pad: [u8; 2],
}

#[repr(C)]
struct KeymapReply {
    response_type: u8,
    pad: u8,
    sequence: u16,
    length: u32,
    keys: [u8; 32],
}

struct XInput {
    conn: *mut c_void,
    disconnect: unsafe extern "C" fn(*mut c_void),
    query_pointer: unsafe extern "C" fn(*mut c_void, u32) -> Cookie,
    query_pointer_reply: unsafe extern "C" fn(*mut c_void, Cookie, *mut *mut c_void) -> *mut PointerReply,
    query_keymap: unsafe extern "C" fn(*mut c_void) -> Cookie,
    query_keymap_reply: unsafe extern "C" fn(*mut c_void, Cookie, *mut *mut c_void) -> *mut KeymapReply,
    last_keys: Instant,
    hotkey: bool,
}

// SAFETY: the XCB connection is created, used and closed only while holding
// the `STATE` mutex (render thread during frames, game thread at shutdown);
// XCB connections may be used from any thread.
unsafe impl Send for XInput {}

// X keycodes (evdev + 8), standard on Xorg/XWayland with evdev or libinput.
const KC_BACKSPACE: usize = 22;
const KC_CONTROL: [usize; 2] = [37, 105];
const KC_SHIFT: [usize; 2] = [50, 62];
const BUTTON1_MASK: u16 = 1 << 8;

impl XInput {
    unsafe fn open() -> Result<Self, String> {
        unsafe {
            let mut lib = libc::dlopen(c"libxcb.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if lib.is_null() {
                lib = libc::dlopen(c"libxcb.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            }
            if lib.is_null() {
                return Err("libxcb not found".into());
            }
            let get = |n: &CStr| -> Result<*mut c_void, String> {
                let p = sym(lib, n);
                if p.is_null() { Err(format!("{n:?} missing")) } else { Ok(p) }
            };
            type Connect = unsafe extern "C" fn(*const c_char, *mut c_int) -> *mut c_void;
            type HasError = unsafe extern "C" fn(*mut c_void) -> c_int;
            let connect: Connect = std::mem::transmute(get(c"xcb_connect")?);
            let has_error: HasError = std::mem::transmute(get(c"xcb_connection_has_error")?);
            let disconnect = std::mem::transmute(get(c"xcb_disconnect")?);
            let conn = connect(std::ptr::null(), std::ptr::null_mut());
            if conn.is_null() || has_error(conn) != 0 {
                return Err("cannot connect to the X server (DISPLAY)".into());
            }
            Ok(Self {
                conn,
                disconnect,
                query_pointer: std::mem::transmute(get(c"xcb_query_pointer")?),
                query_pointer_reply: std::mem::transmute(get(c"xcb_query_pointer_reply")?),
                query_keymap: std::mem::transmute(get(c"xcb_query_keymap")?),
                query_keymap_reply: std::mem::transmute(get(c"xcb_query_keymap_reply")?),
                last_keys: Instant::now() - Duration::from_secs(1),
                hotkey: false,
            })
        }
    }

    fn close(self) {
        unsafe { (self.disconnect)(self.conn) };
    }

    /// Ctrl+Shift+Backspace held (polled at most every 25 ms).
    unsafe fn hotkey(&mut self) -> bool {
        if self.last_keys.elapsed() < Duration::from_millis(25) {
            return self.hotkey;
        }
        self.last_keys = Instant::now();
        unsafe {
            let mut err: *mut c_void = std::ptr::null_mut();
            let r = (self.query_keymap_reply)(self.conn, (self.query_keymap)(self.conn), &mut err);
            if !err.is_null() {
                libc::free(err);
            }
            if r.is_null() {
                return false;
            }
            let keys = (*r).keys;
            libc::free(r.cast());
            let down = |kc: usize| keys[kc / 8] & (1 << (kc % 8)) != 0;
            self.hotkey = down(KC_BACKSPACE) && KC_CONTROL.iter().any(|&k| down(k)) && KC_SHIFT.iter().any(|&k| down(k));
            self.hotkey
        }
    }

    /// Pointer position relative to `window` and left-button state.
    unsafe fn pointer(&mut self, window: u32) -> Option<((f32, f32), bool)> {
        unsafe {
            let mut err: *mut c_void = std::ptr::null_mut();
            let r = (self.query_pointer_reply)(self.conn, (self.query_pointer)(self.conn, window), &mut err);
            if !err.is_null() {
                libc::free(err);
            }
            if r.is_null() {
                return None;
            }
            let p = &*r;
            let out = (p.same_screen != 0).then(|| ((f32::from(p.win_x), f32::from(p.win_y)), p.mask & BUTTON1_MASK != 0));
            libc::free(r.cast());
            out
        }
    }
}

// ---------------------------------------------------------------------------
// OpenGL drawing
// ---------------------------------------------------------------------------

struct Renderer {
    core: Core,
    gl: Option<GlObjects>,
    input: Option<XInput>,
    failed: bool,
    warned_pointer: bool,
}

struct GlObjects {
    context: usize,
    program: u32,
    vao: u32,
    loc_tex: i32,
    caps: Caps,
    /// One texture per sprite id: (GL name, sprite version, size).
    textures: HashMap<u32, (u32, u64, (u32, u32))>,
}

/// Optional GL features, from the context version and extension list (Mesa
/// returns a stub for any glXGetProcAddress name, so pointers prove nothing).
#[derive(Clone, Copy)]
struct Caps {
    samplers: bool,
    clip_control: bool,
}

unsafe fn caps() -> Caps {
    unsafe {
        let (major, minor) = (geti(gl::MAJOR_VERSION), geti(gl::MINOR_VERSION));
        let at_least = |a: i32, b: i32| (major, minor) >= (a, b);
        let mut clip_ext = false;
        let mut sampler_ext = false;
        for i in 0..geti(gl::NUM_EXTENSIONS).max(0) as u32 {
            let p = gl::GetStringi(gl::EXTENSIONS, i);
            if p.is_null() {
                continue;
            }
            match CStr::from_ptr(p.cast()).to_bytes() {
                b"GL_ARB_clip_control" => clip_ext = true,
                b"GL_ARB_sampler_objects" => sampler_ext = true,
                _ => {}
            }
        }
        Caps { samplers: at_least(3, 3) || sampler_ext, clip_control: at_least(4, 5) || clip_ext }
    }
}

const VS: &str = "#version 150
out vec2 v_uv;
void main() {
    vec2 uv = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    v_uv = uv;
    gl_Position = vec4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}
";

const FS: &str = "#version 150
uniform sampler2D u_tex;
in vec2 v_uv;
out vec4 o_color;
void main() { o_color = texture(u_tex, v_uv); }
";

unsafe fn shader(kind: u32, src: &str) -> Result<u32, String> {
    unsafe {
        let s = gl::CreateShader(kind);
        let ptr = src.as_ptr().cast::<gl::types::GLchar>();
        let len = src.len() as i32;
        gl::ShaderSource(s, 1, &ptr, &len);
        gl::CompileShader(s);
        let mut ok = 0;
        gl::GetShaderiv(s, gl::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = vec![0u8; 1024];
            let mut n = 0;
            gl::GetShaderInfoLog(s, 1024, &mut n, log.as_mut_ptr().cast());
            gl::DeleteShader(s);
            return Err(format!("GLSL: {}", String::from_utf8_lossy(&log[..n.max(0) as usize])));
        }
        Ok(s)
    }
}

impl GlObjects {
    unsafe fn new(context: usize) -> Result<Self, String> {
        unsafe {
            for (name, loaded) in [
                ("glCreateShader", gl::CreateShader::is_loaded()),
                ("glGenVertexArrays", gl::GenVertexArrays::is_loaded()),
                ("glDrawArrays", gl::DrawArrays::is_loaded()),
                ("glTexSubImage2D", gl::TexSubImage2D::is_loaded()),
            ] {
                if !loaded {
                    return Err(format!("{name} unavailable (OpenGL 3.2+ needed)"));
                }
            }
            let (mut prev_vao, mut prev_tex) = (0, 0);
            gl::GetIntegerv(gl::VERTEX_ARRAY_BINDING, &mut prev_vao);
            gl::GetIntegerv(gl::TEXTURE_BINDING_2D, &mut prev_tex);

            let vs = shader(gl::VERTEX_SHADER, VS)?;
            let fs = shader(gl::FRAGMENT_SHADER, FS)?;
            let program = gl::CreateProgram();
            gl::AttachShader(program, vs);
            gl::AttachShader(program, fs);
            gl::LinkProgram(program);
            gl::DeleteShader(vs);
            gl::DeleteShader(fs);
            let mut ok = 0;
            gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
            if ok == 0 {
                gl::DeleteProgram(program);
                return Err("GLSL program link failed".into());
            }
            let loc_tex = gl::GetUniformLocation(program, c"u_tex".as_ptr());
            let mut vao = 0;
            gl::GenVertexArrays(1, &mut vao);
            gl::BindTexture(gl::TEXTURE_2D, prev_tex as u32);
            gl::BindVertexArray(prev_vao as u32);
            Ok(Self { context, program, vao, loc_tex, caps: caps(), textures: HashMap::new() })
        }
    }
}

/// GL state touched by the overlay, saved and restored around the draw.
struct SavedGl {
    program: i32,
    vao: i32,
    active_texture: i32,
    texture: i32,
    sampler: i32,
    draw_fb: i32,
    unpack_buffer: i32,
    unpack: [i32; 4],
    viewport: [i32; 4],
    enables: [(u32, bool); 7],
    blend: [i32; 6],
    color_mask: [u8; 4],
    clip: Option<(i32, i32)>,
}

const ENABLES: [u32; 7] = [
    gl::BLEND,
    gl::CULL_FACE,
    gl::DEPTH_TEST,
    gl::STENCIL_TEST,
    gl::SCISSOR_TEST,
    gl::FRAMEBUFFER_SRGB,
    gl::RASTERIZER_DISCARD,
];
const UNPACK: [u32; 4] = [gl::UNPACK_ALIGNMENT, gl::UNPACK_ROW_LENGTH, gl::UNPACK_SKIP_PIXELS, gl::UNPACK_SKIP_ROWS];

unsafe fn geti(name: u32) -> i32 {
    let mut v = 0;
    unsafe { gl::GetIntegerv(name, &mut v) };
    v
}

impl SavedGl {
    /// Leaves texture unit 0 active (its bindings are what gets saved).
    unsafe fn capture(caps: Caps) -> Self {
        unsafe {
            let mut viewport = [0; 4];
            gl::GetIntegerv(gl::VIEWPORT, viewport.as_mut_ptr());
            let mut color_mask = [0u8; 4];
            gl::GetBooleanv(gl::COLOR_WRITEMASK, color_mask.as_mut_ptr());
            let active_texture = geti(gl::ACTIVE_TEXTURE);
            gl::ActiveTexture(gl::TEXTURE0);
            Self {
                program: geti(gl::CURRENT_PROGRAM),
                vao: geti(gl::VERTEX_ARRAY_BINDING),
                active_texture,
                texture: geti(gl::TEXTURE_BINDING_2D),
                sampler: if caps.samplers { geti(gl::SAMPLER_BINDING) } else { 0 },
                draw_fb: geti(gl::DRAW_FRAMEBUFFER_BINDING),
                unpack_buffer: geti(gl::PIXEL_UNPACK_BUFFER_BINDING),
                unpack: UNPACK.map(|n| geti(n)),
                viewport,
                enables: ENABLES.map(|e| (e, gl::IsEnabled(e) == gl::TRUE)),
                blend: [
                    geti(gl::BLEND_SRC_RGB),
                    geti(gl::BLEND_DST_RGB),
                    geti(gl::BLEND_SRC_ALPHA),
                    geti(gl::BLEND_DST_ALPHA),
                    geti(gl::BLEND_EQUATION_RGB),
                    geti(gl::BLEND_EQUATION_ALPHA),
                ],
                color_mask,
                clip: caps.clip_control.then(|| (geti(gl::CLIP_ORIGIN), geti(gl::CLIP_DEPTH_MODE))),
            }
        }
    }

    unsafe fn restore(&self, caps: Caps) {
        unsafe {
            gl::UseProgram(self.program as u32);
            gl::BindVertexArray(self.vao as u32);
            // texture unit 0 is still active here
            gl::BindTexture(gl::TEXTURE_2D, self.texture as u32);
            if caps.samplers {
                gl::BindSampler(0, self.sampler as u32);
            }
            gl::ActiveTexture(self.active_texture as u32);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, self.draw_fb as u32);
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, self.unpack_buffer as u32);
            for (n, v) in UNPACK.iter().zip(self.unpack) {
                gl::PixelStorei(*n, v);
            }
            gl::Viewport(self.viewport[0], self.viewport[1], self.viewport[2], self.viewport[3]);
            for (e, on) in self.enables {
                if on { gl::Enable(e) } else { gl::Disable(e) }
            }
            let b = self.blend;
            gl::BlendFuncSeparate(b[0] as u32, b[1] as u32, b[2] as u32, b[3] as u32);
            gl::BlendEquationSeparate(b[4] as u32, b[5] as u32);
            let m = self.color_mask;
            gl::ColorMask(m[0], m[1], m[2], m[3]);
            if let Some((origin, depth)) = self.clip {
                gl::ClipControl(origin as u32, depth as u32);
            }
        }
    }
}

impl Renderer {
    unsafe fn draw(&mut self, dpy: *mut c_void, drawable: c_ulong) -> Result<(), String> {
        unsafe {
            let (get_proc, query, current) = {
                let g = GLX.lock().unwrap_or_else(PoisonError::into_inner);
                let g = g.as_ref().ok_or("GLX not initialised")?;
                (g.get_proc, g.query_drawable, g.current_context)
            };
            let context = current() as usize;
            if context == 0 {
                return Ok(()); // no current context: nothing we can draw with
            }
            if self.gl.as_ref().is_none_or(|g| g.context != context) {
                gl::load_with(|name| {
                    let c = std::ffi::CString::new(name).unwrap_or_default();
                    get_proc(c.as_ptr().cast())
                });
                if let Some(old) = self.gl.take() {
                    std::mem::forget(old); // belonged to another context
                }
                self.gl = Some(GlObjects::new(context)?);
            }

            // back buffer size
            let (mut w, mut h) = (0u32, 0u32);
            if let Some(q) = query {
                q(dpy, drawable, GLX_WIDTH, &mut w);
                q(dpy, drawable, GLX_HEIGHT, &mut h);
            }
            if w == 0 || h == 0 {
                let mut vp = [0; 4];
                gl::GetIntegerv(gl::VIEWPORT, vp.as_mut_ptr());
                (w, h) = (vp[2].max(0) as u32, vp[3].max(0) as u32);
            }

            // input
            if self.input.is_none() {
                match XInput::open() {
                    Ok(i) => self.input = Some(i),
                    Err(e) => {
                        self.core.shared.log(format!("overlay input unavailable: {e}"));
                        self.input = None;
                    }
                }
            }
            let mut input = Input::default();
            if let Some(x) = self.input.as_mut() {
                input.hotkey = x.hotkey();
                if self.core.edit {
                    match x.pointer(drawable as u32) {
                        Some((p, left)) => {
                            input.pointer = Some(p);
                            input.left_button = left;
                        }
                        None if !self.warned_pointer => {
                            self.warned_pointer = true;
                            self.core.shared.log("overlay: mouse position unavailable for the game window");
                        }
                        None => {}
                    }
                }
            }

            let frame = self.core.frame((w, h), input);
            let objs = self.gl.as_mut().expect("created above");
            let caps = objs.caps;
            // textures of sprites that are gone
            let stale: Vec<u32> = objs.textures.keys().copied().filter(|id| !frame.sprites.iter().any(|s| s.id == *id)).collect();
            for id in stale {
                if let Some((name, _, _)) = objs.textures.remove(&id) {
                    gl::DeleteTextures(1, &name);
                }
            }
            if frame.sprites.is_empty() {
                return Ok(());
            }

            let saved = SavedGl::capture(caps);
            gl::BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            gl::PixelStorei(gl::UNPACK_SKIP_PIXELS, 0);
            gl::PixelStorei(gl::UNPACK_SKIP_ROWS, 0);
            if caps.samplers {
                gl::BindSampler(0, 0);
            }
            // upload only sprites whose image changed (texture unit 0 is active)
            for sp in &frame.sprites {
                let (iw, ih) = (sp.image.width(), sp.image.height());
                let entry = objs.textures.entry(sp.id).or_insert_with(|| {
                    let mut name = 0;
                    gl::GenTextures(1, &mut name);
                    gl::BindTexture(gl::TEXTURE_2D, name);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAX_LEVEL, 0);
                    (name, 0, (0, 0))
                });
                if entry.1 != sp.version {
                    gl::BindTexture(gl::TEXTURE_2D, entry.0);
                    let data = sp.image.data().as_ptr().cast();
                    if entry.2 != (iw, ih) {
                        gl::TexImage2D(gl::TEXTURE_2D, 0, gl::RGBA8 as i32, iw as i32, ih as i32, 0, gl::RGBA, gl::UNSIGNED_BYTE, data);
                    } else {
                        gl::TexSubImage2D(gl::TEXTURE_2D, 0, 0, 0, iw as i32, ih as i32, gl::RGBA, gl::UNSIGNED_BYTE, data);
                    }
                    (entry.1, entry.2) = (sp.version, (iw, ih));
                }
            }

            for e in ENABLES {
                gl::Disable(e);
            }
            gl::Enable(gl::BLEND);
            gl::BlendFuncSeparate(gl::ONE, gl::ONE_MINUS_SRC_ALPHA, gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            gl::BlendEquationSeparate(gl::FUNC_ADD, gl::FUNC_ADD);
            gl::ColorMask(gl::TRUE, gl::TRUE, gl::TRUE, gl::TRUE);
            if let Some((_, depth)) = saved.clip {
                gl::ClipControl(gl::LOWER_LEFT, depth as u32);
            }
            gl::UseProgram(objs.program);
            gl::Uniform1i(objs.loc_tex, 0);
            gl::BindVertexArray(objs.vao);
            // in order: boxes, then the control panel, then the cursor
            for sp in &frame.sprites {
                let Some(&(name, _, (iw, ih))) = objs.textures.get(&sp.id) else { continue };
                gl::BindTexture(gl::TEXTURE_2D, name);
                // GL window coordinates start at the bottom-left corner.
                gl::Viewport(sp.x, h as i32 - sp.y - ih as i32, iw as i32, ih as i32);
                gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
            }
            saved.restore(caps);
            Ok(())
        }
    }
}
