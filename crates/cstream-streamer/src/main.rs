//! cstream-streamer — build the capture-to-WebRTC graph and run it.
//!
//! `--probe` exists so a check bed can execute this exact graph-construction path
//! without a browser: it builds the real pipeline, brings it to PLAYING, reports the
//! encoder that was selected, and exits. A probe that merely inspected the registry
//! would pass on a host where the elements exist but refuse to link.

use anyhow::{Context, Result};
use gst::prelude::*;

use cstream_streamer::control::{self, Control};
use cstream_streamer::pipeline::{build_capture, Geometry};
use cstream_streamer::webrtc::{self, EncodePolicy};

/// How long to wait for the pipeline to finish its asynchronous state change.
///
/// A bound, not a sleep: the wait ends the moment the state settles. Without one, a
/// pipeline that never negotiates hangs the probe instead of failing it.
const STATE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(20);

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn main() -> Result<()> {
    let raised = cstream_streamer::init()?;

    let policy = EncodePolicy::parse(&env_or("CSTREAM_ENCODE", "auto"))?;
    let encoder = webrtc::select_encoder(policy, &webrtc::registry_has)?;
    let render_node = env_or("CSTREAM_RENDER_NODE", "/dev/dri/renderD128");

    eprintln!(
        "cstream-streamer: promoted={} policy={policy:?} encoder={}",
        if raised.is_empty() {
            "none".into()
        } else {
            raised.join(",")
        },
        encoder.label()
    );

    let probe = std::env::args().any(|a| a == "--probe");

    // CSTREAM_FRAME_DIR turns on the frame tap. It is how this process takes over
    // what a separate parent pipeline used to do: `waylanddisplaysrc` creates a
    // compositor and holds the render node, so nothing else can observe this
    // display. The desktop nests into THIS one and any frame-content gate reads
    // THIS tap.
    // The compositor cannot create its socket without this, and its failure to do
    // so surfaces as an opaque panic inside the element rather than as a missing
    // directory. See display::prepare_runtime_dir.
    let runtime_dir = env_or("XDG_RUNTIME_DIR", "/tmp/cstream-rt");
    cstream_streamer::display::prepare_runtime_dir(&runtime_dir)?;
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);

    let frame_dir = std::env::var("CSTREAM_FRAME_DIR").ok();
    let sink = webrtc::make_sink(&encoder)?;
    // Clone before handing the sink to build_capture, which takes it by value. A gst::Element
    // is a refcounted GObject handle, so this is the SAME element, not a second sink.
    let audio_sink = sink.clone();
    let capture = build_capture(
        &render_node,
        Geometry::default(),
        sink,
        frame_dir.as_deref(),
    )
    .context("building the capture-to-webrtcsink graph")?;

    // Audio is attached AFTER the video graph exists, onto the same sink, so a failure here
    // names the audio branch instead of surfacing as an opaque failure to build "the graph".
    match cstream_streamer::audio::AudioConfig::from_env() {
        Some(cfg) => {
            cstream_streamer::audio::attach(&capture.pipeline, &audio_sink, &cfg)
                .context("attaching the audio branch")?;
            println!(
                "audio: capturing {} (capture_sink={})",
                cfg.target, cfg.capture_sink
            );
        }
        None => println!("audio: disabled by CSTREAM_AUDIO=off"),
    }

    if let Err(e) = capture.pipeline.set_state(gst::State::Playing) {
        // set_state returns a bare StateChangeError that names nothing. The reason
        // is on the BUS, and without draining it the operator gets "bringing the
        // pipeline to PLAYING" and no way to tell a missing element from a caps
        // negotiation failure from a busy render node. Measured: that bare message
        // cost a debugging cycle.
        let detail = capture
            .pipeline
            .bus()
            .map(|bus| {
                let mut msgs = Vec::new();
                while let Some(m) = bus.pop() {
                    if let gst::MessageView::Error(err) = m.view() {
                        msgs.push(format!(
                            "{}: {}{}",
                            err.src()
                                .map(|s| s.path_string().to_string())
                                .unwrap_or_default(),
                            err.error(),
                            err.debug().map(|d| format!(" [{d}]")).unwrap_or_default()
                        ));
                    }
                }
                msgs.join("; ")
            })
            .unwrap_or_default();
        let _ = capture.pipeline.set_state(gst::State::Null);
        if detail.is_empty() {
            return Err(anyhow::anyhow!("bringing the pipeline to PLAYING: {e}"));
        }
        return Err(anyhow::anyhow!(
            "bringing the pipeline to PLAYING: {detail}"
        ));
    }

    let (change, state, _) = capture.pipeline.state(STATE_TIMEOUT);
    change.context("the pipeline never settled into a state")?;
    if state != gst::State::Playing {
        anyhow::bail!("the pipeline settled at {state:?}, not PLAYING");
    }

    // The marker is what the bed asserts. It is printed only after the pipeline is
    // ACTUALLY playing, so it cannot be produced by a graph that merely built.
    println!("CSTREAM-PIPELINE-PLAYING encoder={}", encoder.label());
    if let Some(dir) = frame_dir.as_deref() {
        println!("CSTREAM-FRAME-TAP dir={dir}");
    }

    // And that the chosen encoder OPENS, not merely that it is registered. Without
    // this the marker above would pass on a host whose VA plugin loads over an
    // unusable render node -- exactly the case `encode: va` exists to catch.
    webrtc::verify_openable(&encoder)?;
    println!("CSTREAM-ENCODER-OPENABLE encoder={}", encoder.label());

    if probe {
        capture.pipeline.set_state(gst::State::Null).ok();
        return Ok(());
    }

    // The control socket is opened only on the SERVING path, after the pipeline is
    // actually PLAYING. Opening it earlier would advertise a control surface for a
    // graph that may still fail to negotiate, and `--probe` must not leave a socket
    // behind for the next run to trip over.
    //
    // It runs on its own thread: the bus loop below blocks with no timeout, so
    // anything sharing that thread would never be served.
    let ctl_path = control::socket_path();
    match control::bind(&ctl_path) {
        Ok(listener) => {
            let ctl = std::sync::Arc::new(std::sync::Mutex::new(Control::new(
                capture.size.clone(),
                Geometry::default(),
            )));
            std::thread::spawn(move || control::serve(listener, ctl));
            // Asserted by the bed: a resize gate that cannot reach the socket should
            // fail on the socket, not time out looking like a broken compositor.
            println!("CSTREAM-CONTROL-LISTENING socket={ctl_path}");
        }
        Err(e) => {
            // Not fatal: a stream that cannot be resized is still a stream, and
            // killing the session over the control channel would be worse than
            // losing it. It is reported loudly so the bed's gate fails with a cause.
            eprintln!("cstream-streamer: control socket unavailable: {e:#}");
        }
    }

    // Serve until the bus reports EOS or an error. `iter_timed` with no timeout
    // blocks, so this is a bus loop, not a poll.
    let bus = capture.pipeline.bus().context("the pipeline has no bus")?;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(e) => {
                capture.pipeline.set_state(gst::State::Null).ok();
                anyhow::bail!(
                    "pipeline error from {:?}: {}",
                    e.src().map(|s| s.path_string()),
                    e.error()
                );
            }
            _ => {}
        }
    }
    capture.pipeline.set_state(gst::State::Null).ok();
    Ok(())
}
