#![forbid(unsafe_code)]
//! Background UDP sender.
//!
//! The game thread never blocks on this module:
//! * the latest snapshot is handed over with `Mutex::try_lock` (skipped, not
//!   waited for, in the rare case the worker is copying it at that instant);
//! * wake-ups go through a bounded channel of capacity 1 with `try_send`.
//!
//! The worker sends one datagram per tick (configurable rate) and an extra one
//! immediately whenever a discrete state (blinker, hazard, cruise, gear...)
//! changes.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::snapshot::{self, Envelope, SessionMeta, Snapshot};

/// How long `shutdown` waits for the worker before detaching it.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

struct Published {
    snapshot: Snapshot,
    at: Instant,
}

struct Shared {
    latest: Mutex<Published>,
    stop: AtomicBool,
    sent: AtomicU64,
    send_errors: AtomicU64,
    oversize: AtomicU64,
}

/// Game-thread handle to the running worker.
pub struct Link {
    shared: Arc<Shared>,
    wake: SyncSender<()>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    pub local_addrs: Vec<SocketAddr>,
}

pub struct ShutdownReport {
    pub sent: u64,
    pub send_errors: u64,
    pub oversize: u64,
    pub joined: bool,
}

pub fn start(config: &Config, meta: SessionMeta, initial: Snapshot) -> io::Result<Link> {
    // One socket per target, so an error on one receiver (e.g. an ICMP
    // port-unreachable reported back as WSAECONNRESET on Windows) can never
    // affect sends to the others.
    let mut sockets = Vec::with_capacity(config.targets.len());
    for &target in &config.targets {
        let bind: SocketAddr = if target.is_ipv4() {
            (Ipv4Addr::UNSPECIFIED, 0).into()
        } else {
            (Ipv6Addr::UNSPECIFIED, 0).into()
        };
        let socket = UdpSocket::bind(bind)?;
        // Defensive: a UDP send should never block, but never let it stall shutdown.
        socket.set_write_timeout(Some(Duration::from_millis(50)))?;
        sockets.push((socket, target));
    }
    let local_addrs = sockets.iter().filter_map(|(s, _)| s.local_addr().ok()).collect();

    let shared = Arc::new(Shared {
        latest: Mutex::new(Published {
            snapshot: initial,
            at: Instant::now(),
        }),
        stop: AtomicBool::new(false),
        sent: AtomicU64::new(0),
        send_errors: AtomicU64::new(0),
        oversize: AtomicU64::new(0),
    });
    let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);
    let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);

    let worker_shared = Arc::clone(&shared);
    let interval = config.interval();
    let thread = thread::Builder::new()
        .name("aigakira-udp".to_owned())
        .spawn(move || {
            // A panic here stays on this thread (it is not an FFI boundary),
            // but catch it anyway so `done` is always signalled.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&sockets, interval, &meta, &worker_shared, &wake_rx);
            }));
            let _ = done_tx.try_send(());
        })?;

    Ok(Link {
        shared,
        wake: wake_tx,
        done: done_rx,
        thread: Some(thread),
        local_addrs,
    })
}

fn lock(m: &Mutex<Published>) -> MutexGuard<'_, Published> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn run(
    sockets: &[(UdpSocket, SocketAddr)],
    interval: Duration,
    meta: &SessionMeta,
    shared: &Shared,
    wake: &Receiver<()>,
) {
    let mut seq: u64 = 0;
    loop {
        let woke = wake.recv_timeout(interval);
        let stopping =
            shared.stop.load(Ordering::Acquire) || matches!(woke, Err(RecvTimeoutError::Disconnected));

        // Copy out under the lock; encode and send without holding it.
        let (snapshot, at) = {
            let guard = lock(&shared.latest);
            (guard.snapshot, guard.at)
        };
        let env = Envelope {
            seq,
            ts_ms: unix_ms(),
            age_ms: u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX),
            alive: !stopping,
        };
        let payload = snapshot::encode(&snapshot, meta, env);
        if payload.len() > snapshot::MAX_DATAGRAM {
            shared.oversize.fetch_add(1, Ordering::Relaxed);
        }
        // Errors (e.g. ICMP port-unreachable surfacing as WSAECONNRESET when no
        // receiver is listening) are expected and only counted.
        for (socket, target) in sockets {
            match socket.send_to(&payload, target) {
                Ok(_) => shared.sent.fetch_add(1, Ordering::Relaxed),
                Err(_) => shared.send_errors.fetch_add(1, Ordering::Relaxed),
            };
        }
        seq = seq.wrapping_add(1);

        if stopping {
            break;
        }
    }
}

impl Link {
    /// Game thread: hand over the newest snapshot without ever blocking.
    pub fn publish(&self, snapshot: &Snapshot, wake: bool) {
        let mut guard = match self.shared.latest.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            // Worker is copying right now; the next frame will publish again.
            Err(TryLockError::WouldBlock) => return,
        };
        guard.snapshot = *snapshot;
        guard.at = Instant::now();
        drop(guard);

        if wake {
            // Full means a wake-up is already pending: nothing to do.
            match self.wake.try_send(()) {
                Ok(()) | Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => {}
            }
        }
    }

    /// Game thread: stop the worker, sending a final `alive:false` datagram.
    /// Waits at most `SHUTDOWN_GRACE`; the worker must be gone before the game
    /// unloads the DLL, so in the normal case it is joined.
    pub fn shutdown(mut self) -> ShutdownReport {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        let joined = match self.done.recv_timeout(SHUTDOWN_GRACE) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                self.thread.take().is_some_and(|t| t.join().is_ok())
            }
            Err(RecvTimeoutError::Timeout) => false,
        };
        ShutdownReport {
            sent: self.shared.sent.load(Ordering::Relaxed),
            send_errors: self.shared.send_errors.load(Ordering::Relaxed),
            oversize: self.shared.oversize.load(Ordering::Relaxed),
            joined,
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // Safety net if `shutdown` was not called: at least ask the thread to stop.
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Caps, SCHEMA};

    #[test]
    fn streams_wakes_and_stops_cleanly() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let target = rx.local_addr().unwrap().to_string();
        let config = Config::from_values(Some(target.as_str()), Some("1"));
        let meta = SessionMeta {
            sid: 7,
            game: "ets2",
            game_ver: "1.18".to_owned(),
        };
        let link = start(&config, meta, Snapshot::new(Caps::default())).unwrap();

        // 1 Hz periodic rate, so a packet within 2 s after a toggle proves the wake path.
        let mut s = Snapshot::new(Caps::default());
        s.truck_present = true;
        s.lblinker = true;
        link.publish(&s, true);

        let mut buf = [0u8; 2048];
        let mut saw_blinker = false;
        for _ in 0..3 {
            let n = rx.recv(&mut buf).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
            assert_eq!(v["schema"], SCHEMA);
            assert_eq!(v["sid"], 7);
            if v["ind"]["lblinker"] == true {
                saw_blinker = true;
                break;
            }
        }
        assert!(saw_blinker);

        let report = link.shutdown();
        assert!(report.joined);
        assert!(report.sent >= 2);
        // final datagram announces shutdown
        let mut last = None;
        while let Ok(n) = rx.recv(&mut buf) {
            last = Some(serde_json::from_slice::<serde_json::Value>(&buf[..n]).unwrap());
            if last.as_ref().is_some_and(|v| v["alive"] == false) {
                break;
            }
        }
        assert_eq!(last.unwrap()["alive"], false);
    }

    #[test]
    fn every_target_receives_every_datagram() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        // a port nobody listens on must not disturb the others
        let dead = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        for s in [&a, &b] {
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        }
        let list = format!("{},{dead},{}", a.local_addr().unwrap(), b.local_addr().unwrap());
        let config = Config::from_values(Some(&list), Some("50"));
        assert_eq!(config.targets.len(), 3);
        let meta = SessionMeta { sid: 9, game: "ets2", game_ver: "1.18".to_owned() };
        let link = start(&config, meta, Snapshot::new(Caps::default())).unwrap();
        assert_eq!(link.local_addrs.len(), 3);

        let mut buf = [0u8; 2048];
        let seqs = |rx: &UdpSocket, buf: &mut [u8]| -> Vec<u64> {
            (0..5)
                .map(|_| {
                    let n = rx.recv(buf).unwrap();
                    serde_json::from_slice::<serde_json::Value>(&buf[..n]).unwrap()["seq"].as_u64().unwrap()
                })
                .collect()
        };
        let sa = seqs(&a, &mut buf);
        let sb = seqs(&b, &mut buf);
        let report = link.shutdown();
        assert!(report.joined);
        assert_eq!(sa, sb, "both receivers see the same datagram sequence");
        assert_eq!(sa, vec![0, 1, 2, 3, 4]);
    }
}
