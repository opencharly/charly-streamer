//! cstream-streamer — the Wayland streamer.
//!
//! Four modules carry the parts that are easy to get subtly wrong:
//!   * `rank`    — encoder promotion, which must happen before the first webrtcsink
//!   * `display` — the gst-wayland-display source, created from the registry
//!   * `input`   — event field types GWD `.expect()`s exactly
//!   * `webrtc`  — which encoder actually carries the stream, and saying so out loud

pub mod control;
pub mod display;
pub mod input;
pub mod pipeline;
pub mod rank;
pub mod webrtc;

/// Initialise GStreamer and promote hardware encoders in the correct order.
///
/// The order is the contract: webrtcsink's codec table is a one-shot `LazyLock`
/// populated on first use, so promotion after that point is silently ineffective.
pub fn init() -> anyhow::Result<Vec<String>> {
    gst::init()?;
    Ok(rank::raise_hardware_encoders())
}
