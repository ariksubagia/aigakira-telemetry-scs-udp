//! Aigakira SCS telemetry: UDP JSON bridge and in-game overlay.
//!
//! Loaded by Euro Truck Simulator 2 (or ATS) from `bin/win_x64/plugins/`
//! (Windows build, also under Steam Proton) or `bin/linux_x64/plugins/`
//! (native Linux build). Truck telemetry is collected on the game thread into
//! a plain snapshot and streamed as compact JSON datagrams by a background
//! worker thread, so the SDK callbacks never block on I/O. Optionally an
//! overlay panel is drawn into the game's frames (see [`overlay`]).
//!
//! The SDK FFI, the two loader exports and per-entry-point panic containment
//! are provided by `scs-sdk-plugin`. Every module is `#![forbid(unsafe_code)]`
//! except the two overlay rendering backends (`overlay::d3d11`,
//! `overlay::glx`), which must hook the game's renderer.

mod config;
mod overlay;
mod plugin;
mod settings;
mod snapshot;
mod worker;

// Generates exactly `scs_telemetry_init` and `scs_telemetry_shutdown`
// (`extern "system"`, `#[no_mangle]`). Each delegates to the framework runtime,
// which wraps every call into this plugin in `catch_unwind`.
scs_sdk_plugin::export_plugin!(crate::plugin::AigakiraPlugin::default());
