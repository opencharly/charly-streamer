//! cstream-input — drive the streamed desktop the way a browser does.
//!
//! It joins the stream as a consumer (webrtcsrc against the streamer's loopback
//! signalling server) and sends input over the producer's `control` data channel, as
//! the SAME JSON the browser player sends. The whole producer side of input is
//! therefore exercised exactly as for a real viewer: webrtcsink parses the request,
//! pushes a GstNavigation event upstream, and waylanddisplaysrc presses the key on its
//! seat.
//!
//! Delivery is confirmed, not assumed. webrtcsink answers every control request with
//! `{"id":N,"error":null}` only AFTER it has pushed the event to the source, so this
//! tool waits for one answer per event and exits 0 only when all of them arrived
//! error-free. No fixed delay is involved.
//!
//! Usage: `cstream-input <action>...` where each action is one of
//!   `type=<text>`  `key=<keysym>`  `move=<x>,<y>`  `click=<x>,<y>[,<button>]`
//!   `scroll=<x>,<y>,<dx>,<dy>` (deltas: 120 per wheel notch, positive = up/left -- GStreamer
//!   sink convention; the GWD element maps them onto the seat's wheel axis)
//! Signalling: `CSTREAM_SIGNALLING_URI`, else `ws://127.0.0.1:$CSTREAM_SIGNALLING_PORT`
//! (default port 8443).

use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use gst_webrtc::{WebRTCDataChannel, WebRTCDataChannelState, WebRTCKind, WebRTCRTPTransceiver};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cstream_streamer::keys::{steps_for_text, KeyStep};

/// The whole exchange -- join, negotiate, send, collect every answer -- must finish
/// within this bound; a stream that never offers a control channel fails here
/// instead of hanging the check that called it.
const DEADLINE: Duration = Duration::from_secs(30);

/// The label webrtcsink gives its control data channel.
const CONTROL_LABEL: &str = "control";

fn nav(event: &str, fields: Value) -> Value {
    let mut v = fields;
    v["event"] = json!(event);
    // Required by webrtcsink's NavigationEvent: an event without it is dropped.
    v["modifier_state"] = json!("");
    v
}

fn parse_floats(s: &str, n: usize) -> Result<Vec<f64>> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("expected {n} numbers in {s:?}"))?;
    if v.len() < n {
        bail!("expected {n} numbers in {s:?}");
    }
    Ok(v)
}

/// Turn the command-line actions into the navigation events the player would send.
fn events_for(args: &[String]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for a in args {
        let (verb, arg) = a
            .split_once('=')
            .ok_or_else(|| anyhow!("action {a:?} is not <verb>=<argument>"))?;
        match verb {
            "type" => {
                let steps =
                    steps_for_text(arg).map_err(|c| anyhow!("{c:?} has no key on a US layout"))?;
                for s in steps {
                    out.push(match s {
                        KeyStep::Press(k) => nav("KeyPress", json!({ "key": k })),
                        KeyStep::Release(k) => nav("KeyRelease", json!({ "key": k })),
                    });
                }
            }
            "key" => {
                out.push(nav("KeyPress", json!({ "key": arg })));
                out.push(nav("KeyRelease", json!({ "key": arg })));
            }
            "move" => {
                let p = parse_floats(arg, 2)?;
                out.push(nav("MouseMove", json!({ "x": p[0], "y": p[1] })));
            }
            "click" => {
                let p = parse_floats(arg, 2)?;
                let button = p.get(2).copied().unwrap_or(1.0) as i32;
                let at = json!({ "x": p[0], "y": p[1] });
                out.push(nav("MouseMove", at.clone()));
                let mut b = at.clone();
                b["button"] = json!(button);
                out.push(nav("MouseButtonPress", b.clone()));
                out.push(nav("MouseButtonRelease", b));
            }
            "scroll" => {
                let p = parse_floats(arg, 4)?;
                out.push(nav(
                    "MouseScroll",
                    json!({ "x": p[0], "y": p[1], "delta_x": p[2], "delta_y": p[3] }),
                ));
            }
            other => bail!("unknown action {other:?} (type, key, move, click, scroll)"),
        }
    }
    if out.is_empty() {
        bail!("no actions given");
    }
    Ok(out)
}

enum Note {
    Channel(WebRTCDataChannel),
    Open,
    Reply(String),
    Webrtcbin(gst::Element),
}

/// The negotiated VIDEO stream's mid. Input is addressed to it: with a null mid webrtcsink
/// pushes the event into every input stream (audio too) and answers "no error" if ANY of
/// them took it -- measured, the event reached pipewiresrc as well as waylanddisplaysrc --
/// so the answer would not prove the desktop received anything. Addressed to the video
/// mid, webrtcsink pushes into the video stream only, and its answer is
/// waylanddisplaysrc's own verdict on the event.
fn video_mid(webrtcbin: &gst::Element) -> Option<String> {
    (0i32..)
        .map_while(|i| {
            webrtcbin.emit_by_name::<Option<WebRTCRTPTransceiver>>("get-transceiver", &[&i])
        })
        .find(|t| t.property::<WebRTCKind>("kind") == WebRTCKind::Video)
        .and_then(|t| t.mid().map(|m| m.to_string()))
}

fn signalling_uri() -> String {
    std::env::var("CSTREAM_SIGNALLING_URI").unwrap_or_else(|_| {
        let port = std::env::var("CSTREAM_SIGNALLING_PORT").unwrap_or_else(|_| "8443".into());
        format!("ws://127.0.0.1:{port}")
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let events = events_for(&args)?;
    gst::init()?;

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("webrtcsrc")
        .property("connect-to-first-producer", true)
        .build()
        .context("creating webrtcsrc — is gst-plugin-rswebrtc installed?")?;
    src.property::<gst::glib::Object>("signaller")
        .set_property("uri", signalling_uri());
    pipeline.add(&src)?;

    // Every media pad is consumed and discarded: the stream is joined for its control
    // channel, and an unlinked pad would stop the session with not-negotiated.
    let pl = pipeline.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(pl) = pl.upgrade() else { return };
        let Ok(sink) = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
        else {
            return;
        };
        if pl.add(&sink).is_ok() {
            let _ = sink.sync_state_with_parent();
            let _ = pad.link(&sink.static_pad("sink").unwrap());
        }
    });

    let (tx, rx) = mpsc::channel::<Note>();
    let bin = src
        .clone()
        .downcast::<gst::Bin>()
        .map_err(|_| anyhow!("webrtcsrc is not a bin"))?;
    bin.connect_deep_element_added(move |_, _, element| {
        if element
            .factory()
            .map(|f| f.name() != "webrtcbin")
            .unwrap_or(true)
        {
            return;
        }
        let _ = tx.send(Note::Webrtcbin(element.clone()));
        let tx = tx.clone();
        element.connect("on-data-channel", false, move |values| {
            let ch = values[1].get::<WebRTCDataChannel>().ok()?;
            if ch.label().as_deref() != Some(CONTROL_LABEL) {
                return None;
            }
            let t = tx.clone();
            ch.connect_on_open(move |_| {
                let _ = t.send(Note::Open);
            });
            let t = tx.clone();
            ch.connect_on_message_string(move |_, msg| {
                if let Some(m) = msg {
                    let _ = t.send(Note::Reply(m.to_string()));
                }
            });
            let open = ch.ready_state() == WebRTCDataChannelState::Open;
            let _ = tx.send(Note::Channel(ch));
            if open {
                let _ = tx.send(Note::Open);
            }
            None
        });
    });

    pipeline.set_state(gst::State::Playing)?;
    let result = drive(&pipeline, &rx, &events);
    let _ = pipeline.set_state(gst::State::Null);
    let n = result?;
    println!("CSTREAM-INPUT-DELIVERED events={n}");
    Ok(())
}

/// Wait for the control channel, send every event, and collect one answer per event.
fn drive(pipeline: &gst::Pipeline, rx: &mpsc::Receiver<Note>, events: &[Value]) -> Result<usize> {
    let deadline = Instant::now() + DEADLINE;
    let bus = pipeline.bus().context("the pipeline has no bus")?;
    let mut channel: Option<WebRTCDataChannel> = None;
    let mut webrtcbin: Option<gst::Element> = None;
    let mut open = false;
    let mut sent = 0usize;
    let mut answered = std::collections::BTreeSet::new();

    loop {
        // A pipeline error (no producer, refused signalling) ends the run with its cause.
        while let Some(m) = bus.pop() {
            if let gst::MessageView::Error(e) = m.view() {
                bail!(
                    "{}: {}",
                    e.src()
                        .map(|s| s.path_string().to_string())
                        .unwrap_or_default(),
                    e.error()
                );
            }
        }
        if let (Some(ch), true, 0) = (channel.as_ref(), open, sent) {
            let mid = webrtcbin
                .as_ref()
                .and_then(video_mid)
                .ok_or_else(|| anyhow!("the stream negotiated no video transceiver with a mid"))?;
            for (i, ev) in events.iter().enumerate() {
                let msg = json!({ "id": i + 1, "mid": mid, "request": { "type": "navigationEvent", "event": ev } });
                ch.send_string_full(Some(&msg.to_string()))
                    .with_context(|| format!("sending event {}", i + 1))?;
            }
            sent = events.len();
        }
        if sent > 0 && answered.len() == sent {
            return Ok(sent);
        }
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                anyhow!(
                    "timed out after {DEADLINE:?}: control channel {}, open={open}, sent={sent}, answered={}",
                    if channel.is_some() { "found" } else { "never offered" },
                    answered.len()
                )
            })?;
        // Bus errors are re-checked at least every 200 ms while waiting.
        match rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(Note::Channel(ch)) => channel = Some(ch),
            Ok(Note::Webrtcbin(b)) => webrtcbin = Some(b),
            Ok(Note::Open) => open = true,
            Ok(Note::Reply(r)) => {
                let v: Value =
                    serde_json::from_str(&r).with_context(|| format!("bad reply {r:?}"))?;
                if let Some(err) = v.get("error").and_then(Value::as_str) {
                    bail!("the producer refused event {}: {err}", v["id"]);
                }
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    answered.insert(id);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the stream closed before every event was answered")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// Every event this tool sends deserializes into the NavigationEvent webrtcsink
    /// parses -- the same contract the player is held to in tests/contract.rs.
    #[test]
    fn every_event_is_a_valid_navigation_event() {
        gst::init().unwrap();
        let evs = events_for(&s(&[
            "type=Hi!",
            "key=Return",
            "move=1,2",
            "click=3,4,3",
            "scroll=1,2,0,-120",
        ]))
        .unwrap();
        for e in &evs {
            let ok: std::result::Result<gst_video::NavigationEvent, _> =
                serde_json::from_value(e.clone());
            assert!(ok.is_ok(), "{e} would be dropped: {:?}", ok.err());
        }
        // H (shift + h, 4 events), i (2), ! (shift + 1, 4), Return (2), move (1),
        // click (3), scroll (1)
        assert_eq!(evs.len(), 4 + 2 + 4 + 2 + 1 + 3 + 1);
    }

    #[test]
    fn bad_actions_are_errors() {
        assert!(events_for(&s(&[])).is_err());
        assert!(events_for(&s(&["type"])).is_err());
        assert!(events_for(&s(&["jump=1"])).is_err());
        assert!(events_for(&s(&["move=1"])).is_err());
        assert!(events_for(&s(&["type=€"])).is_err());
    }
}
