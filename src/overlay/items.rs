#![forbid(unsafe_code)]
//! The telemetry items the overlay can show. Together they cover every field
//! of the `aigakira.scs.telemetry/1` datagram; in edit mode each one can be
//! shown or hidden.

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Normal,
    Dim,
    Good,
    Warn,
    Alert,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Show {
    Text(String, Tone),
    /// Gauge 0..=1 with a short text next to it.
    Bar { frac: f32, text: String, tone: Tone },
    /// Blinker arrows; `true` = lamp lit.
    Blinkers { left: bool, right: bool },
}

pub struct Item {
    pub id: &'static str,
    pub label: &'static str,
    pub default_on: bool,
    pub show: fn(&Value) -> Show,
}

fn get<'a>(p: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(p, |v, k| v.get(k)).filter(|v| !v.is_null())
}
fn f(p: &Value, path: &str) -> Option<f64> {
    get(p, path).and_then(Value::as_f64).filter(|x| x.is_finite())
}
fn b(p: &Value, path: &str) -> Option<bool> {
    get(p, path).and_then(Value::as_bool)
}
fn s<'a>(p: &'a Value, path: &str) -> Option<&'a str> {
    get(p, path).and_then(Value::as_str)
}

const NA: &str = "n/a";

fn text(t: impl Into<String>, tone: Tone) -> Show {
    Show::Text(t.into(), tone)
}

fn na() -> Show {
    text(NA, Tone::Dim)
}

fn on_off(p: &Value, path: &str, on_tone: Tone) -> Show {
    match b(p, path) {
        Some(true) => text("on", on_tone),
        Some(false) => text("off", Tone::Dim),
        None => na(),
    }
}

fn bar(frac: f64, text: String, tone: Tone) -> Show {
    Show::Bar { frac: frac.clamp(0.0, 1.0) as f32, text, tone }
}

fn pedal(p: &Value, path: &str) -> Show {
    match f(p, path) {
        Some(v) => bar(v, format!("{:.0}%", (v * 100.0).clamp(0.0, 100.0)), Tone::Normal),
        None => na(),
    }
}

fn steering(p: &Value, path: &str) -> Show {
    match f(p, path) {
        Some(v) => bar((v + 1.0) / 2.0, format!("{:+.2}", v), Tone::Normal),
        None => na(),
    }
}

fn speed(p: &Value) -> Show {
    let Some(kmh) = f(p, "drive.speed_kmh") else { return na() };
    // Red as soon as the speed shown is above the limit shown (whole km/h,
    // so 80.4 in an 80 zone, displayed as "80", stays white).
    let shown = kmh.abs().round();
    let over = f(p, "nav.limit_kmh").filter(|l| *l > 0.0).is_some_and(|l| shown > l.round());
    text(format!("{shown:.0} km/h"), if over { Tone::Alert } else { Tone::Normal })
}

fn limit(p: &Value) -> Show {
    match f(p, "nav.limit_kmh") {
        Some(l) if l > 0.0 => text(format!("{l:.0} km/h"), Tone::Normal),
        Some(_) => text("none", Tone::Dim),
        None => na(),
    }
}

fn cruise(p: &Value) -> Show {
    match (b(p, "cruise.on"), f(p, "cruise.set_kmh")) {
        (Some(true), Some(k)) => text(format!("{k:.0} km/h"), Tone::Good),
        (Some(_), _) => text("off", Tone::Dim),
        _ => na(),
    }
}

fn gear(p: &Value) -> Show {
    match s(p, "drive.gear_label") {
        Some(g) => text(g, if g == "N" { Tone::Dim } else { Tone::Normal }),
        None => na(),
    }
}

fn rpm(p: &Value) -> Show {
    let Some(rpm) = f(p, "drive.rpm") else { return na() };
    let max = f(p, "drive.rpm_max").unwrap_or(0.0);
    let frac = if max > 0.0 { rpm / max } else { 0.0 };
    let tone = if frac > 0.92 { Tone::Alert } else { Tone::Normal };
    bar(frac, format!("{rpm:.0}"), tone)
}

fn fuel(p: &Value) -> Show {
    let (Some(l), Some(pct)) = (f(p, "fuel.l"), f(p, "fuel.pct")) else { return na() };
    let tone = if b(p, "fuel.warn") == Some(true) { Tone::Alert } else { Tone::Normal };
    bar(pct / 100.0, format!("{l:.0} l"), tone)
}

fn range(p: &Value) -> Show {
    match f(p, "fuel.range_km") {
        Some(r) => text(format!("{r:.0} km"), Tone::Normal),
        None => na(),
    }
}

fn consumption(p: &Value) -> Show {
    match f(p, "fuel.avg_l100km") {
        Some(c) => text(format!("{c:.1} l/100km"), Tone::Normal),
        None => na(),
    }
}

fn blinkers(p: &Value) -> Show {
    // Prefer the lamp state (blinks); fall back to the switch on old schemas.
    let side = |lamp: &str, switch: &str| b(p, lamp).or_else(|| b(p, switch)).unwrap_or(false);
    Show::Blinkers { left: side("ind.lblinker_lamp", "ind.lblinker"), right: side("ind.rblinker_lamp", "ind.rblinker") }
}

fn hazard(p: &Value) -> Show {
    on_off(p, "ind.hazard", Tone::Warn)
}

fn park_brake(p: &Value) -> Show {
    match b(p, "ind.park_brake") {
        Some(true) => text("ON", Tone::Alert),
        Some(false) => text("off", Tone::Dim),
        None => na(),
    }
}

fn retarder(p: &Value) -> Show {
    let (Some(level), Some(steps)) = (f(p, "ind.retarder"), f(p, "ind.retarder_steps")) else { return na() };
    if steps <= 0.0 {
        return text("none", Tone::Dim);
    }
    let tone = if level > 0.0 { Tone::Good } else { Tone::Dim };
    bar(level / steps, format!("{level:.0}/{steps:.0}"), tone)
}

fn lights(p: &Value) -> Show {
    let on = |k: &str| b(p, k) == Some(true);
    let lit: Vec<&str> = [("lights.parking", "parking"), ("lights.low", "low"), ("lights.high", "high")]
        .into_iter()
        .filter(|(k, _)| on(k))
        .map(|(_, n)| n)
        .collect();
    if lit.is_empty() {
        text("off", Tone::Dim)
    } else {
        text(lit.join(" · "), if on("lights.high") { Tone::Good } else { Tone::Normal })
    }
}

fn aux(p: &Value) -> Show {
    let level = |k: &str| match f(p, k).unwrap_or(0.0) as i64 {
        0 => "off",
        1 => "dim",
        _ => "full",
    };
    let (front, roof) = (level("lights.aux_front"), level("lights.aux_roof"));
    let tone = if front == "off" && roof == "off" { Tone::Dim } else { Tone::Normal };
    text(format!("front {front} · roof {roof}"), tone)
}

fn rear_lights(p: &Value) -> Show {
    let mut on = Vec::new();
    if b(p, "lights.brake") == Some(true) {
        on.push("brake");
    }
    if b(p, "lights.reverse") == Some(true) {
        on.push("reverse");
    }
    if on.is_empty() { text("off", Tone::Dim) } else { text(on.join(" · "), Tone::Warn) }
}

fn warnings(p: &Value) -> Show {
    const W: &[(&str, &str)] = &[
        ("warn.air_emerg", "AIR EMERGENCY"),
        ("warn.air", "air"),
        ("warn.oil", "oil"),
        ("warn.water", "water temp"),
        ("warn.battery", "battery"),
        ("warn.adblue", "AdBlue"),
        ("fuel.warn", "fuel"),
    ];
    let active: Vec<&str> = W.iter().filter(|(k, _)| b(p, k) == Some(true)).map(|(_, n)| *n).collect();
    if active.is_empty() { text("none", Tone::Good) } else { text(active.join(", "), Tone::Alert) }
}

fn game_state(p: &Value) -> Show {
    if b(p, "alive") == Some(false) {
        return text("exited", Tone::Dim);
    }
    match (b(p, "paused"), b(p, "truck")) {
        (_, Some(false)) => text("menu", Tone::Dim),
        (Some(true), _) => text("paused", Tone::Warn),
        (Some(false), _) => text("driving", Tone::Good),
        _ => na(),
    }
}

fn game_info(p: &Value) -> Show {
    let game = s(p, "game").unwrap_or("?").to_uppercase();
    let ver = s(p, "game_ver").unwrap_or("?");
    let sid = get(p, "sid").map(|v| v.to_string()).unwrap_or_default();
    text(format!("{game} {ver} · sid {sid}"), Tone::Dim)
}

fn data_age(p: &Value) -> Show {
    match f(p, "age_ms") {
        Some(a) => text(format!("{a:.0} ms · #{}", get(p, "seq").map(|v| v.to_string()).unwrap_or_default()), if a > 1000.0 { Tone::Warn } else { Tone::Dim }),
        None => na(),
    }
}

macro_rules! item {
    ($id:literal, $label:literal, $on:literal, $f:expr) => {
        Item { id: $id, label: $label, default_on: $on, show: $f }
    };
}

pub const ITEMS: &[Item] = &[
    item!("speed", "Speed", true, speed),
    item!("limit", "Speed limit", true, limit),
    item!("cruise", "Cruise control", true, cruise),
    item!("gear", "Gear", true, gear),
    item!("rpm", "RPM", true, rpm),
    item!("speed_ms", "Speed (m/s)", false, |p| match f(p, "drive.speed_ms") {
        Some(v) => text(format!("{v:.2} m/s"), Tone::Normal),
        None => na(),
    }),
    item!("cruise_ms", "Cruise (m/s)", false, |p| match (b(p, "cruise.on"), f(p, "cruise.set_ms")) {
        (Some(true), Some(v)) => text(format!("{v:.2} m/s"), Tone::Good),
        (Some(_), _) => text("off", Tone::Dim),
        _ => na(),
    }),
    item!("gears", "Gearbox", false, |p| match (f(p, "drive.gear"), f(p, "drive.gears_fwd"), f(p, "drive.gears_rev")) {
        (Some(g), Some(fw), Some(rv)) => text(
            format!("engine {g:.0} · dash {} · {fw:.0}F/{rv:.0}R", get(p, "drive.dgear").map_or("n/a".into(), |v| v.to_string())),
            Tone::Normal,
        ),
        _ => na(),
    }),
    item!("throttle", "Throttle", false, |p| pedal(p, "effective.throttle")),
    item!("brake", "Brake", false, |p| pedal(p, "effective.brake")),
    item!("clutch", "Clutch", false, |p| pedal(p, "effective.clutch")),
    item!("steering", "Steering", false, |p| steering(p, "effective.steer")),
    item!("in_throttle", "Input throttle", false, |p| pedal(p, "input.throttle")),
    item!("in_brake", "Input brake", false, |p| pedal(p, "input.brake")),
    item!("in_clutch", "Input clutch", false, |p| pedal(p, "input.clutch")),
    item!("in_steering", "Input steering", false, |p| steering(p, "input.steer")),
    item!("fuel", "Fuel", true, fuel),
    item!("range", "Fuel range", false, range),
    item!("consumption", "Consumption", false, consumption),
    item!("fuel_cap", "Fuel tank", false, |p| match f(p, "fuel.cap_l") {
        Some(c) => text(format!("{c:.0} l"), Tone::Normal),
        None => na(),
    }),
    item!("blinkers", "Blinkers", true, blinkers),
    item!("blinker_switch", "Blinker switch", false, |p| {
        match (b(p, "ind.lblinker"), b(p, "ind.rblinker")) {
            (Some(true), Some(true)) => text("both", Tone::Warn),
            (Some(true), _) => text("left", Tone::Good),
            (_, Some(true)) => text("right", Tone::Good),
            (Some(false), Some(false)) => text("off", Tone::Dim),
            _ => na(),
        }
    }),
    item!("hazard", "Hazard lights", false, hazard),
    item!("park_brake", "Parking brake", true, park_brake),
    item!("engine_brake", "Engine brake", false, |p| on_off(p, "ind.engine_brake", Tone::Good)),
    item!("retarder", "Retarder", false, retarder),
    item!("engine", "Engine", false, |p| on_off(p, "ind.engine", Tone::Good)),
    item!("electric", "Electrics", false, |p| on_off(p, "ind.electric", Tone::Good)),
    item!("wipers", "Wipers", false, |p| on_off(p, "ind.wipers", Tone::Normal)),
    item!("diff_lock", "Differential lock", false, |p| on_off(p, "ind.diff_lock", Tone::Warn)),
    item!("lights", "Headlights", false, lights),
    item!("beacon", "Beacon", false, |p| on_off(p, "lights.beacon", Tone::Warn)),
    item!("aux", "Aux lights", false, aux),
    item!("rear_lights", "Brake/reverse lights", false, rear_lights),
    item!("warnings", "Warnings", true, warnings),
    item!("game_state", "Game state", false, game_state),
    item!("game_info", "Game", false, game_info),
    item!("data_age", "Data age", false, data_age),
];

pub fn index_of(id: &str) -> Option<usize> {
    ITEMS.iter().position(|i| i.id == id)
}

pub fn default_ids() -> Vec<String> {
    ITEMS.iter().filter(|i| i.default_on).map(|i| i.id.to_owned()).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn sample() -> Value {
        json!({"schema":"aigakira.scs.telemetry/1","seq":1834,"sid":2718281828u32,"ts":1763577600123u64,
         "age_ms":12,"alive":true,"game":"ets2","game_ver":"1.18","paused":false,"truck":true,
         "drive":{"speed_ms":22.21,"speed_kmh":87.0,"rpm":1150.0,"rpm_max":2500.0,"gear":10,"dgear":10,
                  "gear_label":"10","gears_fwd":12,"gears_rev":2},
         "input":{"steer":0.0,"throttle":0.0,"brake":0.0,"clutch":0.0},
         "effective":{"steer":-0.25,"throttle":0.41,"brake":0.0,"clutch":0.0},
         "fuel":{"l":512.3,"cap_l":800.0,"pct":64.0,"range_km":1630.0,"avg_l100km":31.4,"warn":false},
         "cruise":{"on":true,"set_ms":22.22,"set_kmh":80.0},
         "ind":{"lblinker":false,"rblinker":true,"hazard":false,"lblinker_lamp":false,"rblinker_lamp":true,
                "park_brake":false,"engine_brake":true,"retarder":1,"retarder_steps":4,
                "engine":true,"electric":true,"wipers":false,"diff_lock":null},
         "lights":{"parking":true,"low":true,"high":false,"beacon":false,"brake":true,"reverse":false,
                   "aux_front":0,"aux_roof":2},
         "warn":{"air":false,"air_emerg":false,"oil":true,"water":false,"battery":false,"adblue":null},
         "nav":{"limit_kmh":80.0}})
    }

    #[test]
    fn ids_unique_and_every_item_renders() {
        let p = sample();
        let empty = json!({});
        for (i, it) in ITEMS.iter().enumerate() {
            assert_eq!(index_of(it.id), Some(i), "duplicate id {}", it.id);
            (it.show)(&p);
            (it.show)(&empty); // must not panic on missing data
        }
    }

    #[test]
    fn every_wire_field_is_covered() {
        // Each leaf of the datagram must influence at least one item.
        fn leaves(prefix: &str, v: &Value, out: &mut Vec<String>) {
            if let Value::Object(m) = v {
                for (k, v) in m {
                    let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    leaves(&p, v, out);
                }
            } else {
                out.push(prefix.to_owned());
            }
        }
        let base = sample();
        let render = |p: &Value| ITEMS.iter().map(|i| (i.show)(p)).collect::<Vec<_>>();
        let before = render(&base);
        let mut all = Vec::new();
        leaves("", &base, &mut all);
        let transport = ["schema", "ts"]; // envelope only
        for leaf in all.iter().filter(|l| !transport.contains(&l.as_str())) {
            let mut p = base.clone();
            let mut slot = &mut p;
            for k in leaf.split('.') {
                slot = slot.get_mut(k).unwrap();
            }
            *slot = match slot {
                Value::Bool(b) => Value::Bool(!*b),
                Value::Number(n) => json!(if n.as_f64().unwrap() == 0.0 { 1.0 } else { 0.0 }),
                Value::String(_) => json!("X"),
                Value::Null => json!(true),
                _ => unreachable!(),
            };
            assert_ne!(before, render(&p), "wire field {leaf} is not shown by any item");
        }
    }

    #[test]
    fn speed_turns_red_above_the_limit() {
        let speed = |kmh: f64, limit: f64| {
            let mut p = sample();
            p["drive"]["speed_kmh"] = json!(kmh);
            p["nav"]["limit_kmh"] = json!(limit);
            (ITEMS[index_of("speed").unwrap()].show)(&p)
        };
        assert_eq!(speed(80.0, 80.0), Show::Text("80 km/h".into(), Tone::Normal), "at the limit");
        assert_eq!(speed(80.4, 80.0), Show::Text("80 km/h".into(), Tone::Normal), "shown as 80");
        assert_eq!(speed(80.6, 80.0), Show::Text("81 km/h".into(), Tone::Alert), "shown as 81 → red");
        assert_eq!(speed(81.0, 80.0), Show::Text("81 km/h".into(), Tone::Alert));
        assert_eq!(speed(-90.0, 80.0), Show::Text("90 km/h".into(), Tone::Alert), "reverse counts too");
        assert_eq!(speed(150.0, 0.0), Show::Text("150 km/h".into(), Tone::Normal), "no known limit");
        let mut p = sample();
        p["nav"]["limit_kmh"] = Value::Null; // game without navigation data
        assert_eq!((ITEMS[index_of("speed").unwrap()].show)(&p), Show::Text("87 km/h".into(), Tone::Normal));
    }

    #[test]
    fn formatting() {
        let p = sample();
        let show = |id: &str| (ITEMS[index_of(id).unwrap()].show)(&p);
        assert_eq!(show("speed"), Show::Text("87 km/h".into(), Tone::Alert)); // over the 80 limit
        assert_eq!(show("cruise"), Show::Text("80 km/h".into(), Tone::Good));
        assert_eq!(show("warnings"), Show::Text("oil".into(), Tone::Alert));
        assert_eq!(show("blinkers"), Show::Blinkers { left: false, right: true });
        assert_eq!(show("diff_lock"), Show::Text("n/a".into(), Tone::Dim));
        assert_eq!(show("aux"), Show::Text("front off · roof full".into(), Tone::Normal));
        match show("fuel") {
            Show::Bar { frac, text, .. } => {
                assert!((frac - 0.64).abs() < 1e-6);
                assert_eq!(text, "512 l");
            }
            other => panic!("{other:?}"),
        }
    }
}
