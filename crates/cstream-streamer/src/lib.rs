//! cstream-streamer — the Wayland streamer.
//!
//! These modules carry the parts that are easy to get subtly wrong:
//!   * `rank`    — encoder promotion, which must happen before the first webrtcsink
//!   * `display` — the gst-wayland-display source, created from the registry
//!   * `webrtc`  — which encoder actually carries the stream, and saying so out loud
//!   * `audio`   — monitor capture, which connects and stays SILENT if misconfigured
//!   * `lifecycle` — locking the desktop once the last viewer is gone
//!
//! Input has no module here: webrtcsink turns the browser's control-channel
//! messages into GstNavigation events, and waylanddisplaysrc maps those onto its
//! own seat (key names to evdev scancodes included). The contract this crate owns is
//! the JSON shape, asserted in `tests/contract.rs`.

pub mod audio;
pub mod control;
pub mod display;
pub mod keys;
pub mod lifecycle;
pub mod pipeline;
pub mod rank;
pub mod status;
pub mod webrtc;

/// Initialise GStreamer and promote hardware encoders in the correct order.
///
/// The order is the contract: webrtcsink's codec table is a one-shot `LazyLock`
/// populated on first use, so promotion after that point is silently ineffective.
pub fn init(backend: Option<&rank::Backend>) -> anyhow::Result<Vec<String>> {
    gst::init()?;
    Ok(rank::raise_hardware_encoders(backend))
}
