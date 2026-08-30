//! cstream-streamer — build the capture-to-WebRTC graph and run it.
//!
//! `--probe` exists so a check bed can execute this exact graph-construction path
//! without a browser: it builds the real pipeline, brings it to PLAYING, reports the
//! encoder that was selected, and exits. A probe that merely inspected the registry
//! would pass on a host where the elements exist but refuse to link.

use anyhow::{Context, Result};
use gst::prelude::*;

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
    let frame_dir = std::env::var("CSTREAM_FRAME_DIR").ok();
    let sink = webrtc::make_sink(&encoder)?;
    let capture = build_capture(
        &render_node,
        Geometry::default(),
        sink,
        frame_dir.as_deref(),
    )
    .context("building the capture-to-webrtcsink graph")?;

    capture
        .pipeline
        .set_state(gst::State::Playing)
        .context("bringing the pipeline to PLAYING")?;

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
