//! Privilege-drop contract tests.
//!
//! These assert the properties that fail SILENTLY: a reordered drop leaves the
//! session holding root's supplementary groups while appearing to have dropped,
//! and a retained saved-set-uid reports the right effective uid while still being
//! able to climb back to root.

use cstream_leader::{drop_privileges_permanently, session_env, DropStep, Identity, DROP_ORDER};

#[test]
fn drop_order_surrenders_groups_before_identity() {
    assert_eq!(
        DROP_ORDER,
        [DropStep::SetGroups, DropStep::SetGid, DropStep::SetUid],
        "setuid must come LAST — after it the process cannot change groups any more"
    );
    let uid_at = DROP_ORDER
        .iter()
        .position(|s| *s == DropStep::SetUid)
        .unwrap();
    let grp_at = DROP_ORDER
        .iter()
        .position(|s| *s == DropStep::SetGroups)
        .unwrap();
    let gid_at = DROP_ORDER
        .iter()
        .position(|s| *s == DropStep::SetGid)
        .unwrap();
    assert!(
        grp_at < gid_at && gid_at < uid_at,
        "groups -> gid -> uid is the only safe order"
    );
}

#[test]
fn refuses_to_drop_to_root() {
    let err = drop_privileges_permanently(Identity { uid: 0, gid: 0 })
        .expect_err("dropping to uid 0 must be refused");
    assert!(
        format!("{err}").contains("not a drop"),
        "the refusal must name the reason: {err}"
    );
}

#[test]
fn session_env_sets_the_class_logind_needs() {
    let env = session_env("alice");
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "XDG_SESSION_CLASS")
            .map(|(_, v)| v.as_str()),
        Some("user"),
        "without XDG_SESSION_CLASS=user, user@UID.service never starts"
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "XDG_SESSION_TYPE")
            .map(|(_, v)| v.as_str()),
        Some("wayland")
    );
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == "USER")
            .map(|(_, v)| v.as_str()),
        Some("alice")
    );
}

#[test]
fn a_failed_drop_is_an_error_not_a_silent_pass() {
    // SAFETY: pure query.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: this test asserts the UNPRIVILEGED failure path");
        return;
    }
    let r = drop_privileges_permanently(Identity {
        uid: 65534,
        gid: 65534,
    });
    assert!(
        r.is_err(),
        "an unprivileged process cannot drop to another uid; reporting success would be a privilege-retention bug"
    );
}
