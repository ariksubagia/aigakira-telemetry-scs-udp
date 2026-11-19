#![forbid(unsafe_code)]
//! Plain-data telemetry snapshot (written on the game thread) and its compact
//! JSON wire encoding (performed on the UDP worker thread).
//!
//! Channel names, types and units, verified against `scs-sdk` 0.1.1
//! `channels::truck` and the raw `scs-sdk-sys` header constants:
//!
//! | field                 | SDK channel                          | type | unit / meaning              |
//! |-----------------------|--------------------------------------|------|-----------------------------|
//! | speed_ms              | truck.speed                          | f32  | m/s, negative = reverse     |
//! | engine_rpm            | truck.engine.rpm                     | f32  | rpm                         |
//! | engine_gear           | truck.engine.gear                    | i32  | >0 fwd, 0 N, <0 reverse     |
//! | displayed_gear        | truck.displayed.gear (ETS2 1.11+)    | i32  | dashboard gear              |
//! | input_*               | truck.input.{steering,throttle,brake,clutch}     | f32 | steer <-1;1>, others <0;1> |
//! | effective_*           | truck.effective.{steering,throttle,brake,clutch} | f32 | as used by simulation      |
//! | cruise_ms             | truck.cruise_control                 | f32  | m/s, 0 = cruise disabled    |
//! | parking_brake         | truck.brake.parking                  | bool |                             |
//! | motor_brake           | truck.brake.motor                    | bool | engine brake                |
//! | retarder_level        | truck.brake.retarder                 | u32  | 0..=retarder.steps          |
//! | fuel_l                | truck.fuel.amount                    | f32  | litres                      |
//! | fuel_warning          | truck.fuel.warning                   | bool |                             |
//! | fuel_avg_l_per_km     | truck.fuel.consumption.average       | f32  | l/km                        |
//! | fuel_range_km         | truck.fuel.range (ETS2 1.12+)        | f32  | km                          |
//! | lblinker / rblinker   | truck.lblinker / truck.rblinker      | bool | logical switch state        |
//! | hazard                | truck.hazard.warning (ETS2 1.17+)    | bool | logical switch state        |
//! | l/rblinker_lamp       | truck.light.lblinker / .rblinker (ETS2 1.04+) | bool | lamp currently lit |
//! | light_*               | truck.light.{parking,beam.low,beam.high,beacon,brake,reverse} | bool |  |
//! | light_aux_front/roof  | truck.light.aux.front / .roof        | u32  | 0 off, 1 dimmed, 2 full     |
//! | engine_on             | truck.engine.enabled                 | bool |                             |
//! | electric_on           | truck.electric.enabled               | bool |                             |
//! | wipers                | truck.wipers                         | bool |                             |
//! | diff_lock             | truck.differential_lock (ETS2 1.17+) | bool |                             |
//! | warn_air              | truck.brake.air.pressure.warning     | bool |                             |
//! | warn_air_emergency    | truck.brake.air.pressure.emergency (ETS2 1.01+) | bool |                  |
//! | warn_oil              | truck.oil.pressure.warning           | bool |                             |
//! | warn_water            | truck.water.temperature.warning      | bool |                             |
//! | warn_battery          | truck.battery.voltage.warning        | bool |                             |
//! | warn_adblue           | truck.adblue.warning (ETS2-only 1.12+) | bool |                           |
//! | nav_speed_limit_ms    | truck.navigation.speed.limit (ETS2 1.12+) | f32 | m/s, 0 = no limit known |
//!
//! Truck configuration attributes (configuration id `truck`):
//! `fuel.capacity` f32, `retarder.steps` u32, `gears.forward` u32,
//! `gears.reverse` u32, `rpm.limit` f32.

use serde::Serialize;

/// Wire schema identifier. Bump the trailing number on breaking changes.
pub const SCHEMA: &str = "aigakira.scs.telemetry/1";
/// Hard ceiling for one datagram (fits a 1500-byte Ethernet MTU).
pub const MAX_DATAGRAM: usize = 1400;

/// Which version-gated channels the running game schema provides.
/// Unavailable channels are emitted as JSON `null` rather than a fake value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub displayed_gear: bool,
    pub hazard: bool,
    pub blinker_lamps: bool,
    pub fuel_range: bool,
    pub adblue: bool,
    pub air_emergency: bool,
    pub diff_lock: bool,
    pub navigation: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub caps: Caps,
    /// SDK pause state. The SDK starts paused and sends `started` once the
    /// simulation runs; main menu, ESC menu and loading are all "paused".
    pub paused: bool,
    /// A non-empty `truck` configuration has been received (player has a truck).
    pub truck_present: bool,

    // truck configuration
    pub fuel_capacity_l: f32,
    pub retarder_steps: u32,
    pub forward_gears: u32,
    pub reverse_gears: u32,
    pub rpm_limit: f32,

    // driving
    pub speed_ms: f32,
    pub engine_rpm: f32,
    pub engine_gear: i32,
    pub displayed_gear: i32,
    pub input_steering: f32,
    pub input_throttle: f32,
    pub input_brake: f32,
    pub input_clutch: f32,
    pub effective_steering: f32,
    pub effective_throttle: f32,
    pub effective_brake: f32,
    pub effective_clutch: f32,

    // fuel
    pub fuel_l: f32,
    pub fuel_warning: bool,
    pub fuel_avg_l_per_km: f32,
    pub fuel_range_km: f32,

    // cruise / brakes
    pub cruise_ms: f32,
    pub parking_brake: bool,
    pub motor_brake: bool,
    pub retarder_level: u32,

    // indicators
    pub lblinker: bool,
    pub rblinker: bool,
    pub hazard: bool,
    pub lblinker_lamp: bool,
    pub rblinker_lamp: bool,
    pub engine_on: bool,
    pub electric_on: bool,
    pub wipers: bool,
    pub diff_lock: bool,

    // lights
    pub light_parking: bool,
    pub light_low_beam: bool,
    pub light_high_beam: bool,
    pub light_beacon: bool,
    pub light_brake: bool,
    pub light_reverse: bool,
    pub light_aux_front: u32,
    pub light_aux_roof: u32,

    // warnings
    pub warn_air: bool,
    pub warn_air_emergency: bool,
    pub warn_oil: bool,
    pub warn_water: bool,
    pub warn_battery: bool,
    pub warn_adblue: bool,

    // navigation
    pub nav_speed_limit_ms: f32,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new(Caps::default())
    }
}

impl Snapshot {
    pub const fn new(caps: Caps) -> Self {
        Self {
            caps,
            paused: true,
            truck_present: false,
            fuel_capacity_l: 0.0,
            retarder_steps: 0,
            forward_gears: 0,
            reverse_gears: 0,
            rpm_limit: 0.0,
            speed_ms: 0.0,
            engine_rpm: 0.0,
            engine_gear: 0,
            displayed_gear: 0,
            input_steering: 0.0,
            input_throttle: 0.0,
            input_brake: 0.0,
            input_clutch: 0.0,
            effective_steering: 0.0,
            effective_throttle: 0.0,
            effective_brake: 0.0,
            effective_clutch: 0.0,
            fuel_l: 0.0,
            fuel_warning: false,
            fuel_avg_l_per_km: 0.0,
            fuel_range_km: 0.0,
            cruise_ms: 0.0,
            parking_brake: false,
            motor_brake: false,
            retarder_level: 0,
            lblinker: false,
            rblinker: false,
            hazard: false,
            lblinker_lamp: false,
            rblinker_lamp: false,
            engine_on: false,
            electric_on: false,
            wipers: false,
            diff_lock: false,
            light_parking: false,
            light_low_beam: false,
            light_high_beam: false,
            light_beacon: false,
            light_brake: false,
            light_reverse: false,
            light_aux_front: 0,
            light_aux_roof: 0,
            warn_air: false,
            warn_air_emergency: false,
            warn_oil: false,
            warn_water: false,
            warn_battery: false,
            warn_adblue: false,
            nav_speed_limit_ms: 0.0,
        }
    }

    pub fn cruise_on(&self) -> bool {
        self.cruise_ms.is_finite() && self.cruise_ms > 0.01
    }

    /// The state a receiver should see. Without a truck (main menu, profile
    /// selection) the channel values are stale leftovers, so safe defaults are
    /// emitted instead. Internal state is kept intact because channels without
    /// `EACH_FRAME` are only re-sent by the SDK when they change.
    pub fn effective(&self) -> Self {
        if self.truck_present {
            *self
        } else {
            Self {
                paused: self.paused,
                ..Self::new(self.caps)
            }
        }
    }

    /// Low-cardinality state used to wake the UDP worker immediately on a
    /// discrete change (blinker toggle, cruise on/off, gear...), so button-like
    /// states reach the receiver without waiting for the next periodic tick.
    pub fn discrete(&self) -> Discrete {
        let flags = [
            self.paused,
            self.truck_present,
            self.cruise_on(),
            self.parking_brake,
            self.motor_brake,
            self.lblinker,
            self.rblinker,
            self.hazard,
            self.lblinker_lamp,
            self.rblinker_lamp,
            self.engine_on,
            self.electric_on,
            self.wipers,
            self.diff_lock,
            self.light_parking,
            self.light_low_beam,
            self.light_high_beam,
            self.light_beacon,
            self.light_brake,
            self.light_reverse,
            self.fuel_warning,
            self.warn_air,
            self.warn_air_emergency,
            self.warn_oil,
            self.warn_water,
            self.warn_battery,
            self.warn_adblue,
        ];
        let bits = flags
            .iter()
            .enumerate()
            .fold(0u64, |acc, (i, &on)| acc | (u64::from(on) << i));
        Discrete {
            bits,
            engine_gear: self.engine_gear,
            displayed_gear: self.displayed_gear,
            retarder_level: self.retarder_level,
            light_aux: (self.light_aux_front, self.light_aux_roof),
            cruise_kmh: kmh_whole(self.cruise_ms),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discrete {
    bits: u64,
    engine_gear: i32,
    displayed_gear: i32,
    retarder_level: u32,
    light_aux: (u32, u32),
    cruise_kmh: i32,
}

/// Per-session values that never change between datagrams.
#[derive(Clone, Debug)]
pub struct SessionMeta {
    /// Random-ish id that changes every plugin load, so receivers can tell a
    /// restart (seq reset) from packet loss.
    pub sid: u32,
    /// `ets2`, `ats` or `other`.
    pub game: &'static str,
    /// Game telemetry schema version, e.g. `1.18`.
    pub game_ver: String,
}

/// Per-datagram envelope values.
#[derive(Clone, Copy, Debug)]
pub struct Envelope {
    pub seq: u64,
    pub ts_ms: u64,
    pub age_ms: u64,
    pub alive: bool,
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Wire<'a> {
    schema: &'static str,
    seq: u64,
    sid: u32,
    /// Unix time in ms when the datagram was sent.
    ts: u64,
    /// ms since the game thread last published a frame. Grows during loading
    /// screens or if the game hangs; use it to detect stale data.
    age_ms: u64,
    /// `false` only in the final datagram sent on plugin shutdown.
    alive: bool,
    game: &'static str,
    game_ver: &'a str,
    paused: bool,
    truck: bool,
    drive: Drive,
    input: Controls,
    effective: Controls,
    fuel: Fuel,
    cruise: Cruise,
    ind: Indicators,
    lights: Lights,
    warn: Warnings,
    nav: Nav,
}

#[derive(Serialize)]
struct Drive {
    speed_ms: f32,
    speed_kmh: f32,
    rpm: f32,
    rpm_max: f32,
    gear: i32,
    dgear: Option<i32>,
    /// Dashboard-style label: `N`, `1`..`n`, `R1`..`Rn`.
    gear_label: String,
    gears_fwd: u32,
    gears_rev: u32,
}

#[derive(Serialize)]
struct Controls {
    steer: f32,
    throttle: f32,
    brake: f32,
    clutch: f32,
}

#[derive(Serialize)]
struct Fuel {
    l: f32,
    cap_l: f32,
    pct: f32,
    range_km: Option<f32>,
    avg_l100km: f32,
    warn: bool,
}

#[derive(Serialize)]
struct Cruise {
    on: bool,
    set_ms: f32,
    set_kmh: f32,
}

#[derive(Serialize)]
struct Indicators {
    lblinker: bool,
    rblinker: bool,
    hazard: Option<bool>,
    lblinker_lamp: Option<bool>,
    rblinker_lamp: Option<bool>,
    park_brake: bool,
    engine_brake: bool,
    retarder: u32,
    retarder_steps: u32,
    engine: bool,
    electric: bool,
    wipers: bool,
    diff_lock: Option<bool>,
}

#[derive(Serialize)]
struct Lights {
    parking: bool,
    low: bool,
    high: bool,
    beacon: bool,
    brake: bool,
    reverse: bool,
    aux_front: u32,
    aux_roof: u32,
}

#[derive(Serialize)]
struct Warnings {
    air: bool,
    air_emerg: Option<bool>,
    oil: bool,
    water: bool,
    battery: bool,
    adblue: Option<bool>,
}

#[derive(Serialize)]
struct Nav {
    limit_kmh: Option<f32>,
}

/// Sanitises and rounds a float for the wire: non-finite -> 0, magnitude
/// clamped to keep the text short, negative zero removed.
fn num(x: f32, decimals: i32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let scale = 10f32.powi(decimals);
    let v = (x.clamp(-1.0e6, 1.0e6) * scale).round() / scale;
    if v == 0.0 || !v.is_finite() { 0.0 } else { v }
}

fn kmh(ms: f32) -> f32 {
    num(ms * 3.6, 1)
}

fn kmh_whole(ms: f32) -> i32 {
    // Bounded by num()'s clamp, so the cast cannot overflow.
    num(ms * 3.6, 0) as i32
}

fn gear_label(gear: i32) -> String {
    match gear {
        0 => "N".to_owned(),
        g if g > 0 => g.to_string(),
        g => format!("R{}", g.unsigned_abs()),
    }
}

fn opt<T>(available: bool, value: T) -> Option<T> {
    available.then_some(value)
}

pub fn encode(snapshot: &Snapshot, meta: &SessionMeta, env: Envelope) -> Vec<u8> {
    let s = snapshot.effective();
    let c = s.caps;
    let shown_gear = if c.displayed_gear {
        s.displayed_gear
    } else {
        s.engine_gear
    };
    let fuel_pct = if s.fuel_capacity_l > 0.0 {
        num(s.fuel_l / s.fuel_capacity_l * 100.0, 1).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let cruise_on = s.cruise_on();

    let wire = Wire {
        schema: SCHEMA,
        seq: env.seq,
        sid: meta.sid,
        ts: env.ts_ms,
        age_ms: env.age_ms.min(3_600_000),
        alive: env.alive,
        game: meta.game,
        game_ver: &meta.game_ver,
        paused: s.paused,
        truck: s.truck_present,
        drive: Drive {
            speed_ms: num(s.speed_ms, 2),
            speed_kmh: kmh(s.speed_ms),
            rpm: num(s.engine_rpm, 0),
            rpm_max: num(s.rpm_limit, 0),
            gear: s.engine_gear,
            dgear: opt(c.displayed_gear, s.displayed_gear),
            gear_label: gear_label(shown_gear),
            gears_fwd: s.forward_gears,
            gears_rev: s.reverse_gears,
        },
        input: Controls {
            steer: num(s.input_steering, 3),
            throttle: num(s.input_throttle, 3),
            brake: num(s.input_brake, 3),
            clutch: num(s.input_clutch, 3),
        },
        effective: Controls {
            steer: num(s.effective_steering, 3),
            throttle: num(s.effective_throttle, 3),
            brake: num(s.effective_brake, 3),
            clutch: num(s.effective_clutch, 3),
        },
        fuel: Fuel {
            l: num(s.fuel_l, 1),
            cap_l: num(s.fuel_capacity_l, 0),
            pct: fuel_pct,
            range_km: opt(c.fuel_range, num(s.fuel_range_km, 0)),
            avg_l100km: num(s.fuel_avg_l_per_km * 100.0, 1),
            warn: s.fuel_warning,
        },
        cruise: Cruise {
            on: cruise_on,
            set_ms: if cruise_on { num(s.cruise_ms, 2) } else { 0.0 },
            set_kmh: if cruise_on { kmh(s.cruise_ms) } else { 0.0 },
        },
        ind: Indicators {
            lblinker: s.lblinker,
            rblinker: s.rblinker,
            hazard: opt(c.hazard, s.hazard),
            lblinker_lamp: opt(c.blinker_lamps, s.lblinker_lamp),
            rblinker_lamp: opt(c.blinker_lamps, s.rblinker_lamp),
            park_brake: s.parking_brake,
            engine_brake: s.motor_brake,
            retarder: s.retarder_level,
            retarder_steps: s.retarder_steps,
            engine: s.engine_on,
            electric: s.electric_on,
            wipers: s.wipers,
            diff_lock: opt(c.diff_lock, s.diff_lock),
        },
        lights: Lights {
            parking: s.light_parking,
            low: s.light_low_beam,
            high: s.light_high_beam,
            beacon: s.light_beacon,
            brake: s.light_brake,
            reverse: s.light_reverse,
            aux_front: s.light_aux_front,
            aux_roof: s.light_aux_roof,
        },
        warn: Warnings {
            air: s.warn_air,
            air_emerg: opt(c.air_emergency, s.warn_air_emergency),
            oil: s.warn_oil,
            water: s.warn_water,
            battery: s.warn_battery,
            adblue: opt(c.adblue, s.warn_adblue),
        },
        nav: Nav {
            limit_kmh: opt(c.navigation, kmh(s.nav_speed_limit_ms)),
        },
    };

    // Serialising plain structs of numbers/bools/short strings cannot fail;
    // fall back to a minimal valid object rather than panicking regardless.
    serde_json::to_vec(&wire).unwrap_or_else(|_| {
        format!(
            "{{\"schema\":\"{SCHEMA}\",\"seq\":{},\"sid\":{},\"error\":\"encode\"}}",
            env.seq, meta.sid
        )
        .into_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const ALL_CAPS: Caps = Caps {
        displayed_gear: true,
        hazard: true,
        blinker_lamps: true,
        fuel_range: true,
        adblue: true,
        air_emergency: true,
        diff_lock: true,
        navigation: true,
    };

    fn meta() -> SessionMeta {
        SessionMeta {
            sid: u32::MAX,
            game: "other",
            game_ver: "4294967295.4294967295".to_owned(),
        }
    }

    fn env() -> Envelope {
        Envelope {
            seq: u64::MAX,
            ts_ms: u64::MAX,
            age_ms: u64::MAX,
            alive: true,
        }
    }

    fn parse(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("valid JSON")
    }

    /// Every field at its longest textual form must still fit one datagram.
    #[test]
    fn worst_case_payload_fits_datagram() {
        let mut s = Snapshot::new(ALL_CAPS);
        s.truck_present = true;
        s.paused = false;
        let f = -987_654.3_f32;
        s.fuel_capacity_l = f;
        s.rpm_limit = f;
        s.speed_ms = f;
        s.engine_rpm = f;
        s.engine_gear = i32::MIN;
        s.displayed_gear = i32::MIN;
        s.input_steering = -0.123_456;
        s.input_throttle = -0.123_456;
        s.input_brake = -0.123_456;
        s.input_clutch = -0.123_456;
        s.effective_steering = -0.123_456;
        s.effective_throttle = -0.123_456;
        s.effective_brake = -0.123_456;
        s.effective_clutch = -0.123_456;
        s.fuel_l = f;
        s.fuel_avg_l_per_km = f;
        s.fuel_range_km = f;
        s.cruise_ms = 987_654.3;
        s.nav_speed_limit_ms = f;
        s.retarder_level = u32::MAX;
        s.retarder_steps = u32::MAX;
        s.forward_gears = u32::MAX;
        s.reverse_gears = u32::MAX;
        s.light_aux_front = u32::MAX;
        s.light_aux_roof = u32::MAX;
        // longest bool spelling everywhere
        s.fuel_warning = false;

        let bytes = encode(&s, &meta(), env());
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            bytes.len() < MAX_DATAGRAM,
            "payload is {} bytes: {text}",
            bytes.len()
        );
        let v = parse(&bytes);
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["drive"]["gear_label"], "R2147483648");
    }

    #[test]
    fn non_finite_values_encode_as_valid_json() {
        let mut s = Snapshot::new(ALL_CAPS);
        s.truck_present = true;
        s.speed_ms = f32::NAN;
        s.engine_rpm = f32::INFINITY;
        s.fuel_l = f32::NEG_INFINITY;
        s.fuel_capacity_l = 0.0; // division guard
        s.cruise_ms = f32::NAN;
        let v = parse(&encode(&s, &meta(), env()));
        assert_eq!(v["drive"]["speed_kmh"], 0.0);
        assert_eq!(v["drive"]["rpm"], 0.0);
        assert_eq!(v["fuel"]["pct"], 0.0);
        assert_eq!(v["cruise"]["on"], false);
    }

    #[test]
    fn menu_state_uses_safe_defaults_and_hides_stale_values() {
        let mut s = Snapshot::new(ALL_CAPS);
        s.speed_ms = 25.0;
        s.hazard = true;
        s.cruise_ms = 22.22;
        s.truck_present = false; // e.g. main menu
        let v = parse(&encode(&s, &meta(), env()));
        assert_eq!(v["paused"], true);
        assert_eq!(v["truck"], false);
        assert_eq!(v["drive"]["speed_kmh"], 0.0);
        assert_eq!(v["drive"]["gear_label"], "N");
        assert_eq!(v["ind"]["hazard"], false);
        assert_eq!(v["cruise"]["on"], false);
    }

    #[test]
    fn unavailable_channels_are_null() {
        let mut s = Snapshot::new(Caps::default());
        s.truck_present = true;
        s.engine_gear = -1;
        let v = parse(&encode(&s, &meta(), env()));
        assert!(v["ind"]["hazard"].is_null());
        assert!(v["drive"]["dgear"].is_null());
        assert!(v["nav"]["limit_kmh"].is_null());
        assert_eq!(v["drive"]["gear_label"], "R1");
    }

    #[test]
    fn cruise_and_indicators_normalise() {
        let mut s = Snapshot::new(ALL_CAPS);
        s.truck_present = true;
        s.paused = false;
        s.cruise_ms = 80.0 / 3.6;
        s.lblinker = true;
        s.hazard = true;
        s.retarder_level = 2;
        s.retarder_steps = 4;
        s.fuel_l = 300.0;
        s.fuel_capacity_l = 600.0;
        let v = parse(&encode(&s, &meta(), env()));
        assert_eq!(v["cruise"]["on"], true);
        assert_eq!(v["cruise"]["set_kmh"], 80.0);
        assert_eq!(v["ind"]["lblinker"], true);
        assert_eq!(v["ind"]["hazard"], true);
        assert_eq!(v["ind"]["retarder"], 2);
        assert_eq!(v["fuel"]["pct"], 50.0);
    }

    #[test]
    fn discrete_changes_on_toggles_only() {
        let mut s = Snapshot::new(ALL_CAPS);
        let before = s.discrete();
        s.speed_ms = 12.3; // continuous: no wake
        assert_eq!(before, s.discrete());
        s.hazard = true;
        assert_ne!(before, s.discrete());
    }
}
