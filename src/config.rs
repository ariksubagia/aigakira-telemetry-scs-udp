#![forbid(unsafe_code)]
//! UDP configuration: `[udp]` in the settings file, overridable with
//! environment variables.
//!
//! Wine/Proton (and the native Linux game) pass the environment through, so
//! these can be set in the Steam launch options, e.g.
//! `AIGAKIRA_UDP_TARGET=127.0.0.1:45454,192.168.1.20:45454 AIGAKIRA_UDP_HZ=30 %command%`.

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

pub const ENV_TARGET: &str = "AIGAKIRA_UDP_TARGET";
pub const ENV_HZ: &str = "AIGAKIRA_UDP_HZ";

/// The Touch Portal plugin's port. Several comma-separated receivers are
/// allowed (a UDP port can only be bound by one program).
pub const DEFAULT_TARGET: &str = "127.0.0.1:45454";
pub const DEFAULT_HZ: u32 = 30;
pub const MAX_TARGETS: usize = 8;
const MIN_HZ: u32 = 1;
const MAX_HZ: u32 = 120;

#[derive(Debug, Clone)]
pub struct Config {
    /// Every datagram is sent to each of these (at least one, at most
    /// [`MAX_TARGETS`], no duplicates).
    pub targets: Vec<SocketAddr>,
    pub hz: u32,
    /// Problems found while parsing; logged to game.log.txt by the caller.
    pub warnings: Vec<String>,
}

impl Config {
    /// Settings-file values, each overridden by its environment variable
    /// when that is set.
    pub fn from_settings(udp: &crate::settings::UdpSettings) -> Self {
        let target = std::env::var(ENV_TARGET).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| udp.targets.join(","));
        let hz = std::env::var(ENV_HZ).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| udp.hz.to_string());
        Self::from_values(Some(&target), Some(&hz))
    }

    pub fn from_values(target: Option<&str>, hz: Option<&str>) -> Self {
        let mut warnings = Vec::new();

        let mut targets = match target.map(str::trim).filter(|s| !s.is_empty()) {
            None => Vec::new(),
            Some(raw) => parse_targets(raw, &mut warnings),
        };
        if targets.is_empty() {
            if target.is_some_and(|t| !t.trim().is_empty()) {
                warnings.push(format!("{ENV_TARGET}: no valid target; using {DEFAULT_TARGET}"));
            }
            targets = parse_targets(DEFAULT_TARGET, &mut Vec::new());
        }

        let hz = match hz.map(str::trim).filter(|s| !s.is_empty()) {
            None => DEFAULT_HZ,
            Some(raw) => match raw.parse::<u32>() {
                Ok(v) if (MIN_HZ..=MAX_HZ).contains(&v) => v,
                Ok(v) => {
                    let clamped = v.clamp(MIN_HZ, MAX_HZ);
                    warnings.push(format!(
                        "{ENV_HZ}={v} out of range {MIN_HZ}..={MAX_HZ}; using {clamped}"
                    ));
                    clamped
                }
                Err(_) => {
                    warnings.push(format!(
                        "{ENV_HZ}={raw:?} is not an integer; using {DEFAULT_HZ}"
                    ));
                    DEFAULT_HZ
                }
            },
        };

        Self { targets, hz, warnings }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(self.hz.max(1)))
    }

    /// `udp://a, udp://b` for the log.
    pub fn targets_text(&self) -> String {
        self.targets
            .iter()
            .map(|t| format!("udp://{t}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn parse_targets(raw: &str, warnings: &mut Vec<String>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match parse_target(part) {
            Some(addr) if out.contains(&addr) => {}
            Some(_) if out.len() >= MAX_TARGETS => {
                warnings.push(format!("{ENV_TARGET}: more than {MAX_TARGETS} targets; ignoring {part:?}"));
            }
            Some(addr) => out.push(addr),
            None => warnings.push(format!("{ENV_TARGET}: {part:?} is not a valid host:port; ignored")),
        }
    }
    out
}

/// Accepts `ip:port`, `[ipv6]:port` or `hostname:port`. Hostnames are resolved
/// once at plugin initialisation (localhost resolves without network I/O).
fn parse_target(raw: &str) -> Option<SocketAddr> {
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Some(addr);
    }
    raw.to_socket_addrs().ok()?.next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    #[test]
    fn defaults() {
        let c = Config::from_values(None, None);
        assert_eq!(c.targets, addrs(&["127.0.0.1:45454"]));
        assert_eq!(c.hz, 30);
        assert!(c.warnings.is_empty());
        let from_file = Config::from_settings(&crate::settings::UdpSettings::default());
        assert_eq!(from_file.targets, c.targets);
    }

    #[test]
    fn explicit_values() {
        let c = Config::from_values(Some("192.168.1.20:9000"), Some("60"));
        assert_eq!(c.targets, addrs(&["192.168.1.20:9000"]));
        assert_eq!(c.hz, 60);
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn target_lists() {
        let c = Config::from_values(Some(" 127.0.0.1:1 , [::1]:2,127.0.0.1:1,"), None);
        assert_eq!(c.targets, addrs(&["127.0.0.1:1", "[::1]:2"]));
        assert!(c.warnings.is_empty());

        // one bad entry is skipped, the good ones are kept
        let c = Config::from_values(Some("127.0.0.1:1,nonsense"), None);
        assert_eq!(c.targets, addrs(&["127.0.0.1:1"]));
        assert_eq!(c.warnings.len(), 1);

        let many = (1..=10).map(|p| format!("127.0.0.1:{p}")).collect::<Vec<_>>().join(",");
        let c = Config::from_values(Some(&many), None);
        assert_eq!(c.targets.len(), MAX_TARGETS);
        assert_eq!(c.warnings.len(), 2);
    }

    #[test]
    fn bad_values_fall_back_with_warnings() {
        let c = Config::from_values(Some("nonsense"), Some("abc"));
        assert_eq!(c.targets, addrs(&["127.0.0.1:45454"]));
        assert_eq!(c.hz, DEFAULT_HZ);
        assert_eq!(c.warnings.len(), 3); // bad entry, "no valid target", bad hz

        let c = Config::from_values(None, Some("5000"));
        assert_eq!(c.hz, MAX_HZ);
        assert_eq!(c.warnings.len(), 1);
    }
}
