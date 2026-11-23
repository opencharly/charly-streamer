//! WP5 lock/idle: lock the desktop once the last viewer has been gone long enough.
//!
//! A streamed desktop has no lid to close and no seat to lock from. When the last WebRTC
//! consumer disconnects, the session is still logged in and still rendering — anyone who
//! reconnects lands straight in it. This locks it after a configured idle period.
//!
//! ## The lock is the desktop's own, asked for over D-Bus
//!
//! The streamer is the Wayland *parent*; the desktop nests inside it on a different
//! display, so a locker spawned from this process would attach to the wrong compositor.
//! The lock is therefore REQUESTED from the desktop, through the freedesktop screensaver
//! interface (`org.freedesktop.ScreenSaver.Lock` on the session bus). KDE Plasma's
//! kscreenlocker implements it, as do the other desktops that ship a locker, so the
//! streamer names no compositor at all. `DBUS_SESSION_BUS_ADDRESS` in this process's
//! environment selects the desktop's bus.
//!
//! The call goes through `gdbus` with a bounded `--timeout`: a desktop whose locker
//! is wedged fails the call instead of hanging the lock thread.
//!
//! ## Failures are non-fatal, deliberately
//!
//! Every error path here logs and returns. This process is the Wayland parent: aborting it
//! would take the whole desktop down, and losing a *lock* is not worth losing the session
//! anybody is still watching. That is the same proportionality that made the audio branch
//! opt-in after it killed the parent on a pod where its path was unavailable.

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// The freedesktop screensaver service the desktop's locker owns.
pub const LOCK_DEST: &str = "org.freedesktop.ScreenSaver";
/// Its object path.
pub const LOCK_PATH: &str = "/ScreenSaver";
/// The method that locks the session.
pub const LOCK_METHOD: &str = "org.freedesktop.ScreenSaver.Lock";
/// How long the desktop may take to answer, in seconds (gdbus `--timeout`).
pub const LOCK_TIMEOUT_SECS: u32 = 5;

/// The argv of the lock request.
pub fn lock_argv() -> Vec<String> {
    [
        "gdbus",
        "call",
        "--session",
        "--timeout",
        &LOCK_TIMEOUT_SECS.to_string(),
        "--dest",
        LOCK_DEST,
        "--object-path",
        LOCK_PATH,
        "--method",
        LOCK_METHOD,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Ask the desktop to lock. `Ok` only when the desktop ANSWERED the call: a missing
/// locker answers `ServiceUnknown`, which is a failure to report, not a lock.
pub fn lock_desktop() -> Result<()> {
    let argv = lock_argv();
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .context("running gdbus")?;
    if !out.status.success() {
        bail!(
            "{LOCK_METHOD} refused ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// When to lock after the last consumer leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockPolicy {
    pub after: Duration,
}

impl LockPolicy {
    /// `CSTREAM_LOCK_AFTER=<seconds>` arms it. Unset, empty, `off` or `0` disable it.
    ///
    /// Opt-in, like the audio branch and for a related reason: locking is a visible action
    /// taken on someone's session, and a deployment that has not asked for it should not get
    /// it because a default changed underneath them.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("CSTREAM_LOCK_AFTER").ok()?;
        let raw = raw.trim();
        if raw.is_empty() || raw == "off" {
            return None;
        }
        match raw.parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Self {
                after: Duration::from_secs(secs),
            }),
            // A malformed value must not silently mean "never lock" — that is the
            // security-relevant direction of this feature. Fail loud at parse time.
            Err(_) => {
                eprintln!(
                    "lock: CSTREAM_LOCK_AFTER={raw:?} is not a number of seconds — ignoring, \
                     the desktop will NOT auto-lock"
                );
                None
            }
        }
    }
}

/// When to lock, as a pure state machine over consumer arrivals and departures.
///
/// Kept free of GStreamer and of the clock so the decision itself is testable: the glue in
/// [`arm`] only feeds it events and a `now`.
#[derive(Debug, Default)]
pub struct LockTracker {
    consumers: usize,
    /// When the count last reached zero. `None` while anybody is watching.
    zero_since: Option<std::time::Instant>,
    /// Set once we have locked for this idle period, so the lock fires ONCE and not every
    /// tick afterwards — a lock re-dispatched every second would fight the user typing their
    /// password.
    locked: bool,
}

impl LockTracker {
    pub fn on_consumer_added(&mut self) {
        self.consumers += 1;
        self.zero_since = None;
        // Someone is watching again, so the next departure gets a fresh countdown.
        self.locked = false;
    }

    pub fn on_consumer_removed(&mut self, now: std::time::Instant) {
        self.consumers = self.consumers.saturating_sub(1);
        if self.consumers == 0 {
            self.zero_since = Some(now);
        }
    }

    /// True exactly once per idle period, when the deadline has passed with nobody watching.
    pub fn should_lock(&mut self, now: std::time::Instant, after: Duration) -> bool {
        if self.locked || self.consumers > 0 {
            return false;
        }
        match self.zero_since {
            Some(t) if now.duration_since(t) >= after => {
                self.locked = true;
                true
            }
            _ => false,
        }
    }
}

/// Wire the tracker to `sink`'s consumer signals and poll it on a background thread.
///
/// Every failure path logs and returns: see the module docs on why nothing here may abort
/// the process.
pub fn arm(sink: &gst::Element, policy: LockPolicy) {
    use gst::prelude::*;
    use std::sync::{Arc, Mutex};

    let tracker = Arc::new(Mutex::new(LockTracker::default()));

    let t = Arc::clone(&tracker);
    sink.connect("consumer-added", false, move |_| {
        if let Ok(mut g) = t.lock() {
            g.on_consumer_added();
        }
        None
    });

    let t = Arc::clone(&tracker);
    sink.connect("consumer-removed", false, move |_| {
        if let Ok(mut g) = t.lock() {
            g.on_consumer_removed(std::time::Instant::now());
        }
        None
    });

    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let fire = tracker
            .lock()
            .map(|mut g| g.should_lock(std::time::Instant::now(), policy.after))
            .unwrap_or(false);
        if !fire {
            continue;
        }
        match lock_desktop() {
            Err(e) => eprintln!("lock: {e:#}"),
            Ok(()) => eprintln!("lock: {LOCK_METHOD} requested after idle"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_env<T>(val: Option<&str>, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match val {
            Some(v) => std::env::set_var("CSTREAM_LOCK_AFTER", v),
            None => std::env::remove_var("CSTREAM_LOCK_AFTER"),
        }
        let out = f();
        std::env::remove_var("CSTREAM_LOCK_AFTER");
        out
    }

    #[test]
    fn locking_is_off_unless_asked_for() {
        assert_eq!(with_env(None, LockPolicy::from_env), None);
        assert_eq!(with_env(Some(""), LockPolicy::from_env), None);
        assert_eq!(with_env(Some("off"), LockPolicy::from_env), None);
        assert_eq!(with_env(Some("0"), LockPolicy::from_env), None);
    }

    #[test]
    fn a_duration_arms_it() {
        let p = with_env(Some("300"), LockPolicy::from_env).expect("300 should arm the lock");
        assert_eq!(p.after, Duration::from_secs(300));
    }

    /// The request names the freedesktop interface and a bounded timeout; no
    /// compositor appears in it.
    #[test]
    fn the_lock_is_the_freedesktop_screensaver_call() {
        let argv = lock_argv();
        assert_eq!(argv[0], "gdbus");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--dest" && w[1] == LOCK_DEST));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--method" && w[1] == LOCK_METHOD));
        assert!(argv.windows(2).any(|w| w[0] == "--timeout" && w[1] == "5"));
        assert!(argv.contains(&"--session".to_string()));
    }

    #[test]
    fn it_locks_only_after_the_deadline_and_only_once() {
        let after = Duration::from_secs(60);
        let t0 = std::time::Instant::now();
        let mut k = LockTracker::default();

        k.on_consumer_added();
        assert!(!k.should_lock(t0, after), "somebody is watching");

        k.on_consumer_removed(t0);
        assert!(
            !k.should_lock(t0 + Duration::from_secs(59), after),
            "too early"
        );
        assert!(
            k.should_lock(t0 + Duration::from_secs(60), after),
            "deadline reached"
        );
        assert!(
            !k.should_lock(t0 + Duration::from_secs(600), after),
            "must fire ONCE — re-dispatching every tick would fight the user typing a password"
        );
    }

    #[test]
    fn a_second_viewer_leaving_does_not_start_the_clock() {
        // Two watchers, one leaves: the desktop is still being watched, so no countdown.
        let after = Duration::from_secs(10);
        let t0 = std::time::Instant::now();
        let mut k = LockTracker::default();
        k.on_consumer_added();
        k.on_consumer_added();
        k.on_consumer_removed(t0);
        assert!(!k.should_lock(t0 + Duration::from_secs(999), after));
        k.on_consumer_removed(t0);
        assert!(k.should_lock(t0 + Duration::from_secs(10), after));
    }

    #[test]
    fn reconnecting_rearms_the_lock_for_the_next_departure() {
        let after = Duration::from_secs(5);
        let t0 = std::time::Instant::now();
        let mut k = LockTracker::default();
        k.on_consumer_added();
        k.on_consumer_removed(t0);
        assert!(k.should_lock(t0 + Duration::from_secs(5), after));
        // Viewer returns, then leaves again — it must lock a second time.
        k.on_consumer_added();
        let t1 = t0 + Duration::from_secs(100);
        k.on_consumer_removed(t1);
        assert!(k.should_lock(t1 + Duration::from_secs(5), after));
    }

    #[test]
    fn a_stray_removal_cannot_underflow_the_count() {
        // consumer-removed without a matching add would panic on `-= 1` in debug.
        let mut k = LockTracker::default();
        k.on_consumer_removed(std::time::Instant::now());
        assert_eq!(k.consumers, 0);
    }
}
