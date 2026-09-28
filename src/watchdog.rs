//! Watchdog thread, independent of the tokio runtime.
//!
//! Grabbing the pointer and keyboard can lock the user out of their session, so any grab must
//! end even if the event loop wedges. The event loop bumps a heartbeat every 250 ms and the
//! Wayland layer reports whether it currently holds a grab (lock, inhibitor or exclusive
//! keyboard focus).
//!
//! - Stage 1: the heartbeat is stale for more than [`STALE_MS`] while a grab is held, or the
//!   grab exceeded the hard limit. We `shutdown()` the Wayland socket: niri then drops the lock,
//!   the keyboard focus and the inhibitor at once, while the process (and, in portal mode, its
//!   D-Bus and EIS connections) stays alive.
//! - Stage 2: still stale (or still grabbing) some time after stage 1, or stale for
//!   [`EXIT_STALE_MS`] regardless: exit the process. Process death also frees every grab.
//!
//! The thread never writes to stdout/stderr: if the event loop is wedged because a log pipe is
//! full, a write here would block too and the safety action would never run. It acts first and
//! then appends a line to `$XDG_RUNTIME_DIR/layercapture/watchdog.log` (tmpfs, never blocks).

use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STALE_MS: u64 = 3_000;
const EXIT_STALE_MS: u64 = 30_000;
const POLL: Duration = Duration::from_millis(100);

pub struct Watchdog {
    start: Instant,
    /// Milliseconds since `start` (+1, so 0 means "never") of the last heartbeat.
    heartbeat: AtomicU64,
    /// Milliseconds since `start` (+1) when the current grab began; 0 = no grab held.
    grab_since: AtomicU64,
    /// When stage 1 fired (+1); 0 = not fired.
    stage1_at: AtomicU64,
    /// Maximum grab duration in ms; 0 = unlimited.
    hard_limit_ms: u64,
    /// Delay between stage 1 and stage 2.
    stage2_after_ms: u64,
    /// A duplicate of the Wayland socket, so stage 1 can shut it down from this thread. A dup
    /// refers to the same socket, and owning it means the number can never be reused for an
    /// unrelated file while we hold it.
    wayland_fd: Mutex<Option<OwnedFd>>,
    log_path: Option<PathBuf>,
}

impl Watchdog {
    pub fn start(hard_limit: Option<Duration>, stage2_after: Duration) -> Arc<Self> {
        let wd = Arc::new(Self {
            start: Instant::now(),
            heartbeat: AtomicU64::new(1),
            grab_since: AtomicU64::new(0),
            stage1_at: AtomicU64::new(0),
            hard_limit_ms: hard_limit.map_or(0, |d| d.as_millis() as u64),
            stage2_after_ms: stage2_after.as_millis() as u64,
            wayland_fd: Mutex::new(None),
            log_path: if cfg!(test) {
                None
            } else {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|d| PathBuf::from(d).join("layercapture").join("watchdog.log"))
            },
        });
        let thread_wd = wd.clone();
        std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || thread_wd.run())
            .expect("spawning watchdog thread");
        wd
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + 1
    }

    pub fn beat(&self) {
        self.heartbeat.store(self.now(), Ordering::Relaxed);
    }

    pub fn set_grab_held(&self, held: bool) {
        if held {
            // Keep the original start if a grab is already held.
            let _ = self.grab_since.compare_exchange(
                0,
                self.now(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        } else {
            self.grab_since.store(0, Ordering::Relaxed);
        }
    }

    /// True once stage 1 has fired (for the event loop to report when it wakes up).
    pub fn stage1_fired(&self) -> bool {
        self.stage1_at.load(Ordering::Relaxed) != 0
    }

    pub fn set_wayland_fd(&self, fd: Option<OwnedFd>) {
        *self.wayland_fd.lock().unwrap_or_else(|e| e.into_inner()) = fd;
    }

    /// Best-effort record of what the watchdog did; called only after the action.
    fn record(&self, msg: &str) {
        if let Some(path) = &self.log_path
            && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = writeln!(f, "{:?} pid {}: {msg}", std::time::SystemTime::now(), std::process::id());
        }
    }

    fn run(&self) {
        loop {
            std::thread::sleep(POLL);
            let now = self.now();
            let stale = now.saturating_sub(self.heartbeat.load(Ordering::Relaxed));
            let grab_since = self.grab_since.load(Ordering::Relaxed);
            let grab_age = if grab_since == 0 { 0 } else { now.saturating_sub(grab_since) };
            let over_limit = grab_since != 0 && self.hard_limit_ms != 0 && grab_age > self.hard_limit_ms;
            let stage1_at = self.stage1_at.load(Ordering::Relaxed);

            if stage1_at == 0 && grab_since != 0 && (stale > STALE_MS || over_limit) {
                if let Some(fd) = &*self.wayland_fd.lock().unwrap_or_else(|e| e.into_inner()) {
                    let _ = rustix::net::shutdown(fd, rustix::net::Shutdown::Both);
                }
                self.stage1_at.store(now, Ordering::Relaxed);
                self.record(&format!(
                    "stage 1: heartbeat stale {stale} ms, grab held {grab_age} ms; shut down the Wayland connection"
                ));
                continue;
            }

            if stage1_at != 0 {
                let since = now.saturating_sub(stage1_at);
                let recovered = grab_since == 0 && stale <= STALE_MS;
                if recovered {
                    self.stage1_at.store(0, Ordering::Relaxed);
                } else if since > self.stage2_after_ms {
                    self.record(&format!("stage 2: {since} ms after stage 1; exiting"));
                    std::process::exit(3);
                }
            }

            if stale > EXIT_STALE_MS {
                self.record(&format!("event loop stalled for {stale} ms; exiting"));
                std::process::exit(4);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    /// A held grab with a stale heartbeat must shut the (Wayland) socket down within about
    /// STALE_MS, so the compositor side sees EOF and drops the grab.
    #[test]
    fn stage1_shuts_down_socket_when_heartbeat_goes_stale() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let wd = Watchdog::start(None, Duration::from_secs(60));
        wd.set_wayland_fd(Some(OwnedFd::from(ours)));
        wd.beat();
        wd.set_grab_held(true);
        // No more heartbeats from here on.
        peer.set_read_timeout(Some(Duration::from_millis(STALE_MS + 2_000))).unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 1];
        let n = peer.read(&mut buf).expect("peer must see EOF, not a timeout");
        assert_eq!(n, 0);
        assert!(started.elapsed() >= Duration::from_millis(STALE_MS - 200));
        assert!(wd.stage1_fired());
    }

    /// Without a grab, a stale heartbeat alone does nothing (until the 30 s exit).
    #[test]
    fn no_action_without_grab() {
        let (ours, peer) = UnixStream::pair().unwrap();
        let wd = Watchdog::start(Some(Duration::from_millis(500)), Duration::from_secs(60));
        wd.set_wayland_fd(Some(OwnedFd::from(ours)));
        std::thread::sleep(Duration::from_millis(STALE_MS + 500));
        assert!(!wd.stage1_fired());
        drop(peer);
    }

    /// The hard limit fires even with a healthy heartbeat.
    #[test]
    fn hard_limit_fires_with_fresh_heartbeat() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let wd = Watchdog::start(Some(Duration::from_millis(500)), Duration::from_secs(60));
        wd.set_wayland_fd(Some(OwnedFd::from(ours)));
        wd.set_grab_held(true);
        let beat = {
            let wd = wd.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    wd.beat();
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(peer.read(&mut buf).expect("EOF within 2 s"), 0);
        beat.join().unwrap();
    }
}
