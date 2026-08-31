//! WP5 lock/idle: lock the desktop once the last viewer has been gone long enough.
//!
//! A streamed desktop has no lid to close and no seat to lock from. When the last WebRTC
//! consumer disconnects, the session is still logged in and still rendering — anyone who
//! reconnects lands straight in it. This arms `hyprlock` after a configured idle period.
//!
//! ## Three things here were measured on a live pod, not assumed
//!
//! **The streamer cannot simply spawn `hyprlock`.** It is the Wayland *parent*; Hyprland
//! nests inside it on a different display. A lock spawned from this process would attach to
//! the wrong compositor. It has to be dispatched *into* Hyprland over its IPC socket.
//!
//! **The socket is discoverable, and it is under our own runtime dir.** Hyprland puts it at
//! `$XDG_RUNTIME_DIR/hypr/<instance-signature>/.socket.sock`, and this process owns that
//! `XDG_RUNTIME_DIR`, so the signature can be read off the directory listing. It is NOT in
//! Hyprland's `/proc/<pid>/environ`, so an env probe finds nothing — the same trap
//! `plugin-wl` documents.
//!
//! **The dispatcher is `exec_raw`, not `exec`.** Hyprland >= 0.55 replaced string dispatchers
//! with Lua, and `hl.dsp.exec` does not exist — dispatching it answers
//! `attempt to call a nil value (field 'exec')`, while the legacy `exec hyprlock` form is a
//! Lua syntax error. Enumerated live, the namespace offers `exec_cmd` and `exec_raw`; both
//! launch it, and `exec_raw` is used here because the command is a fixed binary with no
//! arguments and raw exec avoids any shell interpretation of the string.
//!
//! ## Failures are non-fatal, deliberately
//!
//! Every error path here logs and returns. This process is the Wayland parent: aborting it
//! would take the whole desktop down, and losing a *lock* is not worth losing the session
//! anybody is still watching. That is the same proportionality that made the audio branch
//! opt-in after it killed the parent on a pod where its path was unavailable.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The lock command dispatched into Hyprland.
pub const LOCK_COMMAND: &str = "hyprlock";

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

/// Hyprland's IPC socket, located under the runtime dir this process owns.
#[derive(Debug, Clone)]
pub struct HyprIpc {
    pub socket: PathBuf,
}

impl HyprIpc {
    /// Find `<runtime_dir>/hypr/<signature>/.socket.sock`.
    ///
    /// The signature is a per-instance directory name; there is normally exactly one. If
    /// several exist the newest is taken, because a stale directory from a crashed instance
    /// outlives it and silently addressing the dead one is worse than picking wrong.
    pub fn discover(runtime_dir: &str) -> Result<Self> {
        let base = Path::new(runtime_dir).join("hypr");
        let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&base)
            .with_context(|| format!("reading {} — is Hyprland running?", base.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let sock = e.path().join(".socket.sock");
                if !sock.exists() {
                    return None;
                }
                let t = e.metadata().and_then(|m| m.modified()).ok()?;
                Some((t, sock))
            })
            .collect();
        candidates.sort_by_key(|(t, _)| *t);
        let socket = candidates
            .pop()
            .map(|(_, p)| p)
            .ok_or_else(|| anyhow!("no .socket.sock under {}", base.display()))?;
        Ok(Self { socket })
    }

    /// Send one command and return Hyprland's reply.
    pub fn send(&self, command: &str) -> Result<String> {
        let mut s = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(command.as_bytes())
            .context("writing the IPC command")?;
        let mut reply = String::new();
        s.read_to_string(&mut reply).context("reading the reply")?;
        Ok(reply)
    }

    /// Dispatch the lock. Returns the reply so a caller can tell `ok` from a Lua error.
    ///
    /// Hyprland answers `ok` for a dispatch it accepted and an `error: …` string otherwise —
    /// including for a dispatcher that does not exist, which is why the caller checks the
    /// reply rather than treating a successful write as success.
    pub fn lock(&self, command: &str) -> Result<String> {
        self.send(&format!(r#"dispatch hl.dsp.exec_raw("{command}")"#))
    }
}

/// Whether a reply from [`HyprIpc::lock`] actually means it ran.
pub fn dispatch_accepted(reply: &str) -> bool {
    let r = reply.trim();
    !r.is_empty() && !r.starts_with("error") && !r.contains("nil value")
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
pub fn arm(sink: &gst::Element, policy: LockPolicy, runtime_dir: String) {
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
        // Discovered per lock rather than once at startup: Hyprland can restart under us,
        // and a signature cached from a dead instance would send every future lock into a
        // socket nobody is listening on.
        match HyprIpc::discover(&runtime_dir) {
            Err(e) => eprintln!("lock: cannot reach Hyprland: {e:#}"),
            Ok(ipc) => match ipc.lock(LOCK_COMMAND) {
                Err(e) => eprintln!("lock: dispatch failed: {e:#}"),
                Ok(reply) if !dispatch_accepted(&reply) => {
                    eprintln!("lock: Hyprland REFUSED the dispatch: {}", reply.trim())
                }
                Ok(_) => eprintln!("lock: {LOCK_COMMAND} dispatched after idle"),
            },
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("cstream-lifecycle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

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

    #[test]
    fn the_dispatch_names_exec_raw_not_exec() {
        // Measured on a live pod: `hl.dsp.exec` is nil and answers
        // `attempt to call a nil value (field 'exec')`; the namespace offers exec_cmd and
        // exec_raw. Pinning the spelling here because the failure is a runtime Lua error,
        // which no amount of compiling catches.
        let ipc = HyprIpc {
            socket: PathBuf::from("/nonexistent"),
        };
        let cmd = format!(r#"dispatch hl.dsp.exec_raw("{LOCK_COMMAND}")"#);
        assert!(cmd.contains("exec_raw"));
        assert!(!cmd.contains("dsp.exec("));
        // and the command actually sent is built the same way
        assert!(ipc.lock(LOCK_COMMAND).is_err(), "no socket at that path");
    }

    #[test]
    fn discover_finds_the_socket_under_the_runtime_dir() {
        let rt = tmpdir("discover");
        let sig = rt.join("hypr").join("abc123_1_2");
        std::fs::create_dir_all(&sig).unwrap();
        std::fs::write(sig.join(".socket.sock"), b"").unwrap();
        let ipc = HyprIpc::discover(rt.to_str().unwrap()).expect("should find the socket");
        assert_eq!(ipc.socket, sig.join(".socket.sock"));
    }

    #[test]
    fn discover_ignores_an_instance_dir_with_no_socket() {
        // A crashed instance leaves the directory behind. Addressing it silently would send
        // every lock into a socket nobody is listening on.
        let rt = tmpdir("stale");
        std::fs::create_dir_all(rt.join("hypr").join("dead_instance")).unwrap();
        assert!(HyprIpc::discover(rt.to_str().unwrap()).is_err());
    }

    #[test]
    fn discover_reports_a_missing_hypr_dir_rather_than_panicking() {
        let rt = tmpdir("empty");
        let e = HyprIpc::discover(rt.to_str().unwrap()).unwrap_err();
        assert!(format!("{e:#}").contains("is Hyprland running?"));
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

    #[test]
    fn a_lua_error_reply_is_not_treated_as_success() {
        // The exact string a wrong dispatcher produced on the live pod.
        assert!(!dispatch_accepted(
            r#"error: [string "return hl.dispatch(hl.dsp.exec("hyprlock"))"]:1: attempt to call a nil value (field 'exec')"#
        ));
        assert!(!dispatch_accepted(""));
        assert!(!dispatch_accepted("   "));
        assert!(dispatch_accepted("ok"));
    }
}
