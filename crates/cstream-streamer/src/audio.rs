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
    /// Read the policy from the environment. **Audio is OPT-IN.**
    ///
    /// Unset or `off` → `None`, and the graph is byte-identical to the video-only one.
    /// `on` → the default target. Any other value names the target explicitly.
    ///
    /// ## Why opt-in, when silent audio is the thing this module guards against
    ///
    /// Both failures are real and they pull in opposite directions:
    ///
    ///   * audio that connects and carries silence is invisible — hence the assertions here
    ///     and the bed check on the monitor link;
    ///   * but a failure to attach aborts the process, and **this process is the Wayland
    ///     parent**. Measured on `check-cstream-pod`: with audio on by default and the audio
    ///     path unavailable, `cstream-parent` went FATAL, `cstream-hyprland` restart-looped
    ///     with no parent display, and the consumer-side frame probe hung for 11 minutes on
    ///     media that would never arrive. An optional feature took down the product.
    ///
    /// Defaulting off resolves that without weakening anything: a deployment that has not
    /// asked for audio cannot lose its desktop to it, and a deployment that HAS asked still
    /// gets a loud abort rather than silence. What must never happen is audio that was
    /// requested and is quietly absent — and that is still impossible.
    ///
    /// The audio path is not yet reachable in the pod (`pipewiresrc` reports
    /// `target not found` for every target, including the default, and no `default` Metadata
    /// object exists), so nothing enables this yet. `pod-cstream` turns it on when that is
    /// fixed, and its bed check is what will prove it.
    pub fn from_env() -> Option<Self> {
        match std::env::var("CSTREAM_AUDIO").as_deref() {
            Err(_) | Ok("") | Ok("off") => None,
            Ok("on") => Some(Self::default()),
            Ok(target) => Some(Self {
                target: target.to_string(),
                // An explicitly named target is taken at face value: if a caller points at a
                // real source, monitor-capture would be wrong. Sinks are the default, not the
                // only case.
                capture_sink: target.ends_with("-speaker") || target.ends_with(".monitor"),
            }),
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

    /// The caps `pipewiresrc` must negotiate, placed UPSTREAM of `audioconvert`.
    ///
    /// Two separate things depend on this, and both were measured on a live pod.
    ///
    /// **`audio/x-raw` at all.** `pipewiresrc` serves audio *and* video. Against a
    /// caps-agnostic peer it negotiates **video**, finds no video node, and reports
    /// `stream error: target not found` — a message that reads like "your node is missing"
    /// and is in fact "no *video* target". Naming the media type removes the ambiguity.
    ///
    /// **`channels=2` when capturing a sink's monitor.** Without it the stream negotiated a
    /// single `input_MONO` port and PipeWire linked it to `monitor_FL` **only** — the right
    /// channel was not mixed down, it was dropped. Desktop audio came out left-channel-only,
    /// while every check still passed, because a link existed and audio flowed.
    ///
    /// The constraint has to sit upstream of `audioconvert`: downstream of it, conversion
    /// simply satisfies the request and `pipewiresrc` still takes one channel.
    ///
    /// Only for a sink monitor. A real source may legitimately be mono — `cstream-mic` is —
    /// and demanding two channels there would fail negotiation for no reason.
    pub fn caps(&self) -> gst::Caps {
        let b = gst::Caps::builder("audio/x-raw");
        if self.capture_sink {
            b.field("channels", 2i32).build()
        } else {
            b.build()
        }
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

    // Immediately after the source and BEFORE the queue: see AudioConfig::caps. This is what
    // makes pipewiresrc negotiate audio rather than video, and stereo rather than one channel.
    let caps = gst::ElementFactory::make("capsfilter")
        .name("audio-caps")
        .property("caps", cfg.caps())
        .build()
        .context("creating the audio capsfilter")?;

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
        .add_many([&src, &caps, &queue, &convert, &resample])
        .context("adding the audio elements")?;
    gst::Element::link_many([&src, &caps, &queue, &convert, &resample])
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

    /// The safety property: an unconfigured deployment gets NO audio branch.
    ///
    /// This is the test that would have prevented the pod outage. Attaching audio aborts the
    /// process when the path is unavailable, and the process is the Wayland parent, so a
    /// default of "on" costs the whole desktop wherever audio is not reachable yet.
    #[test]
    fn a_sink_monitor_is_captured_in_stereo() {
        let _ = gst::init();
        let c = AudioConfig::default();
        assert_eq!(
            c.caps().structure(0).unwrap().get::<i32>("channels").ok(),
            Some(2),
            "without channels=2 the stream negotiates one input_MONO port and PipeWire links \
             monitor_FL ONLY — the right channel is dropped, not mixed"
        );
    }

    #[test]
    fn a_real_source_is_not_forced_to_stereo() {
        let _ = gst::init();
        // cstream-mic is legitimately MONO. Demanding two channels there would fail
        // negotiation for no reason, so the constraint is tied to monitor capture.
        let c = AudioConfig {
            target: MIC_NODE.into(),
            capture_sink: false,
        };
        assert!(c
            .caps()
            .structure(0)
            .unwrap()
            .get::<i32>("channels")
            .is_err());
    }

    #[test]
    fn the_caps_always_name_the_media_type() {
        let _ = gst::init();
        // pipewiresrc serves audio AND video. Against a caps-agnostic peer it negotiates
        // VIDEO and reports `target not found` — which reads as "node missing" and is not.
        for c in [
            AudioConfig::default(),
            AudioConfig {
                target: MIC_NODE.into(),
                capture_sink: false,
            },
        ] {
            assert_eq!(c.caps().structure(0).unwrap().name(), "audio/x-raw");
        }
    }

    #[test]
    fn audio_is_off_unless_asked_for() {
        assert_eq!(with_env(None, AudioConfig::from_env), None);
        assert_eq!(with_env(Some(""), AudioConfig::from_env), None);
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

        // The capsfilter must be IN the pipeline and actually LINKED downstream of the source.
        // Building an element and forgetting to add/link it compiles, runs, and silently does
        // nothing — I did exactly that while writing this, and every other assertion here still
        // passed. So assert the wiring, not just the construction.
        let capsf = pipeline
            .by_name("audio-caps")
            .expect("the audio capsfilter must be added to the pipeline");
        let peer = capsf
            .static_pad("sink")
            .and_then(|p| p.peer())
            .and_then(|p| p.parent_element())
            .expect("the capsfilter must be linked to something upstream");
        assert_eq!(
            peer.factory().map(|f| f.name()).as_deref(),
            Some("pipewiresrc"),
            "the caps must constrain pipewiresrc directly — downstream of audioconvert the \
             conversion satisfies them and the source still negotiates one channel"
        );
        let want: gst::Caps = capsf.property("caps");
        assert_eq!(
            want.structure(0).unwrap().get::<i32>("channels").ok(),
            Some(2)
        );

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
    fn on_selects_the_default_target() {
        assert_eq!(
            with_env(Some("on"), AudioConfig::from_env),
            Some(AudioConfig::default())
        );
    }

    #[test]
    fn a_typo_does_not_quietly_disable_requested_audio() {
        // "of" is not "off", so it is not a disable. It is read as a target name and audio
        // stays ON — which then fails loudly on a node that does not exist.
        //
        // Opting in is deliberate (see from_env), but having opted in you must not lose audio
        // to a typo: that is exactly the silent absence this module exists to prevent, and the
        // asymmetry is the point. Only the exact word `off` turns it off.
        let c = with_env(Some("of"), AudioConfig::from_env)
            .expect("a typo must not silently disable requested audio");
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
