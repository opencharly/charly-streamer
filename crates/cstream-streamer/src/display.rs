//! The gst-wayland-display source.
//!
//! The element is created FROM THE REGISTRY, never as a crate dependency. That is
//! deliberate and load-bearing: it keeps this crate's gstreamer-rs generation
//! independent of the one gst-wayland-display was built against, so the two can move
//! separately. Linking GWD directly would couple them forever.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use std::time::Duration;

/// The element factory name the layer-gst-wayland-display candy installs.
pub const FACTORY: &str = "waylanddisplaysrc";

/// How long to wait for the compositor to publish its socket name.
///
/// A timeout rather than a blocking wait, because the `wayland.src` bus message is
/// BEST EFFORT: GWD posts it with `gst::warning!` on failure and carries on, so a
/// missing message is not fatal to the element but would hang us forever.
pub const WAYLAND_DISPLAY_TIMEOUT: Duration = Duration::from_secs(10);

/// The render node this stream renders and encodes on.
///
/// `CSTREAM_RENDER_NODE` when the deployment names one, else `DRINODE`, the render node
/// charly's device detection injects into every pod it configures. There is no literal
/// default: on a multi-GPU host a guessed node is the wrong GPU as often as the right
/// one, and the failure -- a stream on the card nobody chose -- is silent. Choosing among
/// GPUs is charly's job (DRINODE), overridable per deployment.
pub fn render_node_from_env() -> Result<String> {
    render_node_from(|k| std::env::var(k).ok())
}

/// [`render_node_from_env`] over an injected environment, for tests.
pub fn render_node_from(get: impl Fn(&str) -> Option<String>) -> Result<String> {
    for key in ["CSTREAM_RENDER_NODE", "DRINODE"] {
        if let Some(v) = get(key).filter(|v| !v.trim().is_empty()) {
            return Ok(v);
        }
    }
    bail!("no render node: set CSTREAM_RENDER_NODE, or deploy through charly, which sets DRINODE")
}

/// Build the source element for a render node.
///
/// `render_node` is ALWAYS a real DRM node. GWD creates the `zwp_linux_dmabuf_v1`
/// global only on the hardware path, and Aquamarine's Wayland backend hard-requires
/// that global — so a software render node cannot host a nested compositor at all.
/// `wl_shm` is not a substitute. Passing a software node here is a configuration
/// error, not a degraded mode.
pub fn make_source(render_node: &str, cuda_device: Option<i32>) -> Result<gst::Element> {
    if render_node.is_empty() {
        bail!("render_node is empty — cstream requires a real DRM render node (a /dev/dri/renderD* device, set by CSTREAM_RENDER_NODE); there is no software fallback that can host a nested compositor");
    }
    let src = gst::ElementFactory::make(FACTORY)
        .property("render-node", render_node)
        .build()
        .with_context(|| {
            format!("creating {FACTORY} — is layer-gst-wayland-display installed and on GST_PLUGIN_PATH?")
        })?;

    // The CUDA path: the source allocates CUDAMemory on this device, the one whose
    // render node it composites on, and NVENC encodes it without a copy.
    //
    // Guarded by the property's existence, not assumed: a waylanddisplaysrc built
    // WITHOUT the cuda feature has no such property, and setting a missing property
    // on a GObject panics. Measured on an NVIDIA host whose image carried the
    // non-cuda layer: `panicked at glib/src/object.rs: property 'cuda-device-id' of
    // type 'GstWaylandDisplaySrc' not found`. A described error is the right answer
    // there, because the caller asked for a path this image cannot take
    // (opencharly/charly-streamer#16).
    if let Some(id) = cuda_device {
        if src.find_property("cuda-device-id").is_none() {
            bail!(
                "the waylanddisplaysrc in this image has no `cuda-device-id` property: it was built \
                 without the cuda feature (layer-gst-wayland-display), so the CUDA backend cannot be \
                 used. Build that layer with `--features cuda`, or run with a non-CUDA encoder policy"
            );
        }
        src.set_property("cuda-device-id", id);
    }

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
mod render_node_tests {
    use super::*;

    #[test]
    fn the_deployment_override_wins_then_charlys_drinode() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            render_node_from(env(&[
                ("CSTREAM_RENDER_NODE", "/dev/dri/renderD129"),
                ("DRINODE", "/dev/dri/renderD128")
            ]))
            .unwrap(),
            "/dev/dri/renderD129"
        );
        assert_eq!(
            render_node_from(env(&[("DRINODE", "/dev/dri/renderD128")])).unwrap(),
            "/dev/dri/renderD128"
        );
        assert_eq!(
            render_node_from(env(&[
                ("CSTREAM_RENDER_NODE", " "),
                ("DRINODE", "/dev/dri/renderD128")
            ]))
            .unwrap(),
            "/dev/dri/renderD128",
            "a blank override is unset, not a node"
        );
        let err = render_node_from(env(&[])).unwrap_err().to_string();
        assert!(
            err.contains("CSTREAM_RENDER_NODE") && err.contains("DRINODE"),
            "{err}"
        );
    }
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
