#![forbid(unsafe_code)]
//! SCS telemetry plugin: subscriptions, channel routing and lifecycle.
//!
//! Every hook here runs on the game main thread inside the framework's
//! `catch_unwind`. Hooks only copy values into a plain `Snapshot` and hand it
//! to the worker with non-blocking primitives; no I/O, no allocation on the
//! per-channel path.

use std::time::{SystemTime, UNIX_EPOCH};

use scs_sdk_plugin::sdk::channels::truck as t;
use scs_sdk_plugin::sdk::configuration::{attributes as attr, ids};
use scs_sdk_plugin::sdk::{TelemetryApiVersion, game};
use scs_sdk_plugin::{
    ChannelUpdate, Game, GameCompatibility, PluginCompatibility, PluginContext, PluginMetadata,
    PluginResult, TelemetryEvent, TelemetryEventKind, TelemetryPlugin,
};

use crate::config::Config;
use crate::overlay::Overlay;
use crate::settings;
use crate::snapshot::{Caps, Discrete, SessionMeta, Snapshot};
use crate::worker::{self, Link};

const LOG_PREFIX: &str = "[aigakira-udp]";

// Minimum schema 1.00 for both games: every channel newer than that is
// subscribed as optional and reported as `null` when the game lacks it.
static SUPPORTED_GAMES: [GameCompatibility; 2] = [
    GameCompatibility::new(Game::EuroTruckSimulator2, game::ets2::V1_00),
    GameCompatibility::new(Game::AmericanTruckSimulator, game::ats::V1_00),
];

#[derive(Default)]
pub struct AigakiraPlugin {
    snap: Snapshot,
    last_discrete: Option<Discrete>,
    link: Option<Link>,
    overlay: Option<Overlay>,
}

impl AigakiraPlugin {
    fn publish(&mut self) {
        if let Some(o) = &self.overlay {
            o.feed(&self.snap);
        }
        let Some(link) = &self.link else { return };
        let discrete = self.snap.discrete();
        let changed = self.last_discrete != Some(discrete);
        self.last_discrete = Some(discrete);
        link.publish(&self.snap, changed);
    }

    /// Render-thread messages go to game.log.txt from the game thread.
    fn flush_overlay_logs(&self, ctx: &mut PluginContext<'_>) {
        if let Some(o) = &self.overlay {
            for line in o.drain_logs() {
                ctx.message(format_args!("{LOG_PREFIX} {line}"));
            }
        }
    }
}

fn session_id() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() ^ (d.as_secs() as u32));
    nanos ^ std::process::id().rotate_left(16)
}

impl TelemetryPlugin for AigakiraPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new("aigakira-telemetry-udp", env!("CARGO_PKG_VERSION"))
    }

    fn compatibility(&self) -> PluginCompatibility {
        PluginCompatibility::new(TelemetryApiVersion::V1_00, &SUPPORTED_GAMES)
    }

    fn initialize(&mut self, ctx: &mut PluginContext<'_>) -> PluginResult {
        let info = ctx.game();
        let caps = Caps {
            displayed_gear: info.supports(t::DISPLAYED_GEAR.availability()),
            hazard: info.supports(t::HAZARD_WARNING.availability()),
            blinker_lamps: info.supports(t::LIGHT_LBLINKER.availability()),
            fuel_range: info.supports(t::FUEL_RANGE.availability()),
            adblue: info.supports(t::ADBLUE_WARNING.availability()),
            air_emergency: info.supports(t::BRAKE_AIR_PRESSURE_EMERGENCY.availability()),
            diff_lock: info.supports(t::DIFFERENTIAL_LOCK.availability()),
            navigation: info.supports(t::NAVIGATION_SPEED_LIMIT.availability()),
        };
        let meta = SessionMeta {
            sid: session_id(),
            game: match info.kind() {
                Game::EuroTruckSimulator2 => "ets2",
                Game::AmericanTruckSimulator => "ats",
                Game::Other => "other",
            },
            game_ver: info.schema_version().to_string(),
        };
        let game_name = info.name().to_owned();
        self.snap = Snapshot::new(caps);
        self.last_discrete = None;

        // Events. FrameEnd = all channel callbacks of this frame delivered.
        ctx.subscribe_event(TelemetryEventKind::FrameEnd)?;
        ctx.subscribe_event(TelemetryEventKind::Paused)?;
        ctx.subscribe_event(TelemetryEventKind::Started)?;
        ctx.subscribe_event(TelemetryEventKind::Configuration)?;

        // Channels present since schema 1.00 in both games: required.
        ctx.subscribe(t::SPEED)?;
        ctx.subscribe(t::ENGINE_RPM)?;
        ctx.subscribe(t::ENGINE_GEAR)?;
        ctx.subscribe(t::INPUT_STEERING)?;
        ctx.subscribe(t::INPUT_THROTTLE)?;
        ctx.subscribe(t::INPUT_BRAKE)?;
        ctx.subscribe(t::INPUT_CLUTCH)?;
        ctx.subscribe(t::EFFECTIVE_STEERING)?;
        ctx.subscribe(t::EFFECTIVE_THROTTLE)?;
        ctx.subscribe(t::EFFECTIVE_BRAKE)?;
        ctx.subscribe(t::EFFECTIVE_CLUTCH)?;
        ctx.subscribe(t::CRUISE_CONTROL)?;
        ctx.subscribe(t::PARKING_BRAKE)?;
        ctx.subscribe(t::MOTOR_BRAKE)?;
        ctx.subscribe(t::RETARDER_LEVEL)?;
        ctx.subscribe(t::FUEL)?;
        ctx.subscribe(t::FUEL_WARNING)?;
        ctx.subscribe(t::FUEL_AVERAGE_CONSUMPTION)?;
        ctx.subscribe(t::LBLINKER)?;
        ctx.subscribe(t::RBLINKER)?;
        ctx.subscribe(t::LIGHT_PARKING)?;
        ctx.subscribe(t::LIGHT_LOW_BEAM)?;
        ctx.subscribe(t::LIGHT_HIGH_BEAM)?;
        ctx.subscribe(t::LIGHT_AUX_FRONT)?;
        ctx.subscribe(t::LIGHT_AUX_ROOF)?;
        ctx.subscribe(t::LIGHT_BEACON)?;
        ctx.subscribe(t::LIGHT_BRAKE)?;
        ctx.subscribe(t::LIGHT_REVERSE)?;
        ctx.subscribe(t::ENGINE_ENABLED)?;
        ctx.subscribe(t::ELECTRIC_ENABLED)?;
        ctx.subscribe(t::WIPERS)?;
        ctx.subscribe(t::BRAKE_AIR_PRESSURE_WARNING)?;
        ctx.subscribe(t::OIL_PRESSURE_WARNING)?;
        ctx.subscribe(t::WATER_TEMPERATURE_WARNING)?;
        ctx.subscribe(t::BATTERY_VOLTAGE_WARNING)?;

        // Version-gated channels: optional (skipped on older schemas / ATS).
        ctx.subscribe_optional(t::DISPLAYED_GEAR)?; // ETS2 1.11
        ctx.subscribe_optional(t::HAZARD_WARNING)?; // ETS2 1.17
        ctx.subscribe_optional(t::LIGHT_LBLINKER)?; // ETS2 1.04
        ctx.subscribe_optional(t::LIGHT_RBLINKER)?; // ETS2 1.04
        ctx.subscribe_optional(t::FUEL_RANGE)?; // ETS2 1.12
        ctx.subscribe_optional(t::ADBLUE_WARNING)?; // ETS2-only 1.12
        ctx.subscribe_optional(t::BRAKE_AIR_PRESSURE_EMERGENCY)?; // ETS2 1.01
        ctx.subscribe_optional(t::DIFFERENTIAL_LOCK)?; // ETS2 1.17
        ctx.subscribe_optional(t::NAVIGATION_SPEED_LIMIT)?; // ETS2 1.12

        // Settings file next to the plugin; created with defaults on the
        // first start (overlay off).
        let loaded = settings::load_or_create(settings::default_path());
        for w in &loaded.warnings {
            ctx.warning(format_args!("{LOG_PREFIX} {w}"));
        }
        let path_text = loaded.path.as_ref().map_or_else(|| "(none)".to_owned(), |p| p.display().to_string());
        if loaded.created {
            ctx.message(format_args!(
                "{LOG_PREFIX} created settings file {path_text}; the in-game overlay is on (set [overlay] enabled = false and restart the game to turn it off)"
            ));
        } else {
            ctx.message(format_args!("{LOG_PREFIX} settings: {path_text}"));
        }
        let settings = loaded.settings;

        // In-game overlay: only hooks the renderer when enabled.
        if settings.overlay.enabled {
            match Overlay::start(settings.overlay.clone(), loaded.path.clone(), meta.clone()) {
                Ok(o) => {
                    ctx.message(format_args!(
                        "{LOG_PREFIX} in-game overlay enabled ({}); Ctrl+Shift+Backspace toggles edit mode",
                        o.backend_name()
                    ));
                    self.overlay = Some(o);
                }
                Err(e) => ctx.warning(format_args!("{LOG_PREFIX} in-game overlay not started: {e}")),
            }
        } else {
            ctx.message(format_args!("{LOG_PREFIX} in-game overlay disabled in settings"));
        }

        if !settings.udp.enabled {
            ctx.message(format_args!("{LOG_PREFIX} UDP streaming disabled in settings"));
            return Ok(());
        }
        // Never fail game start because of networking: log and run without UDP.
        let config = Config::from_settings(&settings.udp);
        for w in &config.warnings {
            ctx.warning(format_args!("{LOG_PREFIX} {w}"));
        }
        let game_ver = meta.game_ver.clone();
        match worker::start(&config, meta, self.snap) {
            Ok(link) => {
                ctx.message(format_args!(
                    "{LOG_PREFIX} v{} streaming JSON to {} at {} Hz (game: {game_name}, schema {game_ver}, local ports {:?})",
                    env!("CARGO_PKG_VERSION"),
                    config.targets_text(),
                    config.hz,
                    link.local_addrs.iter().map(|a| a.port()).collect::<Vec<_>>(),
                ));
                ctx.message(format_args!("{LOG_PREFIX} capabilities: {caps:?}"));
                self.link = Some(link);
            }
            Err(e) => {
                ctx.error(format_args!(
                    "{LOG_PREFIX} could not start UDP worker ({e}); telemetry streaming disabled"
                ));
            }
        }
        Ok(())
    }

    fn channel(&mut self, _ctx: &mut PluginContext<'_>, update: ChannelUpdate<'_>) {
        let s = &mut self.snap;
        // Each arm decodes with the channel's own typed descriptor, so a field
        // whose Rust type differs from the SDK type fails to compile.
        macro_rules! route {
            ($($channel:expr => $field:ident),* $(,)?) => {
                $(
                    if update.is($channel) {
                        if let Some(v) = update.value($channel) {
                            s.$field = v;
                        }
                        return;
                    }
                )*
            };
        }
        route! {
            t::SPEED => speed_ms,
            t::ENGINE_RPM => engine_rpm,
            t::ENGINE_GEAR => engine_gear,
            t::DISPLAYED_GEAR => displayed_gear,
            t::INPUT_STEERING => input_steering,
            t::INPUT_THROTTLE => input_throttle,
            t::INPUT_BRAKE => input_brake,
            t::INPUT_CLUTCH => input_clutch,
            t::EFFECTIVE_STEERING => effective_steering,
            t::EFFECTIVE_THROTTLE => effective_throttle,
            t::EFFECTIVE_BRAKE => effective_brake,
            t::EFFECTIVE_CLUTCH => effective_clutch,
            t::CRUISE_CONTROL => cruise_ms,
            t::PARKING_BRAKE => parking_brake,
            t::MOTOR_BRAKE => motor_brake,
            t::RETARDER_LEVEL => retarder_level,
            t::FUEL => fuel_l,
            t::FUEL_WARNING => fuel_warning,
            t::FUEL_AVERAGE_CONSUMPTION => fuel_avg_l_per_km,
            t::FUEL_RANGE => fuel_range_km,
            t::LBLINKER => lblinker,
            t::RBLINKER => rblinker,
            t::HAZARD_WARNING => hazard,
            t::LIGHT_LBLINKER => lblinker_lamp,
            t::LIGHT_RBLINKER => rblinker_lamp,
            t::LIGHT_PARKING => light_parking,
            t::LIGHT_LOW_BEAM => light_low_beam,
            t::LIGHT_HIGH_BEAM => light_high_beam,
            t::LIGHT_AUX_FRONT => light_aux_front,
            t::LIGHT_AUX_ROOF => light_aux_roof,
            t::LIGHT_BEACON => light_beacon,
            t::LIGHT_BRAKE => light_brake,
            t::LIGHT_REVERSE => light_reverse,
            t::ENGINE_ENABLED => engine_on,
            t::ELECTRIC_ENABLED => electric_on,
            t::WIPERS => wipers,
            t::DIFFERENTIAL_LOCK => diff_lock,
            t::BRAKE_AIR_PRESSURE_WARNING => warn_air,
            t::BRAKE_AIR_PRESSURE_EMERGENCY => warn_air_emergency,
            t::OIL_PRESSURE_WARNING => warn_oil,
            t::WATER_TEMPERATURE_WARNING => warn_water,
            t::BATTERY_VOLTAGE_WARNING => warn_battery,
            t::ADBLUE_WARNING => warn_adblue,
            t::NAVIGATION_SPEED_LIMIT => nav_speed_limit_ms,
        }
    }

    fn event(&mut self, ctx: &mut PluginContext<'_>, event: TelemetryEvent<'_>) {
        match event {
            TelemetryEvent::FrameEnd => {
                self.publish();
                self.flush_overlay_logs(ctx);
            }
            TelemetryEvent::Paused => {
                self.snap.paused = true;
                self.publish();
            }
            TelemetryEvent::Started => {
                self.snap.paused = false;
                self.publish();
            }
            TelemetryEvent::Configuration(cfg) if cfg.is(ids::TRUCK) => {
                // An empty truck configuration means "no truck" (menu, profile
                // switch). Values then go to safe defaults on the wire.
                if cfg.has_attributes() {
                    let s = &mut self.snap;
                    s.truck_present = true;
                    s.fuel_capacity_l = cfg.get(attr::FUEL_CAPACITY).unwrap_or(0.0);
                    s.retarder_steps = cfg.get(attr::RETARDER_STEP_COUNT).unwrap_or(0);
                    s.forward_gears = cfg.get(attr::FORWARD_GEAR_COUNT).unwrap_or(0);
                    s.reverse_gears = cfg.get(attr::REVERSE_GEAR_COUNT).unwrap_or(0);
                    s.rpm_limit = cfg.get(attr::RPM_LIMIT).unwrap_or(0.0);
                } else {
                    self.snap.truck_present = false;
                }
                self.publish();
            }
            _ => {}
        }
    }

    fn shutdown(&mut self, ctx: &mut PluginContext<'_>) {
        // Unhook first: the DLL/.so is unloaded right after this returns.
        if let Some(o) = self.overlay.take() {
            for line in o.shutdown() {
                ctx.message(format_args!("{LOG_PREFIX} {line}"));
            }
            ctx.message(format_args!("{LOG_PREFIX} in-game overlay removed"));
        }
        if let Some(link) = self.link.take() {
            let r = link.shutdown();
            ctx.message(format_args!(
                "{LOG_PREFIX} stopped: {} datagrams sent, {} send errors, {} oversize, worker joined: {}",
                r.sent, r.send_errors, r.oversize, r.joined
            ));
        }
    }
}
