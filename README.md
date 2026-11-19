# aigakira-telemetry-scs-udp

SCS Telemetry SDK plugin for **Euro Truck Simulator 2** (and ATS), built in
Rust, that streams truck telemetry as compact JSON over UDP. It builds as a
Windows x64 DLL for the game under **Steam Proton** (Wine sockets are native
Linux sockets, so `127.0.0.1` reaches Linux receivers) and as a Linux `.so`
for the **native Linux** game.

It can also draw an **[in-game overlay](#in-game-overlay)**: telemetry
boxes rendered into the game's own frames, one per item, each placed where
you drag it in an edit mode on **Ctrl+Shift+Backspace**. Nothing
else needs to be installed. It is **on by default**. The plugin writes a
[settings file](#settings-file) on its first start, where you can turn it off.

```
ETS2 game thread (Proton)                      background thread                 Linux
──────────────────────────                     ─────────────────                 ─────
SDK channel callbacks ──► Snapshot (plain Copy struct)
FrameEnd / Paused / Started / Config
        │  Mutex::try_lock (never waits)  ──►  latest snapshot ──► serde_json ──► UDP ──► receiver
        └─ sync_channel(1).try_send on a    ──►  wake: send now                  127.0.0.1:45454
           discrete change (blinker, cruise…)    else every 1/Hz s                (Touch Portal)
```

Every datagram goes to each configured receiver. A UDP port can only be bound
by one program, so list several if needed.

* **SDK integration:** [`scs-sdk-plugin`](https://crates.io/crates/scs-sdk-plugin)
  0.1.1, a typed, safe wrapper. Its `export_plugin!` macro generates exactly
  `scs_telemetry_init` and `scs_telemetry_shutdown`, and its runtime wraps
  **every** foreign entry point (init, each channel/event callback, shutdown)
  in `catch_unwind`. Every module is `#![forbid(unsafe_code)]` except the two
  overlay renderer backends, which must hook the game's graphics API and also
  catch panics themselves. The release profile keeps `panic = "unwind"` on
  purpose, because with `abort` that panic containment could not work.
* **Non-blocking:** callbacks only copy values into a struct. Handing it to the
  worker uses `try_lock` and `try_send`, so the game thread never waits on the
  worker or on the network.
* **Compact and checked:** the worst-case datagram is about 1280 bytes and a
  normal one is about 950 (unit-tested `< 1400`). Every datagram carries `schema`,
  `seq`, `sid` and `age_ms`.

## Build (Windows DLL for Proton, Linux .so for the native game)

```bash
# one-time toolchain setup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if Rust is missing
rustup target add x86_64-pc-windows-gnu
sudo pacman -S mingw-w64-gcc          # Debian/Ubuntu: sudo apt install gcc-mingw-w64-x86-64

# unit tests (run natively on Linux: JSON encoding, size bound, UDP worker)
cargo test

# the DLL (game under Proton)
cargo build --release --target x86_64-pc-windows-gnu
ls -l target/x86_64-pc-windows-gnu/release/aigakira_telemetry_ets2_udp.dll

# the .so (native Linux game); exports the same two symbols
cargo build --release
nm -D --defined-only target/release/libaigakira_telemetry_ets2_udp.so | grep scs_
```

The repo is a Cargo workspace. Plain `cargo build`/`cargo test` cover only the
game plugin. `harness/` is the overlay test host (see
[Testing the overlay](#testing-the-overlay)).

`.cargo/config.toml` sets the MinGW linker and `+crt-static`, so the DLL needs
no `libgcc_s_seh-1.dll` or `libwinpthread-1.dll`.

MSVC alternative (no MinGW, uses the Windows SDK through `cargo-xwin`):

```bash
cargo install cargo-xwin
rustup target add x86_64-pc-windows-msvc
cargo xwin build --release --target x86_64-pc-windows-msvc
```

### Verify the exported symbols and imports

```bash
DLL=target/x86_64-pc-windows-gnu/release/aigakira_telemetry_ets2_udp.dll

file "$DLL"        # PE32+ executable (DLL) ... x86-64, for MS Windows

# exactly two exports are expected: scs_telemetry_init, scs_telemetry_shutdown
x86_64-w64-mingw32-objdump -p "$DLL" | sed -n '/\[Ordinal\/Name Pointer\] Table/,/^$/p'
# or: llvm-readobj --coff-exports "$DLL"
# or: llvm-objdump -p "$DLL" | grep -A4 'Export Table'

# imports should be Windows system DLLs only (KERNEL32, ntdll, WS2_32, USER32,
# GDI32, d3d11, dxgi, api-ms-win-*, msvcrt/ucrt, bcryptprimitives...), never
# libgcc/libwinpthread. d3dcompiler_47 is loaded at runtime, not imported.
x86_64-w64-mingw32-objdump -p "$DLL" | grep 'DLL Name'
```

## Install

```bash
tools/install-game-plugin.sh --dry-run   # show what goes where
tools/install-game-plugin.sh             # build both and install into every ETS2/ATS in your Steam libraries
```

It installs the DLL into `bin/win_x64/plugins/` of Proton installs and the
`.so` into `bin/linux_x64/plugins/` of native installs (the game folder only has
the directory for the platform Steam installed). Manually:

```bash
ETS2="$HOME/.local/share/Steam/steamapps/common/Euro Truck Simulator 2"
# Proton (Windows build of the game):
mkdir -p "$ETS2/bin/win_x64/plugins"
cp target/x86_64-pc-windows-gnu/release/aigakira_telemetry_ets2_udp.dll "$ETS2/bin/win_x64/plugins/"
# native Linux build of the game:
mkdir -p "$ETS2/bin/linux_x64/plugins"
cp target/release/libaigakira_telemetry_ets2_udp.so "$ETS2/bin/linux_x64/plugins/aigakira_telemetry_ets2_udp.so"
```

The plugin must be **inside `plugins/`**, not next to the game executable.
Restart the game after installing.

## Settings file

On its first start the plugin creates **`aigakira_telemetry.toml` next to
itself**, i.e. in the same `plugins/` folder. It contains comments explaining
every value and the list of available overlay items. `game.log.txt` shows the
path:

```
[aigakira-udp] created settings file …/bin/win_x64/plugins/aigakira_telemetry.toml; the in-game overlay is on (…)
```

```toml
[overlay]
enabled = true           # false + restart the game = in-game overlay off
scale = 1.0              # 0.5–3.0; 1.0 = sized for 1080p, scales with resolution
opacity = 0.8            # 0.15–1.0
hide_when_idle = true    # hide when no truck is active (edit mode always shows them)
visible = true           # false = "Hide all": no boxes in game, selection and places kept
editor = { x = 700, y = 300 }   # where you left the edit-mode control panel (written by edit mode)

[overlay.boxes]          # one box per shown item: top-left corner in screen pixels
speed = { x = 40, y = 40 }
limit = { x = 40, y = 80 }
gear = { x = 1700, y = 950 }
rpm = { x = 900, y = 40, hidden = true }   # unchecked: not shown, but keeps its place
# … delete a line to forget an item, add one to show it (or use edit mode)

[udp]
enabled = true
targets = ["127.0.0.1:45454"]   # one or more "host:port"
hz = 30                          # 1–120
```

* Changing `[overlay] enabled` or anything in `[udp]` needs a game restart.
  The other overlay values are picked up within a second while the game runs.
* Edit mode writes its changes back into this file, keeping your comments.
* Files from v0.3 (one panel: `x`, `y`, `items = [...]`) still load. Their
  items become boxes stacked at the old panel position, and the next save
  rewrites them as `[overlay.boxes]`.
* Invalid values fall back to defaults with a warning in `game.log.txt`. If
  the file is unreadable or not valid TOML, all defaults are used.

### Environment overrides (optional)

Proton and the native game both pass environment variables through. Set them
in Steam: **ETS2 → Properties → Launch Options**. They override `[udp]`:

```
AIGAKIRA_UDP_TARGET=127.0.0.1:45454,192.168.1.20:45454 AIGAKIRA_UDP_HZ=30 %command%
```

| Variable              | Default (from the file) | Meaning |
|-----------------------|-------------------|-----------------------------------------------|
| `AIGAKIRA_UDP_TARGET` | `127.0.0.1:45454` | Comma-separated receivers (`host:port` / `[v6]:port`, up to 8). Each gets every datagram from its own socket. |
| `AIGAKIRA_UDP_HZ`     | `30`              | Periodic rate, 1–120. Discrete changes are always sent immediately. |

## Wire format (`aigakira.scs.telemetry/1`)

One JSON object per datagram (whitespace added here):

```json
{"schema":"aigakira.scs.telemetry/1","seq":1834,"sid":2718281828,"ts":1763577600123,
 "age_ms":12,"alive":true,"game":"ets2","game_ver":"1.18","paused":false,"truck":true,
 "drive":{"speed_ms":22.21,"speed_kmh":80.0,"rpm":1150.0,"rpm_max":2500.0,"gear":10,"dgear":10,
          "gear_label":"10","gears_fwd":12,"gears_rev":2},
 "input":{"steer":0.0,"throttle":0.0,"brake":0.0,"clutch":0.0},
 "effective":{"steer":-0.004,"throttle":0.41,"brake":0.0,"clutch":0.0},
 "fuel":{"l":512.3,"cap_l":800.0,"pct":64.0,"range_km":1630.0,"avg_l100km":31.4,"warn":false},
 "cruise":{"on":true,"set_ms":22.22,"set_kmh":80.0},
 "ind":{"lblinker":false,"rblinker":true,"hazard":false,"lblinker_lamp":false,"rblinker_lamp":true,
        "park_brake":false,"engine_brake":false,"retarder":0,"retarder_steps":4,
        "engine":true,"electric":true,"wipers":false,"diff_lock":false},
 "lights":{"parking":true,"low":true,"high":false,"beacon":false,"brake":false,"reverse":false,
           "aux_front":0,"aux_roof":0},
 "warn":{"air":false,"air_emerg":false,"oil":false,"water":false,"battery":false,"adblue":false},
 "nav":{"limit_kmh":80.0}}
```

| Field | Notes |
|---|---|
| `seq` | +1 per datagram. A gap means loss; a backwards step with the same `sid` means reordering. |
| `sid` | Changes on every plugin load. Reset your `seq` tracking when it changes. |
| `age_ms` | Time since the game thread last published. It rises during loading screens or stalls, so treat > ~1000 as stale. |
| `alive` | `false` only in the final datagram, sent from `scs_telemetry_shutdown`. |
| `paused` | SDK pause state: `true` in the main menu, the ESC menu and loading screens. The plugin starts paused. |
| `truck` | `false` when no truck configuration is active (main menu or profile selection). All values are then safe defaults. |
| `ind.lblinker` / `rblinker` / `hazard` | **Logical switch** state: steady `true` while switched on. |
| `ind.*blinker_lamp` | Lamp actually lit, so it blinks. When hazards are on, both lamps blink. |
| `cruise.on` | Derived from `truck.cruise_control` (m/s, `0` = disabled). `set_*` are `0` when off. |
| `ind.retarder` | `0` = off … `retarder_steps` (from the truck configuration). |
| `lights.aux_*` | `0` = off, `1` = dimmed, `2` = full. |
| `null` values | The channel doesn't exist in the running game's telemetry schema (e.g. `hazard` before ETS2 schema 1.17, `adblue` in ATS). |

Floats are rounded (2–3 decimals for controls, 1 for km/h). `NaN` and `±inf` are
sent as `0`, so the output is always valid JSON. Speeds are negative in reverse.

Every channel name and type was checked against the `scs-sdk` 0.1.1 descriptors
and the raw `scs-sdk-sys` header constants; see the table at the top of
[src/snapshot.rs](src/snapshot.rs). Because routing goes through typed
descriptors, a field whose type differs from the SDK type fails to compile.

## Linux receiver

```bash
python3 tools/udp_listen.py              # one status line per datagram
python3 tools/udp_listen.py --changes    # only discrete state changes (best for testing toggles)
python3 tools/udp_listen.py --raw        # raw JSON
```

The receiver reports invalid JSON, oversize datagrams, dropped and out-of-order
packets, plugin restarts and stale data. A minimal receiver looks like this:

```python
import json, socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.bind(("127.0.0.1", 45454))
while True:
    p = json.loads(s.recv(2048))
    print(p["seq"], p["drive"]["speed_kmh"], p["ind"]["hazard"], p["cruise"])
```

Shell-only check: `socat -u UDP-RECV:45454 STDOUT` or `nc -ul 127.0.0.1 45454`.

## Verification checklist

Paths below assume ETS2 (Steam app id `227300`); for ATS use `270880` and
"American Truck Simulator".

```bash
LOG="$HOME/.local/share/Steam/steamapps/compatdata/227300/pfx/drive_c/users/steamuser/Documents/Euro Truck Simulator 2/game.log.txt"
# Proton with the game in another Steam library: replace ~/.local/share/Steam with that library path.
# Native Linux game:
# LOG="$HOME/.local/share/Euro Truck Simulator 2/game.log.txt"
```

1. **Before launching:** `cargo test` passes, the export check shows exactly
   two exports, and the DLL is in `bin/win_x64/plugins/`.
2. **Start the receiver:** `python3 tools/udp_listen.py --changes`.
3. **Launch ETS2.** On the first start with a new SDK plugin, the game shows an
   "Advanced SDK features" dialog; accept it.
4. **Check `game.log.txt`:** `grep -iE 'telemetry|aigakira' "$LOG"`. Expect:
   * the SDK loading the plugin from `plugins/…dll`;
   * the framework's line with the plugin name `aigakira-telemetry-udp`, the
     game, the API version and the subscription counts;
   * `[aigakira-udp] settings: …/plugins/aigakira_telemetry.toml` (or `created settings file …` the first time);
   * `[aigakira-udp] in-game overlay disabled in settings`, or `in-game overlay enabled (Direct3D 11)` / `(OpenGL (GLX))`;
   * `[aigakira-udp] v0.4.4 streaming JSON to udp://127.0.0.1:45454 at 30 Hz …`;
   * `[aigakira-udp] capabilities: Caps { … }` (all `true` on current ETS2).

   No such lines means the DLL is in the wrong folder or failed to load. A
   framework init error names the failing subscription.
5. **Main menu:** datagrams arrive with `paused=true`, `truck=false`, zero
   speed and gear `N`, and `age_ms` stays small. Steady output means the
   menu edge case works.
6. **Load a profile and enter the truck:** `truck=true`. When the simulation
   runs, `paused=false`.
7. **Toggle while parked, watching `--changes`:**
   | Action (default keys) | Expected change |
   |---|---|
   | Left blinker `[` | `lblinker=true`; `lblinker_lamp` blinks (summary mode) |
   | Right blinker `]` | `rblinker=true` |
   | Hazards `F` | `hazard=true`; both lamps blink |
   | Parking brake `Space` | `park_brake` toggles |
   | Engine on/off `E` | `engine` toggles |
   | Lights `L`, high beam `K`, beacon `O` | `light.parking` / `light.low` / `light.high` / `light.beacon` |
   | Engine brake `B` | `engine_brake` toggles |
   | Retarder up/down `;` / `'` | `retarder` changes 0…`retarder_steps` |
8. **Drive:** in summary mode, check km/h against the dashboard, `gear_label`
   against the gear display, and that throttle and brake respond. Enable cruise
   control (`C`) above 30 km/h: `cruise_on=true` and `cruise_kmh` match the HUD.
   Adjust it with the cruise +/− keys and `cruise_kmh` follows. Brake: `cruise_on=false`.
9. **Pause (ESC):** `paused=true`. Values freeze at their last state and no
   errors appear. Resume: `paused=false`.
10. **Loading edge case:** during a long load, such as a ferry or train, or
    teleporting to a service station, the receiver may warn
    `stale: game thread silent …`; it recovers afterwards. There must be no
    invalid-JSON or oversize warnings.
11. **Quit to main menu:** `truck=false` and values reset to defaults.
12. **Exit the game:** the receiver prints `plugin shut down cleanly`
    (`alive=false`). `game.log.txt` shows
    `[aigakira-udp] stopped: N datagrams sent, … worker joined: true`.
    `send errors` is non-zero only if the receiver was not running.
13. **Robustness:** launch with no receiver running. The game must behave
    normally, because sends to a closed port are only counted. Also launch with
    `AIGAKIRA_UDP_TARGET=garbage`: the log shows a warning and the plugin falls
    back to the default target.

## Troubleshooting

* **Nothing in the log:** the folder must be `bin/win_x64/plugins` (64-bit),
  not `bin/win_x86`.
* **The log shows a load failure:** run the import check above. Any
  `libgcc_s_seh-1.dll` or `libwinpthread-1.dll` import means `+crt-static` was
  not applied; build from the project root so `.cargo/config.toml` is picked up.
* **No datagrams, but the log says streaming:** check the port and the
  `AIGAKIRA_UDP_TARGET` value. Run `ss -ulpn | grep 45454` to confirm the
  receiver is bound. Flatpak Steam shares the host network, so loopback still
  works there.

## In-game overlay

The plugin draws small telemetry boxes directly into the game's frames, one per item.
It needs no separate program, works in fullscreen, and shows up in screenshots
and recordings.

| Game build | Renderer hooked | Notes |
|---|---|---|
| Windows build (ETS2/ATS under Proton, or on Windows) | Direct3D 11 (`IDXGISwapChain::Present`), DXVK or native | default renderer (`[gfx] Selected rendering device 'dx11'`) |
| Native Linux build | OpenGL / GLX (`glXSwapBuffers`) | default renderer on Linux (`[gl] …` in the log). Not available with the Vulkan renderer. |

### Turning it on and off

The overlay is on from the first start: the log says
`in-game overlay enabled (Direct3D 11)` and the boxes appear once you are in a
truck. To turn it off completely, so nothing is hooked into the game, set
`enabled = false` under `[overlay]` in `bin/win_x64/plugins/aigakira_telemetry.toml`
(native: `bin/linux_x64/plugins/`) and restart the game. To just hide the boxes
for a while, use **Hide all** in edit mode.

### Edit mode

Every telemetry item is its own **box**, placed anywhere on screen.

* Press **Ctrl+Shift+Backspace** in game to enter edit mode. Press it again
  (or click **Done**) to leave.
* All shown boxes stay visible as a live preview, even with no truck loaded,
  and get a blue outline. A **control panel** appears in the middle of the
  screen, on top of the boxes.
* The mouse now moves the overlay's own cursor, drawn on top of everything.
  The game ignores the mouse until you leave edit mode, so the camera and
  menus don't react and clicks can't fall through. This works while driving
  or in a menu. Then:
  * **drag any box** to move it; each box moves on its own;
  * in the control panel, **click an item** to show or hide its box. An
    unchecked box keeps its place, so checking it again brings it back
    exactly where it was. An item you show for the first time appears right
    next to the panel, ready to be dragged into place. Every telemetry field is available: speed, limit, cruise, gear,
    RPM, pedals, steering, fuel, range, consumption, blinkers, hazards,
    brakes, retarder, engine, electrics, wipers, diff lock, lights, warnings,
    game state and more;
  * **drag the control panel** by its title if it covers a box;
  * **Size −/+**, **Opacity −/+** and **No truck: hide/show** apply to all
    boxes. They don't change the control panel itself;
  * **Hide all / Show all** hides every box in game in one click, keeping
    which ones are checked and where they are. While everything is hidden,
    edit mode shows the boxes faded so you can still arrange them.
* Every change is saved to the settings file immediately.

How the mouse is taken: ETS2/ATS read the mouse through DirectInput and draw
their own cursor, so the Windows cursor position means nothing in game. The
overlay hooks DirectInput's `GetDeviceState`/`GetDeviceData` for mouse
devices. While editing, it uses the game's mouse movement and left button for
its cursor and hands the game "no movement, no buttons". Outside edit mode
the calls pass through unchanged. If the game is not reading a DirectInput
mouse, the overlay falls back to the Windows cursor.

*Native Linux build:* edit mode still follows the X11 pointer, and clicks
also reach the game. Use it from the pause menu.

### How it works

* **Windows build.** At startup the plugin creates a throw-away Direct3D 11
  device and swap chain on a hidden window, reads the swap chain's vtable
  (shared with the game's swap chain, e.g. DXVK's under Proton) and replaces
  its `Present`/`Present1` entries. Each frame it draws every box (plus, in
  edit mode, the control panel and cursor) as an alpha-blended quad from its
  own texture, re-uploaded only when that box changed, saving and
  restoring all pipeline state it touches. sRGB back buffers (ETS2 uses
  `B8G8R8A8_UNORM_SRGB`) get an sRGB texture, so colours are exact. Shaders are
  compiled with `d3dcompiler_47`, which is loaded at runtime (Proton/Wine ship
  it).
* **Native Linux build.** The game resolves `glXSwapBuffers` with `dlopen`
  before plugins load, so the plugin hot-patches the start of every
  `glXSwapBuffers` entry point (libGL's and libglvnd's libGLX) with a jump to
  its hook, then restores the original bytes around each real call. It draws
  with a GLSL 1.50 program and saves and restores the GL state it changes,
  including `glClipControl` and sRGB writes. Input comes from its own XCB
  connection, so X errors can never reach the game's Xlib error handler.
* **Both.** Each box is rendered on the CPU (tiny-skia + fontdue, with Noto
  Sans embedded under the SIL OFL, see [fonts/OFL.txt](fonts/OFL.txt)) and
  re-rendered only when its content changes. It scales with the screen
  height. Panics are contained, and on any renderer error the overlay turns
  itself off for the session and logs why. On shutdown the hook is removed
  and the plugin waits until no frame is inside it before the game unloads
  the library.
* **Off means off.** With `enabled = false` nothing is hooked.

### Testing the overlay

`harness/` is a fake game. It loads the real plugin through
`scs_telemetry_init`, feeds telemetry through the SDK callbacks, renders
frames and checks the pixels. It checks:

* first-start file creation, with the overlay on by default;
* the boxes being drawn at the right place, with
  exact background colours;
* game state being restored;
* the real keyboard path (Ctrl+Shift+Backspace) opening edit mode;
* on Windows, the fake game reads its mouse through DirectInput like ETS2. The
  harness checks that relative mouse movement steers the overlay cursor
  (drawn on top), that the boxes stay visible in edit mode, that dragging one
  box saves its new position while the others stay put, and that the game
  gets no mouse input while editing but does again afterwards;
* unhooking, and the game continuing to present after the plugin unloads.

```bash
# native Linux / OpenGL (renders off-screen into a pbuffer)
cargo build && cargo build -p overlay-harness
target/debug/glx_harness target/debug/libaigakira_telemetry_ets2_udp.so /tmp/overlay-out

# Windows / Direct3D 11 under Wine, with Proton's DXVK like the real game
cargo build --target x86_64-pc-windows-gnu && cargo build -p overlay-harness --target x86_64-pc-windows-gnu
mkdir -p /tmp/winrun/plugins
cp target/x86_64-pc-windows-gnu/debug/win_harness.exe /tmp/winrun/
cp target/x86_64-pc-windows-gnu/debug/aigakira_telemetry_ets2_udp.dll /tmp/winrun/plugins/
cp "$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/files/lib/wine/dxvk/x86_64-windows/"{d3d11,dxgi}.dll /tmp/winrun/
cd /tmp/winrun && WINEPREFIX=/tmp/winrun/prefix WINEDLLOVERRIDES="d3d11,dxgi=n,b" wine win_harness.exe 'plugins\aigakira_telemetry_ets2_udp.dll' out
```

Each prints `PASS`/`FAIL` lines and writes the captured frames as `.ppm`
images. The Windows harness opens a small window for a few seconds.

### Overlay troubleshooting

* **`in-game overlay not started: …`** in `game.log.txt`: the reason follows.
  On the native build, `the game has not loaded OpenGL` means the game runs on
  Vulkan. Switch it to OpenGL (`uset r_device "gl"` in `config.cfg`).
* **`overlay disabled for this session: …`**: a renderer call failed. The
  game keeps running normally; please report the message.
* **Nothing visible:** check `enabled = true`, that you are in a truck (or set
  `hide_when_idle = false`), and that `[overlay.boxes]` lists some items.
  Boxes are always kept inside the screen.
* **Edit mode does not open:** the game window must have keyboard focus. On the
  native build the hotkey is read from X11 key codes (standard
  evdev/libinput layout).
* **The overlay cursor feels too fast or too slow:** it moves by the raw
  mouse movement the game receives (your mouse DPI, plus Windows pointer
  acceleration under Wine), independently of the game's mouse sensitivity.
* **Conflicts:** other overlays that hook the same function (Steam overlay,
  MangoHud, OBS game capture) normally chain fine. If something misbehaves,
  set `enabled = false` to rule the overlay out. Don't use it with
  TruckersMP; its anti-cheat may object to hooks.
