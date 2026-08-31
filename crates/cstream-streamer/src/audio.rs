//! The outbound audio branch: the desktop's sound, into the same WebRTC sink.
//!
//! The graph is `pipewiresrc ! audioconvert ! audioresample ! <sink>`, and it feeds
//! **raw** audio in exactly as the video branch feeds raw video — `webrtcsink` picks and
//! inserts the encoder itself. Encoding here instead would bypass the sink's own codec
//! negotiation and put a second, unnegotiated encoder in the graph.
//!
//! ## What is being captured, and why it is a sink
//!
//! A streamed desktop has no sound card, so `pod-cstream` creates two `support.null-audio-sink`
//! endpoints: `cstream-speaker` (Audio/Sink) for applications to play into, and `cstream-mic`
//! (Audio/Source/Virtual) for the browser's microphone. The outbound track is the *monitor* of
//! the speaker — what a sink receives, observed from the other side.
//!
//! That is why [`SPEAKER_NODE`] names a **sink** and the stream still captures: PipeWire routes a
//! capture stream to a sink's monitor only when the stream carries
//! `stream.capture.sink = true`. Without it, targeting a sink asks to record from an input and
//! there is nothing to read — the stream connects and stays silent, which is far worse than an
//! error because every liveness check still passes.
//!
//! The node names are load-bearing and shared with the bed; `pod-cstream`'s own config says so.
//! Renaming one detaches the audio path silently.

use anyhow::{Context, Result};
use gst::prelude::*;

/// The PipeWire sink whose monitor carries the desktop's output.
///
/// Must match `node.name` in `pod-cstream`'s `pipewire-cstream.conf`.
pub const SPEAKER_NODE: &str = "cstream-speaker";

/// The PipeWire virtual source the browser's microphone is injected into.
///
/// Not captured by this module — it is the *inbound* direction — but named here so both
/// endpoint names live in one place and a rename shows up as one diff.
pub const MIC_NODE: &str = "cstream-mic";

/// How the audio branch is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioConfig {
    /// PipeWire node to capture. A sink name means "capture its monitor".
    pub target: String,
    /// Whether `target` is a sink whose monitor we want, rather than a source.
    pub capture_sink: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            target: SPEAKER_NODE.to_string(),
            capture_sink: true,
        }
    }
}

impl AudioConfig {
    /// Read the policy from the environment.
    ///
    /// `CSTREAM_AUDIO=off` disables the branch entirely and returns `None`; the pipeline is
    /// then byte-identical to the video-only graph. Any other value, or absence, keeps audio
    /// on — audio being *silently* absent is the failure this whole module guards against, so
    /// it is not something a typo should be able to cause.
    pub fn from_env() -> Option<Self> {
        match std::env::var("CSTREAM_AUDIO").as_deref() {
            Ok("off") => None,
            Ok(target) if !target.is_empty() && target != "on" => Some(Self {
                target: target.to_string(),
                // An explicitly named target is taken at face value: if a caller points at a
                // real source, monitor-capture would be wrong. Sinks are the default, not the
                // only case.
                capture_sink: target.ends_with("-speaker") || target.ends_with(".monitor"),
            }),
            _ => Some(Self::default()),
        }
    }

    /// The PipeWire stream properties this configuration implies.
    pub fn stream_properties(&self) -> gst::Structure {
        let mut b = gst::Structure::builder("props");
        if self.capture_sink {
            // The single property that makes monitor capture work. See the module docs.
            b = b.field("stream.capture.sink", "true");
        }
        b.build()
    }
}

/// Add `pipewiresrc ! queue ! audioconvert ! audioresample` to `pipeline` and link it into `sink`.
///
/// `sink` is the same `webrtcsink` the video branch feeds; linking raw audio into it makes it
/// request an audio pad and negotiate its own encoder.
pub fn attach(pipeline: &gst::Pipeline, sink: &gst::Element, cfg: &AudioConfig) -> Result<()> {
    let src = gst::ElementFactory::make("pipewiresrc")
        .property("target-object", &cfg.target)
        .property("stream-properties", cfg.stream_properties())
        .build()
        .context("creating pipewiresrc — is gst-plugin-pipewire installed?")?;

    // A queue between capture and the sink. The video branch takes the same precaution at its
    // tee: without it, back-pressure from one branch stalls the other, and an audio stall on a
    // shared sink stalls video too.
    let queue = gst::ElementFactory::make("queue")
        .name("audio-queue")
        .build()
        .context("creating the audio queue")?;

    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .context("creating audioconvert")?;
    let resample = gst::ElementFactory::make("audioresample")
        .build()
        .context("creating audioresample")?;

    pipeline
        .add_many([&src, &queue, &convert, &resample])
        .context("adding the audio elements")?;
    gst::Element::link_many([&src, &queue, &convert, &resample])
        .context("linking the audio chain")?;

    // Link into the sink LAST, and by element rather than by a hardcoded pad name:
    // `webrtcsink`'s request-pad template has been spelled differently across gst-plugins-rs
    // releases, and letting GStreamer match by caps is both version-proof and how the video
    // branch already does it.
    resample
        .link(sink)
        .context("linking the audio chain into the WebRTC sink")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the env-var tests.
    ///
    /// `cargo test` runs tests on parallel threads and the environment is per-PROCESS, so
    /// four tests each setting `CSTREAM_AUDIO` race: one clears the variable while another is
    /// reading it. They passed on the first run here, which is exactly how that class of
    /// flake hides.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<T>(val: Option<&str>, f: impl FnOnce() -> T) -> T {
        // Poisoning is irrelevant: the guard protects a variable, not an invariant, and a
        // panicking test should not cascade into failures in every sibling.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match val {
            Some(v) => std::env::set_var("CSTREAM_AUDIO", v),
            None => std::env::remove_var("CSTREAM_AUDIO"),
        }
        let out = f();
        std::env::remove_var("CSTREAM_AUDIO");
        out
    }

    #[test]
    fn the_default_captures_the_speaker_monitor() {
        let c = AudioConfig::default();
        assert_eq!(c.target, SPEAKER_NODE);
        assert!(
            c.capture_sink,
            "the speaker is a SINK; without capture_sink the stream connects and stays silent"
        );
    }

    #[test]
    fn capture_sink_is_what_reaches_the_stream_properties() {
        let _ = gst::init();
        let props = AudioConfig::default().stream_properties();
        assert_eq!(
            props.get::<String>("stream.capture.sink").ok(),
            Some("true".to_string()),
        );
    }

    #[test]
    fn a_source_target_does_not_ask_for_monitor_capture() {
        let _ = gst::init();
        let c = AudioConfig {
            target: "some-real-microphone".into(),
            capture_sink: false,
        };
        assert!(
            c.stream_properties()
                .get::<String>("stream.capture.sink")
                .is_err(),
            "monitor capture must not be forced onto a real source"
        );
    }

    #[test]
    fn off_disables_the_branch_entirely() {
        assert_eq!(with_env(Some("off"), AudioConfig::from_env), None);
    }

    /// Actually RUN `attach` — construct the elements, set the properties, link the chain.
    ///
    /// The tests above only exercise the config struct, which would leave every way this can
    /// really fail untested: a misspelled element, a property `pipewiresrc` does not have (a
    /// GObject property set that misses is a runtime warning, not a compile error), or a chain
    /// that will not link. Those are precisely the mistakes worth catching before a bed run.
    ///
    /// `webrtcsink` is not required to be installed for this: it is `fakesink` here, which
    /// accepts any caps. What this does NOT prove is the real sink requesting an audio pad, or
    /// a single sample flowing — that needs the pod bed, and is not claimed here.
    #[test]
    fn attach_really_builds_and_links_the_chain() {
        let _ = gst::init();
        if gst::ElementFactory::find("pipewiresrc").is_none() {
            eprintln!("SKIP: gst-plugin-pipewire absent — cannot exercise attach() here");
            return;
        }
        let pipeline = gst::Pipeline::new();
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink is in gstreamer core");
        pipeline.add(&sink).unwrap();

        attach(&pipeline, &sink, &AudioConfig::default()).expect("the audio chain must link");

        // The element really carries the property that makes monitor capture work — read back
        // off the constructed element, not off the config that asked for it.
        let src = pipeline
            .children()
            .into_iter()
            .find(|e| {
                e.factory()
                    .map(|f| f.name() == "pipewiresrc")
                    .unwrap_or(false)
            })
            .expect("pipewiresrc should be in the pipeline");
        assert_eq!(src.property::<String>("target-object"), SPEAKER_NODE);
        let props: gst::Structure = src.property("stream-properties");
        assert_eq!(
            props.get::<String>("stream.capture.sink").ok(),
            Some("true".to_string()),
            "the element must carry stream.capture.sink, or it captures silence"
        );
    }

    #[test]
    fn absent_or_on_keeps_audio_with_the_default_target() {
        assert_eq!(
            with_env(None, AudioConfig::from_env),
            Some(AudioConfig::default())
        );
        assert_eq!(
            with_env(Some("on"), AudioConfig::from_env),
            Some(AudioConfig::default())
        );
    }

    #[test]
    fn a_typo_keeps_audio_on_rather_than_silently_dropping_it() {
        // "of" is not "off". The failure this module exists to prevent is audio that is
        // silently absent, so only the exact word disables it.
        let c = with_env(Some("of"), AudioConfig::from_env).expect("a typo must not disable audio");
        assert_eq!(c.target, "of");
    }

    #[test]
    fn an_explicit_monitor_target_still_asks_for_sink_capture() {
        let c = with_env(Some("cstream-speaker.monitor"), AudioConfig::from_env).unwrap();
        assert!(c.capture_sink);
    }

    #[test]
    fn the_two_endpoint_names_match_pod_cstreams_config() {
        // These are addressed by node.name from three places: this module, pod-cstream's
        // pipewire-cstream.conf, and the bed. A rename that misses one detaches audio silently.
        assert_eq!(SPEAKER_NODE, "cstream-speaker");
        assert_eq!(MIC_NODE, "cstream-mic");
    }
}
