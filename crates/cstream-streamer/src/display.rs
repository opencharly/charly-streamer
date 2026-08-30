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

/// Prepare `XDG_RUNTIME_DIR` so the compositor can create its Wayland socket.
///
/// Taking over the parent role means taking over ALL of it, not just the
/// pipeline. GWD creates its socket under `XDG_RUNTIME_DIR`, and if the
/// directory does not exist its compositor thread dies at startup — surfacing as
/// a panic inside the element:
///
/// ```text
/// GstWaylandDisplaySrc: Panicked: called `Result::unwrap()` on an `Err` value: RecvError
/// ```
///
/// which names neither the directory nor the cause. Measured: replacing the
/// parent script with this binary dropped its `mkdir` and the service went FATAL
/// on that panic.
///
/// Only a directory we CREATE is chmod'ed. `chmod` on a pre-existing `/tmp` path
/// we do not own fails with EPERM, and failing there would kill the streamer
/// before it does any work — the parent script carried the same warning.
pub fn prepare_runtime_dir(dir: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::path::Path::new(dir);
    if path.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating XDG_RUNTIME_DIR {dir} for the Wayland socket"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("tightening {dir} to 0700"))?;
    Ok(())
}

#[cfg(test)]
mod runtime_dir_tests {
    use super::*;

    /// A missing runtime dir must be created 0700, and an existing one left alone.
    ///
    /// Both halves matter. Without the create, GWD's compositor dies at startup and
    /// the failure surfaces as an opaque `RecvError` panic inside the element. With
    /// an unconditional chmod, a pre-existing `/tmp` path we do not own fails with
    /// EPERM and kills the streamer before it does any work.
    #[test]
    fn creates_a_missing_runtime_dir_but_does_not_touch_an_existing_one() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("cstream-rt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        prepare_runtime_dir(base.to_str().unwrap()).expect("should create a missing dir");
        assert!(base.is_dir(), "the runtime dir was not created");
        let mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "a created runtime dir must be 0700, got {mode:o}"
        );

        // Loosen it, then re-run: an existing directory must be left exactly as found,
        // because chmod'ing a path we do not own is the EPERM that killed the launcher.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_runtime_dir(base.to_str().unwrap()).expect("should accept an existing dir");
        let after = std::fs::metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, 0o755, "an existing runtime dir must not be chmod'ed");

        let _ = std::fs::remove_dir_all(&base);
    }
}
