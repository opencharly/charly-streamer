//! The per-user session leader.
//!
//! It runs as root exactly long enough to authenticate a user and open a PAM
//! session, then drops to that user and never regains privilege.
//!
//! The order below is not stylistic. `setgroups` must precede `setgid`, and
//! `setgid` must precede `setuid`: after `setuid` the process no longer has the
//! privilege required to change groups, so a wrong order leaves the session
//! holding root's supplementary groups while *appearing* to have dropped.

use anyhow::{bail, Result};

/// The identity a session runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// The order privilege must be surrendered in.
///
/// Exposed as data so the ordering can be asserted by a test without needing root
/// — the sequence is the security property, and a reordering is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropStep {
    SetGroups,
    SetGid,
    SetUid,
}

/// The canonical order. Any other order is a privilege-retention bug.
pub const DROP_ORDER: [DropStep; 3] = [DropStep::SetGroups, DropStep::SetGid, DropStep::SetUid];

/// Drop to `id` permanently, then PROVE the drop is irreversible.
///
/// The verification is the point. `setuid` silently does nothing useful if the
/// process kept a saved-set-user-ID, so a leader that only calls it can still be
/// holding root. Attempting to regain root and requiring failure is the only way
/// to know, and it is done here rather than trusted.
///
/// # Safety-relevant behaviour
/// Returns `Err` — never `Ok` — if root can still be regained afterwards. A caller
/// must treat that as fatal and refuse to run the session.
pub fn drop_privileges_permanently(id: Identity) -> Result<()> {
    if id.uid == 0 {
        bail!("refusing to 'drop' privilege to uid 0 — that is not a drop");
    }

    // SAFETY: each call is checked; ordering is the documented contract above.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 {
            bail!("setgroups(0) failed: {}", std::io::Error::last_os_error());
        }
        if libc::setgid(id.gid) != 0 {
            bail!(
                "setgid({}) failed: {}",
                id.gid,
                std::io::Error::last_os_error()
            );
        }
        if libc::setuid(id.uid) != 0 {
            bail!(
                "setuid({}) failed: {}",
                id.uid,
                std::io::Error::last_os_error()
            );
        }
    }

    verify_irreversible(id)
}

/// Confirm the process cannot climb back to root.
///
/// Checked two ways because either alone can pass while privilege is retained:
/// the effective ids must be the target's, AND an explicit attempt to regain root
/// must FAIL. A process with a saved-set-uid reports the right effective uid and
/// can still call `setuid(0)` successfully.
pub fn verify_irreversible(id: Identity) -> Result<()> {
    // SAFETY: pure queries, no state change.
    let (uid, euid, gid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if uid != id.uid || euid != id.uid {
        bail!(
            "uid did not drop: real={uid} effective={euid} want={}",
            id.uid
        );
    }
    if gid != id.gid || egid != id.gid {
        bail!(
            "gid did not drop: real={gid} effective={egid} want={}",
            id.gid
        );
    }
    // SAFETY: this MUST fail. If it succeeds the process kept a saved-set-uid and
    // the session would run recoverably-root.
    if unsafe { libc::setuid(0) } == 0 {
        bail!(
            "PRIVILEGE RETAINED: setuid(0) succeeded after the drop — the session must not start"
        );
    }
    Ok(())
}

/// The environment a PAM session must carry before `open_session`.
///
/// `XDG_SESSION_CLASS=user` is load-bearing: without it logind creates a session
/// of the wrong class and the per-user `user@UID.service` tree — which is what
/// actually owns the desktop's units — is never started.
pub fn session_env(user: &str) -> Vec<(String, String)> {
    vec![
        ("XDG_SESSION_CLASS".into(), "user".into()),
        ("XDG_SESSION_TYPE".into(), "wayland".into()),
        ("USER".into(), user.into()),
    ]
}
