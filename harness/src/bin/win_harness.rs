//! Windows / Proton test of the Direct3D 11 overlay backend (run under Wine,
//! with DXVK's d3d11/dxgi like Proton, or Wine's builtin ones).
//!
//!   wine win_harness.exe <aigakira_telemetry_ets2_udp.dll> <out-dir>
//!
//! Creates a swap chain like ETS2's (B8G8R8A8_UNORM_SRGB, discard model),
//! puts a capture hook into the Present vtable slot *before* loading the
//! plugin (so the plugin's hook draws, then calls the capture, then the real
//! Present), loads the plugin through scs_telemetry_init and checks the
//! captured frames. Also presses Ctrl+Shift+Backspace with SendInput to test
//! edit mode.

#[cfg(not(windows))]
fn main() {
    eprintln!("win_harness is a Windows program; build it for x86_64-pc-windows-gnu and run it with wine");
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use overlay_harness as h;
    use windows::Win32::Devices::HumanInterfaceDevice::*;
    use windows::Win32::Foundation::{FreeLibrary, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{HRESULT, HSTRING, Interface, s, w};

    type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
    static ORIG: AtomicUsize = AtomicUsize::new(0);
    static CAPTURE: Mutex<Option<(u32, u32, Vec<u8>)>> = Mutex::new(None);

    thread_local! {
        /// The "game's" DirectInput mouse, read every frame like ETS2 does.
        static GAME_MOUSE: std::cell::RefCell<Option<IDirectInputDevice8W>> = const { std::cell::RefCell::new(None) };
        /// Sum of |movement| and button presses the game received.
        static GAME_SAW: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }

    /// c_dfDIMouse (DIMOUSESTATE: lX, lY, lZ, 4 buttons), built by hand.
    unsafe fn create_game_mouse(hwnd: HWND) -> windows::core::Result<IDirectInputDevice8W> {
        static X: windows::core::GUID = GUID_XAxis;
        static Y: windows::core::GUID = GUID_YAxis;
        static Z: windows::core::GUID = GUID_ZAxis;
        const AXIS: u32 = 3;
        const BUTTON: u32 = 0x0C;
        const ANY: u32 = 0x00FF_FF00;
        const OPTIONAL: u32 = 0x8000_0000;
        unsafe {
            let mut objs = [
                DIOBJECTDATAFORMAT { pguid: &X, dwOfs: 0, dwType: AXIS | ANY, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: &Y, dwOfs: 4, dwType: AXIS | ANY, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: &Z, dwOfs: 8, dwType: AXIS | ANY | OPTIONAL, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: std::ptr::null(), dwOfs: 12, dwType: BUTTON | ANY, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: std::ptr::null(), dwOfs: 13, dwType: BUTTON | ANY, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: std::ptr::null(), dwOfs: 14, dwType: BUTTON | ANY | OPTIONAL, dwFlags: 0 },
                DIOBJECTDATAFORMAT { pguid: std::ptr::null(), dwOfs: 15, dwType: BUTTON | ANY | OPTIONAL, dwFlags: 0 },
            ];
            let mut fmt = DIDATAFORMAT {
                dwSize: size_of::<DIDATAFORMAT>() as u32,
                dwObjSize: size_of::<DIOBJECTDATAFORMAT>() as u32,
                dwFlags: 2, // DIDF_RELAXIS (1 would be DIDF_ABSAXIS)
                dwDataSize: 16,
                dwNumObjs: objs.len() as u32,
                rgodf: objs.as_mut_ptr(),
            };
            let mut raw: *mut c_void = std::ptr::null_mut();
            DirectInput8Create(GetModuleHandleW(None)?.into(), DIRECTINPUT_VERSION, &IDirectInput8W::IID, &mut raw, None)?;
            let di = IDirectInput8W::from_raw(raw);
            let mut dev = None;
            di.CreateDevice(&GUID_SysMouse, &mut dev, None)?;
            let dev = dev.unwrap();
            dev.SetDataFormat(&mut fmt)?;
            dev.SetCooperativeLevel(hwnd, DISCL_NONEXCLUSIVE | DISCL_FOREGROUND)?;
            let _ = dev.Acquire();
            std::mem::forget(di); // keep the DirectInput object alive with the device
            Ok(dev)
        }
    }

    fn poll_game_mouse() {
        GAME_MOUSE.with(|m| {
            if let Some(dev) = m.borrow().as_ref() {
                let mut st = [0i32; 4];
                unsafe {
                    if dev.GetDeviceState(16, st.as_mut_ptr().cast()).is_err() {
                        let _ = dev.Acquire();
                        return;
                    }
                }
                let buttons = i64::from((st[3] as u32 & 0x80) != 0);
                GAME_SAW.set(GAME_SAW.get() + i64::from(st[0].abs()) + i64::from(st[1].abs()) + buttons);
            }
        });
    }

    fn check(ok: bool, what: &str, failures: &mut u32) {
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            *failures += 1;
        }
    }

    /// Copies the back buffer (after the plugin drew) and presents.
    unsafe extern "system" fn capture_present(this: *mut c_void, sync: u32, flags: u32) -> HRESULT {
        unsafe {
            if let Some(sc) = IDXGISwapChain::from_raw_borrowed(&this) {
                let _ = capture(sc);
            }
            let orig: PresentFn = std::mem::transmute(ORIG.load(Ordering::Acquire));
            orig(this, sync, flags)
        }
    }

    unsafe fn capture(sc: &IDXGISwapChain) -> windows::core::Result<()> {
        unsafe {
            let bb: ID3D11Texture2D = sc.GetBuffer(0)?;
            let device: ID3D11Device = sc.GetDevice()?;
            let ctx = device.GetImmediateContext()?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            bb.GetDesc(&mut d);
            d.Usage = D3D11_USAGE_STAGING;
            d.BindFlags = 0;
            d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            d.MiscFlags = 0;
            let mut staging = None;
            device.CreateTexture2D(&d, None, Some(&mut staging))?;
            let staging = staging.unwrap();
            ctx.CopyResource(&staging, &bb);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let mut rgba = vec![0u8; (d.Width * d.Height * 4) as usize];
            for y in 0..d.Height as usize {
                let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), d.Width as usize * 4);
                for x in 0..d.Width as usize {
                    let (b, g, r, a) = (row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]);
                    let o = (y * d.Width as usize + x) * 4;
                    rgba[o..o + 4].copy_from_slice(&[r, g, b, a]);
                }
            }
            ctx.Unmap(&staging, 0);
            *CAPTURE.lock().unwrap() = Some((d.Width, d.Height, rgba));
            Ok(())
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    fn pump() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, time: 0, dwExtraInfo: 0 },
            },
        }
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let plugin = args.get(1).expect("usage: win_harness <plugin.dll> <out-dir>").clone();
        let out = PathBuf::from(args.get(2).map_or(".", String::as_str));
        std::fs::create_dir_all(&out).unwrap();
        let mut failures = 0;
        unsafe { run_inner(&plugin, &out, &mut failures) };
        println!("{}", if failures == 0 { "ALL PASSED" } else { "FAILURES" });
        i32::from(failures != 0)
    }

    unsafe fn load(plugin: &str) -> (HMODULE, h::InitFn, h::ShutdownFn) {
        unsafe {
            let lib = LoadLibraryW(&HSTRING::from(plugin)).expect("LoadLibrary plugin");
            let init: h::InitFn = std::mem::transmute(GetProcAddress(lib, s!("scs_telemetry_init")).expect("init export"));
            let shutdown: h::ShutdownFn = std::mem::transmute(GetProcAddress(lib, s!("scs_telemetry_shutdown")).expect("shutdown export"));
            (lib, init, shutdown)
        }
    }

    unsafe fn frames(sc: &IDXGISwapChain, ctx: &ID3D11DeviceContext, rtv: &ID3D11RenderTargetView, n: usize) {
        unsafe {
            for i in 0..n {
                h::game_frame(25.0, i % 2 == 0);
                // sRGB(40, 90, 160) as linear values for the sRGB render target
                let lin = |c: f32| ((c / 255.0 + 0.055) / 1.055f32).powf(2.4);
                ctx.ClearRenderTargetView(rtv, &[lin(40.0), lin(90.0), lin(160.0), 1.0]);
                pump();
                poll_game_mouse();
                let _ = sc.Present(0, DXGI_PRESENT(0));
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    }

    unsafe fn run_inner(plugin: &str, out: &std::path::Path, failures: &mut u32) {
        unsafe {
            let settings = h::settings_path();
            let _ = std::fs::remove_file(&settings);

            println!("== first start (no settings file)");
            let (lib, init, shutdown) = load(plugin);
            check(h::init(init) == 0, "scs_telemetry_init returns OK", failures);
            check(settings.exists(), "settings file created next to the plugin", failures);
            let text = std::fs::read_to_string(&settings).unwrap_or_default();
            let overlay_on = text.split("\n[udp]\n").next().is_some_and(|o| o.contains("\nenabled = true"));
            check(overlay_on, "overlay on by default", failures);
            check(h::log_contains("the in-game overlay is on"), "game.log says the overlay is on", failures);
            shutdown();
            let _ = FreeLibrary(lib);
            let text = text
                .replace("[udp]\n# Stream telemetry as JSON over UDP (for the Touch Portal plugin or\n# tools/udp_listen.py).\nenabled = true", "[udp]\nenabled = false");
            std::fs::write(&settings, text).unwrap();

            // window + swap chain like ETS2
            let instance = GetModuleHandleW(None).unwrap();
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("overlay_harness"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("overlay_harness"),
                w!("overlay harness"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                1280,
                720,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let mut rc = windows::Win32::Foundation::RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, hgt) = (rc.right as u32, rc.bottom as u32);
            let desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: w,
                    Height: hgt,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                    RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                    ..Default::default()
                },
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 1,
                OutputWindow: hwnd,
                Windowed: true.into(),
                SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
                ..Default::default()
            };
            let (mut sc, mut dev, mut ctx) = (None, None, None);
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut sc),
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .expect("D3D11CreateDeviceAndSwapChain");
            let (sc, dev, ctx): (IDXGISwapChain, ID3D11Device, ID3D11DeviceContext) = (sc.unwrap(), dev.unwrap(), ctx.unwrap());
            let bb: ID3D11Texture2D = sc.GetBuffer(0).unwrap();
            let mut rtv = None;
            dev.CreateRenderTargetView(&bb, None, Some(&mut rtv)).unwrap();
            let rtv = rtv.unwrap();
            match create_game_mouse(hwnd) {
                Ok(m) => GAME_MOUSE.with(|g| *g.borrow_mut() = Some(m)),
                Err(e) => println!("   (no DirectInput mouse for the fake game: {e})"),
            }

            // capture hook in the Present slot, before the plugin hooks it
            let vtable = *(sc.as_raw() as *const *mut usize);
            let slot = vtable.add(8);
            let mut old = PAGE_PROTECTION_FLAGS(0);
            VirtualProtect(slot as *const c_void, 8, PAGE_READWRITE, &mut old).unwrap();
            ORIG.store(*slot, Ordering::Release);
            *slot = capture_present as *const () as usize;

            println!("== start with [overlay] enabled = true");
            let (lib, init, shutdown) = load(plugin);
            check(h::init(init) == 0, "scs_telemetry_init returns OK", failures);
            check(h::log_contains("in-game overlay enabled (Direct3D 11)"), "overlay hook installed", failures);
            check(*slot != capture_present as *const () as usize, "Present slot now points into the plugin", failures);
            h::load_truck();

            // game state the overlay must restore
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            let game_vp = D3D11_VIEWPORT { TopLeftX: 10.0, TopLeftY: 20.0, Width: 300.0, Height: 200.0, MinDepth: 0.0, MaxDepth: 1.0 };
            ctx.RSSetViewports(Some(&[game_vp]));

            frames(&sc, &ctx, &rtv, 12);
            let Some((cw, ch, img)) = CAPTURE.lock().unwrap().take() else {
                check(false, "frame captured", failures);
                return;
            };
            h::write_ppm(&out.join("d3d11-overlay.ppm"), cw, ch, &img);
            let stride = (cw * 4) as usize;
            let far = h::avg(&img, stride, 900, 500, 1000, 600);
            let corner = h::avg(&img, stride, 42, 42, 48, 48);
            println!("   background {far:?}, panel corner {corner:?}");
            check((far[0] - 40.0).abs() < 4.0 && (far[2] - 160.0).abs() < 4.0, "background untouched (sRGB colours exact)", failures);
            check(corner[2] < 80.0, "dark panel drawn at the top-left", failures);
            let bright = (45..300).any(|x| (45..250).any(|y| img[y * stride + x * 4] > 200 && img[y * stride + x * 4 + 2] > 200));
            check(bright, "panel contains light text", failures);
            let live_covered = h::covered(&img, [40, 90, 160]);

            let mut n = 1;
            let mut vp = [D3D11_VIEWPORT::default(); 1];
            ctx.RSGetViewports(&mut n, Some(vp.as_mut_ptr()));
            let mut rtvs = [None];
            ctx.OMGetRenderTargets(Some(&mut rtvs), None);
            check(vp[0] == game_vp, "game viewport restored", failures);
            check(rtvs[0].as_ref().map(Interface::as_raw) == Some(rtv.as_raw()), "game render target restored", failures);

            // edit mode via the real keyboard path
            println!("== Ctrl+Shift+Backspace");
            let _ = SetForegroundWindow(hwnd);
            pump();
            let down = [key(VK_CONTROL, false), key(VK_SHIFT, false), key(VK_BACK, false)];
            let up = [key(VK_BACK, true), key(VK_SHIFT, true), key(VK_CONTROL, true)];
            let sent = SendInput(&down, size_of::<INPUT>() as i32);
            pump();
            std::thread::sleep(Duration::from_millis(50));
            let st = |vk: VIRTUAL_KEY| GetAsyncKeyState(i32::from(vk.0)) as u16;
            println!(
                "   SendInput sent {sent}; GetAsyncKeyState ctrl={:#06x} shift={:#06x} back={:#06x}; foreground={}",
                st(VK_CONTROL), st(VK_SHIFT), st(VK_BACK), GetForegroundWindow() == hwnd
            );
            frames(&sc, &ctx, &rtv, 3);
            SendInput(&up, size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 3);
            let foreground = GetForegroundWindow() == hwnd;
            let mut edit_box = None;
            if let Some((cw, ch, img)) = CAPTURE.lock().unwrap().take() {
                h::write_ppm(&out.join("d3d11-edit.ppm"), cw, ch, &img);
                // edit panel bounds = pixels that differ from the background
                let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
                for y in 0..ch {
                    for x in 0..cw {
                        let i = ((y * cw + x) * 4) as usize;
                        if (img[i], img[i + 1], img[i + 2]) != (40, 90, 160) {
                            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                        }
                    }
                }
                edit_box = Some((x0, y0, x1, y1));
                let edit_covered = h::covered(&img, [40, 90, 160]);
                println!("   overlay pixels: live {live_covered}, edit {edit_covered} (foreground: {foreground})");
                check(edit_covered > live_covered + 20_000, "hotkey opened edit mode (control panel shown)", failures);
                let (_, _, w, _) = (0, 0, cw, ch);
                let corner = h::avg(&img, (w * 4) as usize, 42, 42, 48, 48);
                check(corner[2] < 80.0, "boxes stay visible in edit mode (preview)", failures);
            }
            check(h::log_contains("overlay edit mode on"), "game.log reports edit mode", failures);

            // Drag with the mouse the way ETS2 sees it: relative movement read
            // through DirectInput. The overlay starts its cursor in the middle
            // of the panel; move it to an empty spot in the top-left padding.
            println!("== mouse drag in edit mode (DirectInput, like ETS2)");
            let rel = |dx: i32, dy: i32| INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: 0 } },
            };
            // Windows "enhance pointer precision" doubles moves above 6 px;
            // send exact movements as 5 px steps.
            let exact = |dx: i32, dy: i32| {
                let n = (dx.abs().max(dy.abs()) + 4) / 5;
                let mut steps = Vec::new();
                let (mut sx, mut sy) = (0, 0);
                for i in 1..=n {
                    let (tx, ty) = (dx * i / n, dy * i / n);
                    steps.push(rel(tx - sx, ty - sy));
                    (sx, sy) = (tx, ty);
                }
                steps
            };
            let button = |flags: MOUSE_EVENT_FLAGS| INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
            };
            let (x0, y0, x1, y1) = edit_box.unwrap_or((40, 40, 600, 400));
            let (cx, cy) = (((x0 + x1) / 2) as i32, ((y0 + y1) / 2) as i32);
            println!("   edit panel {x0},{y0}..{x1},{y1}; cursor starts near {cx},{cy}");
            GAME_SAW.set(0);
            let _ = (cx, cy);
            // far up-left: the overlay cursor stops at the screen corner (0,0) …
            SendInput(&[rel(-3000, -3000)], size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 2);
            // … then exactly onto the panel's top-left padding
            SendInput(&exact(44, 44), size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 3);
            if let Some((cw, _, img)) = CAPTURE.lock().unwrap().take() {
                h::write_ppm(&out.join("d3d11-edit-cursor.ppm"), cw, img.len() as u32 / cw / 4, &img);
                let white = (43..56).any(|x: usize| (43..60).any(|y: usize| {
                    let i = (y * cw as usize + x) * 4;
                    img[i] > 230 && img[i + 1] > 230 && img[i + 2] > 230
                }));
                check(white, "overlay cursor drawn on top where the mouse points", failures);
            }
            SendInput(&[button(MOUSEEVENTF_LEFTDOWN)], size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 2);
            SendInput(&exact(200, 100), size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 3);
            SendInput(&[button(MOUSEEVENTF_LEFTUP)], size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 2);
            let saved = std::fs::read_to_string(&settings).unwrap_or_default();
            let line = |id: &str| saved.lines().find(|l| l.starts_with(&format!("{id} = "))).unwrap_or("").to_owned();
            let nums = |l: &str| l.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse::<i32>().ok()).collect::<Vec<_>>();
            let speed = nums(&line("speed"));
            let limit = nums(&line("limit"));
            println!("   saved: {} / {}", line("speed"), line("limit"));
            check(speed.len() == 2 && (speed[0] - 240).abs() <= 2 && (speed[1] - 140).abs() <= 2, "dragging the Speed box moved it and saved its position", failures);
            check(limit == vec![40, 80], "the other boxes stayed where they were", failures);
            check(GAME_SAW.get() == 0, "the game received no mouse input while editing", failures);

            // leave edit mode again (saves the file)
            SendInput(&down, size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 2);
            SendInput(&up, size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 2);
            GAME_SAW.set(0);
            SendInput(&[rel(30, 0)], size_of::<INPUT>() as i32);
            frames(&sc, &ctx, &rtv, 3);
            check(GAME_SAW.get() > 0, "the game gets the mouse again after edit mode", failures);

            shutdown();
            check(*slot == capture_present as *const () as usize, "Present slot restored on shutdown", failures);
            check(h::log_contains("in-game overlay removed"), "overlay removed on shutdown", failures);
            let _ = FreeLibrary(lib);
            frames(&sc, &ctx, &rtv, 2);
            println!("PASS present after unload did not crash");
            *slot = ORIG.load(Ordering::Acquire);
            let _ = DestroyWindow(hwnd);
        }
    }
}
