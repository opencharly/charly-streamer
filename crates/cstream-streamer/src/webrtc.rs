//! The delivery half: which encoder carries the stream, and the sink that uses it.
//!
//! The failure this module exists to prevent is a stream that WORKS while running on
//! the CPU. webrtcsink enumerates encoders once into a `LazyLock` and picks silently,
//! so a host whose VA-API encoder is present but unranked streams perfectly well at a
//! fraction of the density and nothing says so. `encode: hardware` therefore has to
//! fail LOUDLY rather than degrade, and `auto` has to report which branch it took.

use crate::pipeline::SizeLimits;
use anyhow::{anyhow, Context, Result};
use gst::prelude::*;

use crate::rank::Backend;

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodePolicy {
    /// Hardware when this host has it, software otherwise.
    Auto,
    /// The render node's hardware encoder, or fail. Never a silent CPU fallback.
    Hardware,
    /// Software, even where hardware exists.
    Software,
}

impl EncodePolicy {
    /// Parse the `encode:` value. An unknown word is an error rather than a default:
    /// silently treating a typo as `auto` is precisely how a deployment intended to
    /// be hardware-only ends up on the CPU.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "auto" => Ok(Self::Auto),
            "hardware" => Ok(Self::Hardware),
            "software" => Ok(Self::Software),
            other => Err(anyhow!(
                "unknown encode policy {other:?}: expected one of auto, hardware, software"
            )),
        }
    }
}

/// What the policy resolved to on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoder {
    /// A hardware element, named so a probe can assert WHICH one was chosen.
    Hardware(String),
    Software,
}

impl Encoder {
    pub fn label(&self) -> String {
        match self {
            Self::Hardware(name) => format!("hardware:{name}"),
            Self::Software => "software".to_string(),
        }
    }

    /// The frame sizes this encoder can carry, from its own sink template. Software is
    /// the H.264 software encoder webrtcsink uses (x264enc, else openh264enc).
    pub fn limits(&self) -> Result<SizeLimits> {
        match self {
            Self::Hardware(name) => SizeLimits::of_factory(name),
            Self::Software => ["x264enc", "openh264enc"]
                .into_iter()
                .find(|n| registry_has(n))
                .map(SizeLimits::of_factory)
                .unwrap_or_else(|| {
                    Err(anyhow!(
                        "no H.264 software encoder (x264enc, openh264enc) to size the stream for"
                    ))
                }),
        }
    }
}

/// Resolve a policy against a render node's backend.
///
/// `present` is injected rather than reading the registry directly so the decision
/// table is testable on a machine with no hardware encoder at all — which is every
/// CI runner, and is exactly the host where getting this wrong is invisible.
pub fn select_encoder(
    policy: EncodePolicy,
    backend: Option<&Backend>,
    present: &dyn Fn(&str) -> bool,
) -> Result<Encoder> {
    let candidates = backend.map(Backend::encoders).unwrap_or_default();
    let found = candidates.iter().find(|n| present(n));
    match (policy, found) {
        (EncodePolicy::Software, _) => Ok(Encoder::Software),
        (_, Some(name)) => Ok(Encoder::Hardware((*name).to_string())),
        (EncodePolicy::Auto, None) => Ok(Encoder::Software),
        (EncodePolicy::Hardware, None) => {
            Err(anyhow!(
            "encode: hardware was requested but this render node has no usable hardware encoder \
             (backend {backend:?}, looked for {}). Refusing to fall back to software: a CPU stream \
             at this density is a capacity failure, not a degraded mode.",
            if candidates.is_empty() { "none".to_string() } else { candidates.join(", ") }
        ))
        }
    }
}

/// True when the GStreamer registry has this element.
pub fn registry_has(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// The port the in-process signalling server listens on.
///
/// Overridable because a second streamer on the same host would otherwise collide on
/// the element's default (8443) and fail to bind -- which is precisely the
/// multi-session case this component exists for.
pub fn signalling_port() -> u32 {
    std::env::var("CSTREAM_SIGNALLING_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|p| (1..=65535).contains(p))
        .unwrap_or(8443)
}

/// Build the webrtcsink that carries the encoded stream.
///
/// Two details are load-bearing and neither is obvious from the property list:
///
/// - `video-caps` is pinned to H.264. Left open, webrtcsink negotiates whatever the
///   consumer prefers, which can route around the encoder this host was ranked for.
/// - the `encoder-setup` handler returns **true**. The signal's contract is "true if
///   the encoder is entirely configured"; returning false lets webrtcsink's default
///   handler run afterwards and overwrite the tuning applied here, so a handler that
///   returns false appears to work and silently has no effect.
pub fn make_sink(encoder: &Encoder) -> Result<gst::Element> {
    let sink = gst::ElementFactory::make("webrtcsink")
        .build()
        .context("creating webrtcsink — is gst-plugin-rswebrtc installed?")?;

    sink.set_property("video-caps", gst::Caps::builder("video/x-h264").build());

    // Opus for the audio track, constrained the same way and for the same reason: the
    // audio branch feeds RAW audio in (see `audio`), so without this the sink is free to
    // negotiate whatever encoder it finds first. Opus is the only audio codec every WebRTC
    // implementation is required to support, so pinning it removes a class of
    // browser-specific "connects but no sound" that would look exactly like a capture bug.
    sink.set_property("audio-caps", gst::Caps::builder("audio/x-opus").build());

    // Run the signalling server in-process. Not a convenience: webrtcsink's default
    // signaller DIALS ws://127.0.0.1:8443, and with nothing listening it posts a
    // stream error onto the bus moments after the pipeline reaches PLAYING.
    // Measured -- the graph builds, the encoder opens, both markers print, and then:
    //
    //   Error: pipeline error from GstWebRTCSink:webrtcsink0:
    //          GStreamer encountered a general stream error.
    //
    // So without this the streamer cannot serve at all, and the failure arrives AFTER
    // every "it works" signal, which is the worst place for one. `false` is the
    // element's own default, so it has to be set explicitly.
    sink.set_property("run-signalling-server", true);
    sink.set_property("signalling-server-port", signalling_port());
    // Bind the signalling server to loopback only. The gateway's authenticated
    // /signalling proxy is the ONE way in; a server on 0.0.0.0 would hand the stream
    // to anyone who can reach the port, login or not.
    sink.set_property("signalling-server-host", "127.0.0.1");

    // The browser's input path: webrtcsink opens a "control" data channel per session
    // and turns its `navigationEvent` requests into GstNavigation events sent
    // upstream, which waylanddisplaysrc maps onto its seat. Off by default, and with
    // it off a viewer can watch but nothing it types or clicks arrives.
    sink.set_property("enable-control-data-channel", true);

    let want = encoder.clone();
    sink.connect("encoder-setup", false, move |values| {
        // (sink, consumer_id, pad_name, encoder)
        if let Some(enc) = values.get(3).and_then(|v| v.get::<gst::Element>().ok()) {
            tune_encoder(&enc, &want);
        }
        // TRUE: fully configured. See the doc comment -- false is the silent no-op.
        Some(true.to_value())
    });

    Ok(sink)
}

/// Apply the live-streaming tuning an encoder needs to be usable interactively.
///
/// Guarded per property: the VA and software encoders do not share a property set,
/// and setting a missing property on a GObject is a runtime warning that aborts
/// nothing -- so an unguarded set would leave the encoder half-tuned and quiet.
fn tune_encoder(enc: &gst::Element, want: &Encoder) {
    let has = |p: &str| enc.find_property(p).is_some();
    // A desktop stream is latency-bound, not bitrate-bound: without this the encoder
    // buffers for quality and the cursor lags behind the pointer.
    if has("tune") && matches!(want, Encoder::Software) {
        enc.set_property_from_str("tune", "zerolatency");
    }
    if has("key-int-max") {
        enc.set_property("key-int-max", 60u32);
    }
    if has("bframes") {
        enc.set_property("bframes", 0u32);
    }
    if has("rate-control") {
        enc.set_property_from_str("rate-control", "cbr");
    }
}

/// Prove a chosen hardware encoder can actually OPEN on this host.
///
/// Registration is not capability. A VA element is registered whenever the plugin
/// loads, which happens on a machine with no usable render node at all -- so
/// `select_encoder` returning `Hardware` means "the element exists", and nothing
/// more. Bringing it to READY is what makes it open the driver, and it is the
/// cheapest check that distinguishes a working hardware path from a registry entry.
///
/// Software is trivially ready; only the hardware claim needs proving.
pub fn verify_openable(encoder: &Encoder) -> Result<()> {
    let Encoder::Hardware(name) = encoder else {
        return Ok(());
    };
    let enc = gst::ElementFactory::make(name)
        .build()
        .with_context(|| format!("creating {name}"))?;
    let outcome = enc.set_state(gst::State::Ready);
    // NULL first, whatever happened: an element left in READY holds the render node
    // open, and the next probe in the same container then fails for the wrong reason.
    let _ = enc.set_state(gst::State::Null);
    outcome.with_context(|| {
        format!(
            "{name} is registered but will not open on this render node -- the plugin loads \
                 even where the hardware is unusable, so a registry lookup alone proves nothing"
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }
    fn all(_: &str) -> bool {
        true
    }

    const CUDA: Backend = Backend::Cuda { device_id: 0 };

    #[test]
    fn hardware_without_an_encoder_fails_rather_than_degrading() {
        let err = select_encoder(EncodePolicy::Hardware, Some(&Backend::Va), &none)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Refusing to fall back to software"),
            "the error must name the refusal, got: {err}"
        );
        assert!(
            err.contains("vah264enc"),
            "the error must name what it looked for: {err}"
        );
    }

    #[test]
    fn hardware_on_a_node_with_no_backend_fails() {
        assert!(select_encoder(EncodePolicy::Hardware, None, &all).is_err());
    }

    #[test]
    fn auto_without_hardware_uses_software() {
        assert_eq!(
            select_encoder(EncodePolicy::Auto, Some(&Backend::Va), &none).unwrap(),
            Encoder::Software
        );
        assert_eq!(
            select_encoder(EncodePolicy::Auto, None, &all).unwrap(),
            Encoder::Software
        );
    }

    /// The backend decides WHICH hardware encoder: on an NVIDIA node a registered
    /// VA encoder (the AMD card next to it) must not be picked, and vice versa.
    #[test]
    fn the_backend_chooses_its_own_encoder() {
        let got = select_encoder(EncodePolicy::Auto, Some(&CUDA), &all).unwrap();
        assert_eq!(got, Encoder::Hardware("nvh264enc".into()));
        assert_eq!(got.label(), "hardware:nvh264enc");
        let got = select_encoder(EncodePolicy::Auto, Some(&Backend::Va), &all).unwrap();
        assert_eq!(got.label(), "hardware:vah264enc");
        let only_va = |n: &str| n.starts_with("va");
        assert_eq!(
            select_encoder(EncodePolicy::Auto, Some(&CUDA), &only_va).unwrap(),
            Encoder::Software,
            "an NVIDIA node must not stream through the other card's VA encoder"
        );
    }

    #[test]
    fn software_is_honoured_even_where_hardware_exists() {
        // The inverse of the hardware case: an operator who asked for software must
        // not be silently upgraded either, or a capacity test measures the wrong path.
        assert_eq!(
            select_encoder(EncodePolicy::Software, Some(&CUDA), &all).unwrap(),
            Encoder::Software
        );
    }

    #[test]
    fn an_unknown_policy_is_an_error_not_a_default() {
        assert!(EncodePolicy::parse("va").is_err());
        assert!(EncodePolicy::parse("").is_err());
        assert_eq!(
            EncodePolicy::parse(" hardware ").unwrap(),
            EncodePolicy::Hardware
        );
    }
}
