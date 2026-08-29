//! The gst-wayland-display source.
//!
//! The element is created FROM THE REGISTRY, never as a crate dependency. That is
//! deliberate and load-bearing: it keeps this crate's gstreamer-rs generation
//! independent of the one gst-wayland-display was built against, so the two can move
//! separately. Linking GWD directly would couple them forever.

use anyhow::{bail, Context, Result};
use std::time::Duration;

/// The element factory name the layer-gst-wayland-display candy installs.
pub const FACTORY: &str = "waylanddisplaysrc";

/// How long to wait for the compositor to publish its socket name.
///
/// A timeout rather than a blocking wait, because the `wayland.src` bus message is
/// BEST EFFORT: GWD posts it with `gst::warning!` on failure and carries on, so a
/// missing message is not fatal to the element but would hang us forever.
pub const WAYLAND_DISPLAY_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the source element for a render node.
///
/// `render_node` is ALWAYS a real DRM node. GWD creates the `zwp_linux_dmabuf_v1`
/// global only on the hardware path, and Aquamarine's Wayland backend hard-requires
/// that global — so a software render node cannot host a nested compositor at all.
/// `wl_shm` is not a substitute. Passing a software node here is a configuration
/// error, not a degraded mode.
pub fn make_source(render_node: &str) -> Result<gst::Element> {
    if render_node.is_empty() {
        bail!("render_node is empty — cstream requires a real DRM render node (e.g. /dev/dri/renderD128); there is no software fallback that can host a nested compositor");
    }
    let src = gst::ElementFactory::make(FACTORY)
        .property("render-node", render_node)
        .build()
        .with_context(|| {
            format!("creating {FACTORY} — is layer-gst-wayland-display installed and on GST_PLUGIN_PATH?")
        })?;

    // mouse/keyboard take EVDEV DEVICE PATHS. Leaving them unset is part of the
    // security claim: the streamer never opens an input device, it injects events
    // as upstream GStreamer events instead.
    Ok(src)
}

/// Extract the compositor's `WAYLAND_DISPLAY` from a bus message, if this is that message.
///
/// Returns `None` for every unrelated message so a caller can drive it from a bus loop.
pub fn wayland_display_from_message(msg: &gst::Message) -> Option<String> {
    let s = msg.structure()?;
    if s.name() != "wayland.src" {
        return None;
    }
    s.get::<String>("WAYLAND_DISPLAY").ok()
}
