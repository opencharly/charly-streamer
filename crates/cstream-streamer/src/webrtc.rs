//! The delivery half: which encoder carries the stream, and the sink that uses it.
//!
//! The failure this module exists to prevent is a stream that WORKS while running on
//! the CPU. webrtcsink enumerates encoders once into a `LazyLock` and picks silently,
//! so a host whose VA-API encoder is present but unranked streams perfectly well at a
//! fraction of the density and nothing says so. `encode: va` therefore has to fail
//! LOUDLY rather than degrade, and `auto` has to report which branch it took.

use anyhow::{anyhow, Context, Result};
use gst::prelude::*;

use crate::rank::HARDWARE_ENCODERS;

/// What the operator asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodePolicy {
    /// Hardware when this host has it, software otherwise.
    Auto,
    /// Hardware, or fail. Never a silent CPU fallback.
    Va,
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
            "va" => Ok(Self::Va),
            "software" => Ok(Self::Software),
            other => Err(anyhow!(
                "unknown encode policy {other:?}: expected one of auto, va, software"
            )),
        }
    }
}

/// What the policy resolved to on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoder {
    /// A VA-API element, named so a probe can assert WHICH one was chosen.
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
}

/// Resolve a policy against a host.
///
/// `present` is injected rather than reading the registry directly so the decision
/// table is testable on a machine with no VA-API at all — which is every CI runner,
/// and is exactly the host where getting this wrong is invisible.
pub fn select_encoder(policy: EncodePolicy, present: &dyn Fn(&str) -> bool) -> Result<Encoder> {
    let found = HARDWARE_ENCODERS.iter().find(|n| present(n));
    match (policy, found) {
        (EncodePolicy::Software, _) => Ok(Encoder::Software),
        (_, Some(name)) => Ok(Encoder::Hardware((*name).to_string())),
        (EncodePolicy::Auto, None) => Ok(Encoder::Software),
        (EncodePolicy::Va, None) => Err(anyhow!(
            "encode: va was requested but no VA-API encoder is available on this render node \
             (looked for {}). Refusing to fall back to software: a CPU stream at this density \
             is a capacity failure, not a degraded mode.",
            HARDWARE_ENCODERS.join(", ")
        )),
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

    #[test]
    fn va_without_hardware_fails_rather_than_degrading() {
        let err = select_encoder(EncodePolicy::Va, &none)
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
    fn auto_without_hardware_uses_software() {
        assert_eq!(
            select_encoder(EncodePolicy::Auto, &none).unwrap(),
            Encoder::Software
        );
    }

    #[test]
    fn auto_with_hardware_prefers_it_and_names_it() {
        let got = select_encoder(EncodePolicy::Auto, &all).unwrap();
        assert_eq!(got, Encoder::Hardware("vah264enc".into()));
        assert_eq!(got.label(), "hardware:vah264enc");
    }

    #[test]
    fn software_is_honoured_even_where_hardware_exists() {
        // The inverse of the va case: an operator who asked for software must not be
        // silently upgraded either, or a capacity test measures the wrong path.
        assert_eq!(
            select_encoder(EncodePolicy::Software, &all).unwrap(),
            Encoder::Software
        );
    }

    #[test]
    fn an_unknown_policy_is_an_error_not_a_default() {
        assert!(EncodePolicy::parse("hardware").is_err());
        assert!(EncodePolicy::parse("").is_err());
        assert_eq!(EncodePolicy::parse(" va ").unwrap(), EncodePolicy::Va);
    }
}
