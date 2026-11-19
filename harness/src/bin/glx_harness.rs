//! Native-Linux test of the OpenGL overlay backend.
//!
//!   glx_harness <path/to/libaigakira_telemetry_ets2_udp.so> <out-dir>
//!
//! Like the native game it loads libGL with dlopen, resolves glXSwapBuffers
//! *before* the plugin is loaded, uses a GL 3.3 core context with
//! glClipControl(UPPER_LEFT) and sRGB writes enabled, then loads the plugin
//! through scs_telemetry_init. It renders into a single-buffered pbuffer so
//! the overlay drawn inside glXSwapBuffers can be read back afterwards.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("glx_harness only runs on Linux");
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::run());
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, c_int, c_ulong, c_void};
    use std::path::PathBuf;
    use std::time::Duration;

    use overlay_harness as h;

    type GetProc = unsafe extern "C" fn(*const u8) -> *const c_void;

    unsafe fn sym(lib: *mut c_void, name: &CStr) -> *mut c_void {
        let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
        assert!(!p.is_null(), "missing symbol {name:?}");
        p
    }

    fn check(ok: bool, what: &str, failures: &mut u32) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            *failures += 1;
        }
    }

    unsafe fn load_plugin(path: &str) -> (*mut c_void, h::InitFn, h::ShutdownFn) {
        unsafe {
            let c = std::ffi::CString::new(path).unwrap();
            let lib = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            assert!(!lib.is_null(), "dlopen plugin: {:?}", CStr::from_ptr(libc::dlerror()));
            let init: h::InitFn = std::mem::transmute(sym(lib, c"scs_telemetry_init"));
            let shutdown: h::ShutdownFn = std::mem::transmute(sym(lib, c"scs_telemetry_shutdown"));
            (lib, init, shutdown)
        }
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let plugin = args.get(1).expect("usage: glx_harness <plugin.so> <out-dir>").clone();
        let out = PathBuf::from(args.get(2).map_or(".", String::as_str));
        std::fs::create_dir_all(&out).unwrap();
        let mut failures = 0;
        unsafe { run_inner(&plugin, &out, &mut failures) };
        println!("{}", if failures == 0 { "ALL PASSED" } else { "FAILURES" });
        i32::from(failures != 0)
    }

    unsafe fn run_inner(plugin: &str, out: &std::path::Path, failures: &mut u32) {
        unsafe {
            let settings = h::settings_path();
            let _ = std::fs::remove_file(&settings);

            // --- phase 1: first start creates the settings file, overlay off
            println!("== first start (no settings file)");
            let (lib, init, shutdown) = load_plugin(plugin);
            check(h::init(init) == 0, "scs_telemetry_init returns OK", failures);
            check(settings.exists(), "settings file created next to the plugin", failures);
            let text = std::fs::read_to_string(&settings).unwrap_or_default();
            let overlay_on = text.split("\n[udp]\n").next().is_some_and(|o| o.contains("\nenabled = true"));
            check(overlay_on, "overlay on by default", failures);
            check(h::log_contains("created settings file"), "game.log says the file was created", failures);
            check(h::log_contains("the in-game overlay is on"), "game.log says the overlay is on", failures);
            shutdown();
            libc::dlclose(lib);

            // enable it (and keep UDP quiet for the test)
            let text = text
                .replace("[udp]\n# Stream telemetry as JSON over UDP (for the Touch Portal plugin or\n# tools/udp_listen.py).\nenabled = true", "[udp]\nenabled = false");
            std::fs::write(&settings, text).unwrap();

            // --- GL setup, like the game: dlopen libGL, resolve functions
            let libgl = libc::dlopen(c"libGL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            let libx11 = libc::dlopen(c"libX11.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            assert!(!libgl.is_null() && !libx11.is_null(), "libGL/libX11");
            let gpa: GetProc = std::mem::transmute(sym(libgl, c"glXGetProcAddressARB"));
            type OpenDisplay = unsafe extern "C" fn(*const i8) -> *mut c_void;
            type DefaultScreen = unsafe extern "C" fn(*mut c_void) -> c_int;
            type ChooseFb = unsafe extern "C" fn(*mut c_void, c_int, *const c_int, *mut c_int) -> *mut *mut c_void;
            type CreatePbuffer = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_int) -> c_ulong;
            type CreateCtx = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, c_int, *const c_int) -> *mut c_void;
            type MakeCurrent = unsafe extern "C" fn(*mut c_void, c_ulong, c_ulong, *mut c_void) -> c_int;
            type Swap = unsafe extern "C" fn(*mut c_void, c_ulong);
            let open: OpenDisplay = std::mem::transmute(sym(libx11, c"XOpenDisplay"));
            let default_screen: DefaultScreen = std::mem::transmute(sym(libx11, c"XDefaultScreen"));
            let choose: ChooseFb = std::mem::transmute(sym(libgl, c"glXChooseFBConfig"));
            let create_pbuffer: CreatePbuffer = std::mem::transmute(sym(libgl, c"glXCreatePbuffer"));
            let make_current: MakeCurrent = std::mem::transmute(sym(libgl, c"glXMakeContextCurrent"));
            let create_ctx: CreateCtx = std::mem::transmute(gpa(c"glXCreateContextAttribsARB".as_ptr().cast()));
            // resolved before the plugin is loaded, exactly like the game
            let swap: Swap = std::mem::transmute(sym(libgl, c"glXSwapBuffers"));
            let swap_bytes: [u8; 16] = std::ptr::read(swap as *const [u8; 16]);

            let dpy = open(std::ptr::null());
            assert!(!dpy.is_null(), "XOpenDisplay (DISPLAY not set?)");
            let attrs = [0x8010, 0x4, 0x8011, 0x1, 8, 8, 9, 8, 10, 8, 11, 8, 5, 0, 0];
            let mut n = 0;
            let cfgs = choose(dpy, default_screen(dpy), attrs.as_ptr(), &mut n);
            assert!(!cfgs.is_null() && n > 0, "no pbuffer FB config");
            let cfg = *cfgs;
            let (w, hgt) = (1280i32, 720i32);
            let pbuf = create_pbuffer(dpy, cfg, [0x8041, w, 0x8040, hgt, 0].as_ptr());
            let ctx = create_ctx(dpy, cfg, std::ptr::null_mut(), 1, [0x2091, 3, 0x2092, 3, 0x9126, 1, 0].as_ptr());
            assert!(pbuf != 0 && !ctx.is_null(), "pbuffer/context");
            assert!(make_current(dpy, pbuf, pbuf, ctx) != 0, "make current");
            gl::load_with(|s| {
                let c = std::ffi::CString::new(s).unwrap();
                gpa(c.as_ptr().cast())
            });

            // game-like state that must survive the overlay
            let mut game_vao = 0;
            gl::GenVertexArrays(1, &mut game_vao);
            gl::BindVertexArray(game_vao);
            gl::ClipControl(gl::UPPER_LEFT, gl::ZERO_TO_ONE);
            gl::Enable(gl::DEPTH_TEST);
            gl::Viewport(10, 20, 300, 200);

            // --- phase 2: overlay enabled
            println!("== start with [overlay] enabled = true");
            let (lib, init, shutdown) = load_plugin(plugin);
            check(h::init(init) == 0, "scs_telemetry_init returns OK", failures);
            check(h::log_contains("in-game overlay enabled (OpenGL (GLX))"), "overlay hook installed", failures);
            let patched: [u8; 16] = std::ptr::read(swap as *const [u8; 16]);
            check(patched != swap_bytes, "glXSwapBuffers entry patched", failures);
            h::load_truck();

            let bg = [40u8, 90, 160];
            for i in 0..12 {
                h::game_frame(25.0, i % 2 == 0);
                gl::ClearColor(f32::from(bg[0]) / 255.0, f32::from(bg[1]) / 255.0, f32::from(bg[2]) / 255.0, 1.0);
                gl::Clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);
                swap(dpy, pbuf);
                std::thread::sleep(Duration::from_millis(60));
            }
            gl::Finish();
            let mut px = vec![0u8; (w * hgt * 4) as usize];
            gl::PixelStorei(gl::PACK_ALIGNMENT, 4);
            gl::ReadPixels(0, 0, w, hgt, gl::RGBA, gl::UNSIGNED_BYTE, px.as_mut_ptr().cast());
            // GL rows are bottom-up; flip to top-down
            let stride = (w * 4) as usize;
            let mut img = vec![0u8; px.len()];
            for y in 0..hgt as usize {
                img[y * stride..(y + 1) * stride].copy_from_slice(&px[(hgt as usize - 1 - y) * stride..(hgt as usize - y) * stride]);
            }
            h::write_ppm(&out.join("glx-overlay.ppm"), w as u32, hgt as u32, &img);

            let far = h::avg(&img, stride, 900, 500, 1000, 600);
            let corner = h::avg(&img, stride, 42, 42, 48, 48); // panel padding, top-left
            println!("   background {far:?}, panel corner {corner:?}");
            check((far[0] - 40.0).abs() < 3.0 && (far[2] - 160.0).abs() < 3.0, "background untouched away from the panel", failures);
            check(corner[2] < 80.0, "dark panel drawn at the top-left (correct orientation)", failures);
            let bright = (45..300).any(|x| (45..250).any(|y| img[y * stride + x * 4] > 200 && img[y * stride + x * 4 + 2] > 200));
            check(bright, "panel contains light text", failures);

            let mut vp = [0; 4];
            gl::GetIntegerv(gl::VIEWPORT, vp.as_mut_ptr());
            let mut origin = 0;
            gl::GetIntegerv(gl::CLIP_ORIGIN, &mut origin);
            let mut vao = 0;
            gl::GetIntegerv(gl::VERTEX_ARRAY_BINDING, &mut vao);
            check(vp == [10, 20, 300, 200], "game viewport restored", failures);
            check(origin == gl::UPPER_LEFT as i32, "game clip control restored", failures);
            check(vao == game_vao as i32, "game VAO binding restored", failures);
            check(gl::IsEnabled(gl::DEPTH_TEST) == gl::TRUE, "game depth test state restored", failures);
            check(gl::GetError() == gl::NO_ERROR, "no GL error left behind", failures);

            shutdown();
            let after: [u8; 16] = std::ptr::read(swap as *const [u8; 16]);
            check(after == swap_bytes, "glXSwapBuffers restored on shutdown", failures);
            check(h::log_contains("in-game overlay removed"), "overlay removed on shutdown", failures);
            libc::dlclose(lib);
            swap(dpy, pbuf); // must still work after the plugin is gone
            println!("PASS swap after unload did not crash");
        }
    }
}
