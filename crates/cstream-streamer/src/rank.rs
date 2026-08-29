//! Encoder ranking.
//!
//! webrtcsink enumerates encoders ONCE, into a `LazyLock` `CODECS` table, and only
//! considers those at `Rank::MARGINAL` or above. `vah264enc` registers at
//! `GST_RANK_NONE`, so without intervention webrtcsink never sees it and silently
//! picks a software encoder — the exact "works, but on the CPU" failure the spec's
//! §5.5 contract exists to prevent.
//!
//! The ordering constraint is the whole point: because `CODECS` is a one-shot
//! `LazyLock`, raising the rank AFTER the first webrtcsink is created has no effect
//! at all. `raise_hardware_encoders` must run before then, and
//! `ranking_is_still_pending` exists so a caller can assert it did.

use gst::prelude::*;

/// Elements promoted so webrtcsink's one-shot enumeration can see them.
pub const HARDWARE_ENCODERS: &[&str] = &["vah264enc", "vah265enc"];

/// Raise every available hardware encoder above webrtcsink's MARGINAL floor.
///
/// Returns the elements actually promoted. A missing element is NOT an error: a host
/// with no VA-API simply has none, and `encode: auto` is expected to fall through to
/// software there. Refusing to start would break the software path for no reason.
pub fn raise_hardware_encoders() -> Vec<String> {
    let mut raised = Vec::new();
    for name in HARDWARE_ENCODERS {
        if let Some(factory) = gst::ElementFactory::find(name) {
            // PRIMARY + 1 puts it above every software encoder, which sit at or below
            // PRIMARY. MARGINAL alone would clear webrtcsink's floor but would not
            // guarantee it wins the pick.
            factory.set_rank(gst::Rank::PRIMARY + 1);
            raised.push((*name).to_string());
        }
    }
    raised
}

/// True while no hardware encoder has been promoted yet.
///
/// A caller can use this to fail loudly rather than stream on the CPU by accident.
pub fn ranking_is_still_pending() -> bool {
    HARDWARE_ENCODERS.iter().all(|name| {
        gst::ElementFactory::find(name)
            .map(|f| f.rank() < gst::Rank::MARGINAL)
            .unwrap_or(true)
    })
}

/// Whether this host has a VA-API H.264 encoder at all.
pub fn has_hardware_h264() -> bool {
    gst::ElementFactory::find("vah264enc").is_some()
}
