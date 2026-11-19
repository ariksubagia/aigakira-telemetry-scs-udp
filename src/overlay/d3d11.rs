//! Direct3D 11 backend (Windows build: the game under Proton/DXVK or on
//! Windows).
//!
//! Hooking: a throw-away device + swap chain on a hidden window yields the
//! `IDXGISwapChain` vtable of the DXGI implementation the game uses (DXVK's
//! under Proton). Its `Present` (slot 8) and `Present1` (slot 22) entries are
//! swapped for ours; the game's swap chain shares that vtable, so every frame
//! passes through [`present_hook`] before the original `Present`.
//!
//! Drawing: the panel image is uploaded to a dynamic texture (only when it
//! changed) and drawn as one viewport-sized quad with premultiplied-alpha
//! blending. All pipeline state touched is saved and restored. For an sRGB
//! back buffer (ETS2 uses `B8G8R8A8_UNORM_SRGB`) the texture is sRGB too, so
//! colours come out unchanged.
//!
//! Safety: hooks only run while `ACTIVE`; panics are caught; uninstall puts
//! the original pointers back and waits until no thread is inside a hook
//! before the plugin DLL can be unloaded.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_PRIMITIVE_TOPOLOGY, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT_TEST, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGISwapChain,
    IDXGISwapChain1,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_BACK, VK_CONTROL, VK_LBUTTON, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_OWNDC, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos, GetForegroundWindow,
    RegisterClassExW, UnregisterClassW, WINDOW_EX_STYLE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HRESULT, Interface, PCSTR, PCWSTR, s, w};

use super::{Core, Input, Shared};

const SLOT_PRESENT: usize = 8;
const SLOT_PRESENT1: usize = 22;

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> HRESULT;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static ORIG_PRESENT: AtomicUsize = AtomicUsize::new(0);
static ORIG_PRESENT1: AtomicUsize = AtomicUsize::new(0);
static VTABLE: AtomicUsize = AtomicUsize::new(0);
static STATE: Mutex<Option<Renderer>> = Mutex::new(None);

thread_local! {
    /// Set while this thread is inside a hook, so Present → Present1 (DXVK
    /// implements one through the other) draws only once.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

pub(crate) struct Hook {
    shared: Arc<Shared>,
}

impl Hook {
    pub(crate) fn install(core: Core) -> Result<Self, String> {
        if VTABLE.load(Ordering::Acquire) != 0 {
            return Err("overlay hook already installed".into());
        }
        let shared = Arc::clone(&core.shared);
        let (vtable, has_present1) = unsafe { swapchain_vtable()? };
        *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Renderer { core, gpu: None, failed: false, vcursor: None });
        unsafe {
            let orig = patch(vtable, SLOT_PRESENT, present_hook as *const () as usize)?;
            ORIG_PRESENT.store(orig, Ordering::Release);
            if has_present1 {
                match patch(vtable, SLOT_PRESENT1, present1_hook as *const () as usize) {
                    Ok(o) => ORIG_PRESENT1.store(o, Ordering::Release),
                    Err(e) => shared.log(format!("Present1 not hooked: {e}")),
                }
            }
        }
        VTABLE.store(vtable, Ordering::Release);
        ACTIVE.store(true, Ordering::Release);
        // Edit-mode mouse: take the game's DirectInput mouse while editing.
        if let Err(e) = super::dinput::install() {
            shared.log(format!("overlay edit mode will use the Windows cursor (DirectInput hook failed: {e})"));
        }
        Ok(Self { shared })
    }

    pub(crate) fn uninstall(&self) {
        super::dinput::uninstall();
        ACTIVE.store(false, Ordering::Release);
        let vtable = VTABLE.swap(0, Ordering::AcqRel);
        if vtable != 0 {
            unsafe {
                restore(vtable, SLOT_PRESENT, present_hook as *const () as usize, ORIG_PRESENT.load(Ordering::Acquire));
                if ORIG_PRESENT1.load(Ordering::Acquire) != 0 {
                    restore(vtable, SLOT_PRESENT1, present1_hook as *const () as usize, ORIG_PRESENT1.load(Ordering::Acquire));
                }
            }
        }
        // A thread may have fetched our pointer just before the restore:
        // wait for every hook call to return, twice with a gap.
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
        // Releases the GPU objects (reference-counted, any thread).
        *STATE.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

// ---------------------------------------------------------------------------
// vtable patching
// ---------------------------------------------------------------------------

unsafe fn patch(vtable: usize, slot: usize, hook: usize) -> Result<usize, String> {
    let entry = (vtable as *mut usize).wrapping_add(slot);
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe {
        VirtualProtect(entry as *const c_void, size_of::<usize>(), PAGE_READWRITE, &mut old)
            .map_err(|e| format!("VirtualProtect: {e}"))?;
        let orig = entry.read_volatile();
        entry.write_volatile(hook);
        let _ = VirtualProtect(entry as *const c_void, size_of::<usize>(), old, &mut old);
        Ok(orig)
    }
}

unsafe fn restore(vtable: usize, slot: usize, ours: usize, orig: usize) {
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

unsafe extern "system" fn dummy_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Creates a temporary device + swap chain to read the swap chain vtable.
unsafe fn swapchain_vtable() -> Result<(usize, bool), String> {
    unsafe {
        let instance = GetModuleHandleW(PCWSTR::null()).map_err(|e| format!("GetModuleHandle: {e}"))?;
        let class = w!("aigakira_overlay_probe");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_OWNDC,
            lpfnWndProc: Some(dummy_wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("aigakira overlay probe"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            64,
            64,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| format!("probe window: {e}"))?;

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 64,
                Height: 64,
                RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
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
        let mut result = Err("D3D11CreateDeviceAndSwapChain failed".to_owned());
        for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            let mut sc: Option<IDXGISwapChain> = None;
            let mut dev: Option<ID3D11Device> = None;
            let r = D3D11CreateDeviceAndSwapChain(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut sc),
                Some(&mut dev),
                None,
                None,
            );
            match (r, sc) {
                (Ok(()), Some(sc)) => {
                    let vtable = *(sc.as_raw() as *const usize);
                    let has1 = sc.cast::<IDXGISwapChain1>().is_ok();
                    result = Ok((vtable, has1));
                    break;
                }
                (Err(e), _) => result = Err(format!("D3D11CreateDeviceAndSwapChain: {e}")),
                (Ok(()), None) => {}
            }
        }
        let _ = DestroyWindow(hwnd);
        let _ = UnregisterClassW(class, Some(instance.into()));
        result
    }
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

/// Runs `draw` unless nested; never lets a panic reach the game.
fn guarded(this: *mut c_void, flags: u32) {
    if !ACTIVE.load(Ordering::Acquire) || flags & DXGI_PRESENT_TEST.0 != 0 || IN_HOOK.get() {
        return;
    }
    IN_HOOK.set(true);
    if catch_unwind(AssertUnwindSafe(|| draw(this))).is_err() {
        ACTIVE.store(false, Ordering::Release);
    }
    IN_HOOK.set(false);
}

unsafe extern "system" fn present_hook(this: *mut c_void, sync: u32, flags: u32) -> HRESULT {
    IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    guarded(this, flags);
    let orig: PresentFn = unsafe { std::mem::transmute(ORIG_PRESENT.load(Ordering::Acquire)) };
    let was = IN_HOOK.replace(true); // Present → Present1 inside DXVK: no second draw
    let hr = unsafe { orig(this, sync, flags) };
    IN_HOOK.set(was);
    IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    hr
}

unsafe extern "system" fn present1_hook(this: *mut c_void, sync: u32, flags: u32, params: *const c_void) -> HRESULT {
    IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    guarded(this, flags);
    let orig: Present1Fn = unsafe { std::mem::transmute(ORIG_PRESENT1.load(Ordering::Acquire)) };
    let was = IN_HOOK.replace(true);
    let hr = unsafe { orig(this, sync, flags, params) };
    IN_HOOK.set(was);
    IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    hr
}

fn draw(this: *mut c_void) {
    let Some(sc) = (unsafe { IDXGISwapChain::from_raw_borrowed(&this) }) else { return };
    let Ok(mut guard) = STATE.try_lock() else { return };
    let Some(r) = guard.as_mut() else { return };
    if r.failed {
        return;
    }
    if let Err(e) = unsafe { r.draw(sc) } {
        r.failed = true;
        r.core.shared.log(format!("overlay disabled for this session: {e}"));
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

struct Renderer {
    core: Core,
    gpu: Option<Gpu>,
    failed: bool,
    /// Edit-mode cursor driven by the game's DirectInput mouse movement.
    vcursor: Option<(f32, f32)>,
}

struct Gpu {
    device: usize,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    depth: ID3D11DepthStencilState,
    /// One texture per sprite id, with the sprite version it holds.
    textures: HashMap<u32, (Tex, u64)>,
}

struct Tex {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    w: u32,
    h: u32,
    srgb: bool,
}

const HLSL: &str = r#"
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    float2 uv = float2((float)(id & 1u), (float)(id >> 1u));
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps_main(VSOut i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

type D3DCompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    PCSTR,
    *const c_void,
    *const c_void,
    PCSTR,
    PCSTR,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> HRESULT;

/// Compiles HLSL with d3dcompiler_47 (loaded at runtime so the plugin never
/// fails to load where it is missing; Wine/Proton ship a builtin one).
unsafe fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, String> {
    unsafe {
        let mut lib = None;
        for name in [w!("d3dcompiler_47.dll"), w!("d3dcompiler_46.dll"), w!("d3dcompiler_43.dll")] {
            if let Ok(h) = LoadLibraryW(name) {
                lib = Some(h);
                break;
            }
        }
        let lib = lib.ok_or("d3dcompiler_47.dll not found")?;
        let f = GetProcAddress(lib, s!("D3DCompile")).ok_or("D3DCompile not found")?;
        let f: D3DCompileFn = std::mem::transmute(f);
        let mut code: *mut c_void = std::ptr::null_mut();
        let mut errors: *mut c_void = std::ptr::null_mut();
        let hr = f(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            s!("aigakira_overlay"),
            std::ptr::null(),
            std::ptr::null(),
            entry,
            target,
            0,
            0,
            &mut code,
            &mut errors,
        );
        let errors = (!errors.is_null()).then(|| ID3DBlob::from_raw(errors));
        if hr.is_err() || code.is_null() {
            let msg = errors
                .map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())).into_owned())
                .unwrap_or_default();
            return Err(format!("shader compile failed ({hr}): {msg}"));
        }
        let blob = ID3DBlob::from_raw(code);
        Ok(std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()).to_vec())
    }
}

fn some<T>(o: Option<T>, what: &str) -> Result<T, String> {
    o.ok_or_else(|| format!("{what} not created"))
}

impl Gpu {
    unsafe fn new(device: &ID3D11Device) -> Result<Self, String> {
        unsafe {
            let vs_code = compile(s!("vs_main"), s!("vs_4_0"))?;
            let ps_code = compile(s!("ps_main"), s!("ps_4_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vs_code, None, Some(&mut vs)).map_err(|e| format!("vertex shader: {e}"))?;
            let mut ps = None;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps)).map_err(|e| format!("pixel shader: {e}"))?;

            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler)).map_err(|e| format!("sampler: {e}"))?;

            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&bd, Some(&mut blend)).map_err(|e| format!("blend state: {e}"))?;

            let rd = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            };
            let mut raster = None;
            device.CreateRasterizerState(&rd, Some(&mut raster)).map_err(|e| format!("rasterizer: {e}"))?;

            let dd = D3D11_DEPTH_STENCIL_DESC {
                DepthEnable: false.into(),
                DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D11_COMPARISON_ALWAYS,
                StencilEnable: false.into(),
                ..Default::default()
            };
            let mut depth = None;
            device.CreateDepthStencilState(&dd, Some(&mut depth)).map_err(|e| format!("depth state: {e}"))?;

            Ok(Self {
                device: device.as_raw() as usize,
                vs: some(vs, "vertex shader")?,
                ps: some(ps, "pixel shader")?,
                sampler: some(sampler, "sampler")?,
                blend: some(blend, "blend state")?,
                raster: some(raster, "rasterizer")?,
                depth: some(depth, "depth state")?,
                textures: HashMap::new(),
            })
        }
    }

}

/// Uploads `img` into the texture in `slot`, (re)creating it when the size
/// or colour space changed.
unsafe fn upload(slot: &mut Option<Tex>, device: &ID3D11Device, ctx: &ID3D11DeviceContext, img: &tiny_skia::Pixmap, srgb: bool) -> Result<(), String> {
        unsafe {
            let (w, h) = (img.width(), img.height());
            if !slot.as_ref().is_some_and(|t| t.w == w && t.h == h && t.srgb == srgb) {
                *slot = None;
                let td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: if srgb { DXGI_FORMAT_R8G8B8A8_UNORM_SRGB } else { DXGI_FORMAT_R8G8B8A8_UNORM },
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    MiscFlags: 0,
                };
                let mut tex = None;
                device.CreateTexture2D(&td, None, Some(&mut tex)).map_err(|e| format!("texture: {e}"))?;
                let tex = tex.ok_or("texture not created")?;
                let mut srv = None;
                device.CreateShaderResourceView(&tex, None, Some(&mut srv)).map_err(|e| format!("texture view: {e}"))?;
                *slot = Some(Tex { tex, srv: srv.ok_or("texture view not created")?, w, h, srgb });
            }
            let t = slot.as_ref().ok_or("no texture")?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&t.tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).map_err(|e| format!("map: {e}"))?;
            let src = img.data();
            let row = w as usize * 4;
            for y in 0..h as usize {
                let dst = (mapped.pData as *mut u8).add(y * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src.as_ptr().add(y * row), dst, row);
            }
            ctx.Unmap(&t.tex, 0);
            Ok(())
        }
}

/// Everything the overlay draw changes, to put back afterwards.
struct Saved {
    rtvs: [Option<ID3D11RenderTargetView>; 8],
    dsv: Option<ID3D11DepthStencilView>,
    blend: Option<ID3D11BlendState>,
    blend_factor: [f32; 4],
    sample_mask: u32,
    depth: Option<ID3D11DepthStencilState>,
    stencil_ref: u32,
    raster: Option<ID3D11RasterizerState>,
    viewports: [D3D11_VIEWPORT; 16],
    n_viewports: u32,
    scissors: [RECT; 16],
    n_scissors: u32,
    vs: Option<ID3D11VertexShader>,
    ps: Option<ID3D11PixelShader>,
    gs: Option<ID3D11GeometryShader>,
    hs: Option<ID3D11HullShader>,
    ds: Option<ID3D11DomainShader>,
    srv: [Option<ID3D11ShaderResourceView>; 1],
    sampler: [Option<ID3D11SamplerState>; 1],
    topology: D3D_PRIMITIVE_TOPOLOGY,
    layout: Option<ID3D11InputLayout>,
    vb: Option<ID3D11Buffer>,
    vb_stride: u32,
    vb_offset: u32,
    ib: Option<ID3D11Buffer>,
    ib_format: DXGI_FORMAT,
    ib_offset: u32,
}

impl Saved {
    unsafe fn capture(ctx: &ID3D11DeviceContext) -> Self {
        unsafe {
            let mut s = Saved {
                rtvs: Default::default(),
                dsv: None,
                blend: None,
                blend_factor: [0.0; 4],
                sample_mask: 0,
                depth: None,
                stencil_ref: 0,
                raster: ctx.RSGetState().ok(),
                viewports: [D3D11_VIEWPORT::default(); 16],
                n_viewports: 16,
                scissors: [RECT::default(); 16],
                n_scissors: 16,
                vs: None,
                ps: None,
                gs: None,
                hs: None,
                ds: None,
                srv: [None],
                sampler: [None],
                topology: ctx.IAGetPrimitiveTopology(),
                layout: ctx.IAGetInputLayout().ok(),
                vb: None,
                vb_stride: 0,
                vb_offset: 0,
                ib: None,
                ib_format: DXGI_FORMAT_UNKNOWN,
                ib_offset: 0,
            };
            ctx.OMGetRenderTargets(Some(&mut s.rtvs), Some(&mut s.dsv));
            ctx.OMGetBlendState(Some(&mut s.blend), Some(&mut s.blend_factor), Some(&mut s.sample_mask));
            ctx.OMGetDepthStencilState(Some(&mut s.depth), Some(&mut s.stencil_ref));
            ctx.RSGetViewports(&mut s.n_viewports, Some(s.viewports.as_mut_ptr()));
            ctx.RSGetScissorRects(&mut s.n_scissors, Some(s.scissors.as_mut_ptr()));
            ctx.VSGetShader(&mut s.vs, None, None);
            ctx.PSGetShader(&mut s.ps, None, None);
            ctx.GSGetShader(&mut s.gs, None, None);
            ctx.HSGetShader(&mut s.hs, None, None);
            ctx.DSGetShader(&mut s.ds, None, None);
            ctx.PSGetShaderResources(0, Some(&mut s.srv));
            ctx.PSGetSamplers(0, Some(&mut s.sampler));
            ctx.IAGetVertexBuffers(0, 1, Some(&mut s.vb), Some(&mut s.vb_stride), Some(&mut s.vb_offset));
            ctx.IAGetIndexBuffer(Some(&mut s.ib), Some(&mut s.ib_format), Some(&mut s.ib_offset));
            s
        }
    }

    unsafe fn restore(self, ctx: &ID3D11DeviceContext) {
        unsafe {
            ctx.OMSetRenderTargets(Some(&self.rtvs), self.dsv.as_ref());
            ctx.OMSetBlendState(self.blend.as_ref(), Some(&self.blend_factor), self.sample_mask);
            ctx.OMSetDepthStencilState(self.depth.as_ref(), self.stencil_ref);
            ctx.RSSetState(self.raster.as_ref());
            ctx.RSSetViewports(Some(&self.viewports[..self.n_viewports.min(16) as usize]));
            ctx.RSSetScissorRects(Some(&self.scissors[..self.n_scissors.min(16) as usize]));
            ctx.VSSetShader(self.vs.as_ref(), None);
            ctx.PSSetShader(self.ps.as_ref(), None);
            ctx.GSSetShader(self.gs.as_ref(), None);
            ctx.HSSetShader(self.hs.as_ref(), None);
            ctx.DSSetShader(self.ds.as_ref(), None);
            ctx.PSSetShaderResources(0, Some(&self.srv));
            ctx.PSSetSamplers(0, Some(&self.sampler));
            ctx.IASetPrimitiveTopology(self.topology);
            ctx.IASetInputLayout(self.layout.as_ref());
            ctx.IASetVertexBuffers(0, 1, Some(&self.vb), Some(&self.vb_stride), Some(&self.vb_offset));
            ctx.IASetIndexBuffer(self.ib.as_ref(), self.ib_format, self.ib_offset);
        }
    }
}

fn srgb_format(f: DXGI_FORMAT) -> bool {
    matches!(f, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB | DXGI_FORMAT_B8G8R8X8_UNORM_SRGB)
}

/// Typeless back buffers need a concrete view format.
fn view_format(f: DXGI_FORMAT) -> DXGI_FORMAT {
    match f {
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_B8G8R8X8_TYPELESS => DXGI_FORMAT_B8G8R8X8_UNORM,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        other => other,
    }
}

fn poll_input(hwnd: HWND, bw: u32, bh: u32) -> Input {
    unsafe {
        if GetForegroundWindow() != hwnd {
            return Input::default();
        }
        let down = |vk: i32| (GetAsyncKeyState(vk) as u16) & 0x8000 != 0;
        let hotkey = down(i32::from(VK_CONTROL.0)) && down(i32::from(VK_SHIFT.0)) && down(i32::from(VK_BACK.0));
        let left_button = down(i32::from(VK_LBUTTON.0));
        let mut p = POINT::default();
        let mut rc = RECT::default();
        let pointer = (GetCursorPos(&mut p).is_ok()
            && ScreenToClient(hwnd, &mut p).as_bool()
            && GetClientRect(hwnd, &mut rc).is_ok()
            && rc.right > 0
            && rc.bottom > 0)
            .then(|| {
                let sx = bw as f32 / rc.right as f32;
                let sy = bh as f32 / rc.bottom as f32;
                (p.x as f32 * sx, p.y as f32 * sy)
            });
        Input { pointer, left_button, hotkey }
    }
}

impl Renderer {
    unsafe fn draw(&mut self, sc: &IDXGISwapChain) -> Result<(), String> {
        unsafe {
            let desc = sc.GetDesc().map_err(|e| format!("GetDesc: {e}"))?;
            let (bw, bh) = (desc.BufferDesc.Width, desc.BufferDesc.Height);
            let mut input = poll_input(desc.OutputWindow, bw, bh);
            // In edit mode the game's own DirectInput mouse drives an overlay
            // cursor (the Windows cursor does not follow the game's cursor).
            let taken = if self.core.edit && input.pointer.is_some() { super::dinput::take() } else { None };
            match taken {
                Some((dx, dy, left)) => {
                    let start = self.core.panel_center();
                    let c = self.vcursor.get_or_insert(start);
                    c.0 = (c.0 + dx as f32).clamp(0.0, bw.saturating_sub(1) as f32);
                    c.1 = (c.1 + dy as f32).clamp(0.0, bh.saturating_sub(1) as f32);
                    input.pointer = Some(*c);
                    input.left_button = left;
                }
                None => self.vcursor = None,
            }

            let device: ID3D11Device = match sc.GetDevice() {
                Ok(d) => d,
                Err(_) => return Err("the game does not render with Direct3D 11".into()),
            };
            if self.gpu.as_ref().is_none_or(|g| g.device != device.as_raw() as usize) {
                self.gpu = Some(Gpu::new(&device)?);
                self.core.shared.log(format!("overlay drawing with Direct3D 11 on a {bw}x{bh} {:?} back buffer", desc.BufferDesc.Format));
            }
            let ctx = device.GetImmediateContext().map_err(|e| format!("context: {e}"))?;
            let backbuffer: ID3D11Texture2D = sc.GetBuffer(0).map_err(|e| format!("GetBuffer: {e}"))?;
            let mut bb_desc = D3D11_TEXTURE2D_DESC::default();
            backbuffer.GetDesc(&mut bb_desc);
            let srgb = srgb_format(bb_desc.Format);

            let frame = self.core.frame((bw, bh), input);
            super::dinput::set_editing(frame.edit);
            let gpu = self.gpu.as_mut().expect("created above");
            gpu.textures.retain(|id, _| frame.sprites.iter().any(|s| s.id == *id));
            if frame.sprites.is_empty() {
                return Ok(());
            }
            // upload only sprites whose image changed
            for sp in &frame.sprites {
                let fresh = gpu.textures.get(&sp.id).is_some_and(|(t, v)| *v == sp.version && t.srgb == srgb);
                if !fresh {
                    let mut slot = gpu.textures.remove(&sp.id).map(|(t, _)| t);
                    upload(&mut slot, &device, &ctx, sp.image, srgb)?;
                    if let Some(t) = slot {
                        gpu.textures.insert(sp.id, (t, sp.version));
                    }
                }
            }

            let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: view_format(bb_desc.Format),
                ViewDimension: if bb_desc.SampleDesc.Count > 1 { D3D11_RTV_DIMENSION_TEXTURE2DMS } else { D3D11_RTV_DIMENSION_TEXTURE2D },
                ..Default::default()
            };
            let mut rtv = None;
            device
                .CreateRenderTargetView(&backbuffer, Some(&rtv_desc), Some(&mut rtv))
                .map_err(|e| format!("render target view: {e}"))?;
            let rtv = rtv.ok_or("render target view not created")?;

            let saved = Saved::capture(&ctx);
            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None::<&ID3D11DepthStencilView>);
            ctx.OMSetBlendState(&gpu.blend, Some(&[0.0; 4]), 0xffff_ffff);
            ctx.OMSetDepthStencilState(&gpu.depth, 0);
            ctx.RSSetState(&gpu.raster);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None::<&ID3D11InputLayout>);
            ctx.IASetVertexBuffers(0, 1, Some(&None), Some(&0), Some(&0));
            ctx.VSSetShader(&gpu.vs, None);
            ctx.PSSetShader(&gpu.ps, None);
            ctx.GSSetShader(None::<&ID3D11GeometryShader>, None);
            ctx.HSSetShader(None::<&ID3D11HullShader>, None);
            ctx.DSSetShader(None::<&ID3D11DomainShader>, None);
            ctx.PSSetSamplers(0, Some(&[Some(gpu.sampler.clone())]));
            // in order: boxes, then the control panel, then the cursor
            for sp in &frame.sprites {
                let Some((t, _)) = gpu.textures.get(&sp.id) else { continue };
                ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: sp.x as f32,
                    TopLeftY: sp.y as f32,
                    Width: t.w as f32,
                    Height: t.h as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                ctx.PSSetShaderResources(0, Some(&[Some(t.srv.clone())]));
                ctx.Draw(4, 0);
            }
            saved.restore(&ctx);
            Ok(())
        }
    }
}
