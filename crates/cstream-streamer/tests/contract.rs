//! Contract tests for the three things that fail SILENTLY if they regress.

use cstream_streamer::{display, rank};
use gst::prelude::*;

fn init() {
    // gst::init is idempotent and safe to call from every test.
    gst::init().expect("gst init");
}

// ---------------------------------------------------------------------------
// Input. The player (cmd/cstream-gateway/web/player.js) sends GstNavigation events
// over webrtcsink's "control" data channel; webrtcsink deserializes each one into
// gst_video::NavigationEvent and pushes it upstream to waylanddisplaysrc. A message
// that does not deserialize is logged by webrtcsink and DROPPED -- the viewer's
// input silently vanishes -- so the exact JSON shapes the player emits are the
// contract, asserted here against the real type.
// ---------------------------------------------------------------------------

/// One message per shape player.js sends, written exactly as it writes them.
const PLAYER_EVENTS: &[&str] = &[
    r#"{"event":"KeyPress","key":"a","modifier_state":""}"#,
    r#"{"event":"KeyRelease","key":"Return","modifier_state":"shift-mask"}"#,
    r#"{"event":"KeyPress","key":"Shift_L","modifier_state":"shift-mask+control-mask+mod1-mask+super-mask"}"#,
    r#"{"event":"MouseMove","x":10.5,"y":20.25,"modifier_state":""}"#,
    r#"{"event":"MouseButtonPress","button":1,"x":1,"y":2,"modifier_state":""}"#,
    r#"{"event":"MouseButtonRelease","button":3,"x":1,"y":2,"modifier_state":""}"#,
    r#"{"event":"MouseScroll","x":1,"y":2,"delta_x":0,"delta_y":-120,"modifier_state":""}"#,
];

#[test]
fn every_player_event_shape_deserializes_into_a_navigation_event() {
    init();
    for msg in PLAYER_EVENTS {
        let ev: Result<gst_video::NavigationEvent, _> = serde_json::from_str(msg);
        assert!(ev.is_ok(), "webrtcsink would DROP {msg}: {:?}", ev.err());
    }
}

/// modifier_state is required: an event without it is dropped, which is why the
/// player always sends the field, even when no modifier is held.
#[test]
fn an_event_without_modifier_state_is_rejected() {
    init();
    let ev: Result<gst_video::NavigationEvent, _> =
        serde_json::from_str(r#"{"event":"KeyPress","key":"a"}"#);
    assert!(
        ev.is_err(),
        "the field is required; the player must always send it"
    );
}

/// The control-channel envelope: webrtcsink parses `request` as a tagged
/// `navigationEvent` whose `event` is the NavigationEvent above.
#[test]
fn the_control_envelope_carries_the_event() {
    let msg = r#"{"id":1,"mid":null,"request":{"type":"navigationEvent","event":{"event":"KeyPress","key":"a","modifier_state":""}}}"#;
    let v: serde_json::Value = serde_json::from_str(msg).unwrap();
    assert_eq!(v["request"]["type"], "navigationEvent");
    init();
    let ev: gst_video::NavigationEvent =
        serde_json::from_value(v["request"]["event"].clone()).unwrap();
    assert!(matches!(ev, gst_video::NavigationEvent::KeyPress { .. }));
}

// ---------------------------------------------------------------------------
// Render node. A software node cannot host a nested compositor, so an empty
// node is a configuration error and must fail loudly rather than degrade.
// ---------------------------------------------------------------------------

#[test]
fn empty_render_node_is_rejected_with_a_named_reason() {
    init();
    let err = display::make_source("", None).expect_err("an empty render node must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("render node"),
        "the error must name the cause: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Bus message parsing. The `wayland.src` message is BEST EFFORT — a post failure
// is only a warning upstream — so the parser must ignore everything else cleanly.
// ---------------------------------------------------------------------------

#[test]
fn wayland_display_is_read_from_the_right_message_only() {
    init();
    let wanted = gst::message::Application::new(
        gst::Structure::builder("wayland.src")
            .field("WAYLAND_DISPLAY", "wayland-1")
            .build(),
    );
    assert_eq!(
        display::wayland_display_from_message(&wanted).as_deref(),
        Some("wayland-1")
    );

    let unrelated = gst::message::Application::new(
        gst::Structure::builder("some.other.message")
            .field("WAYLAND_DISPLAY", "wayland-9")
            .build(),
    );
    assert_eq!(
        display::wayland_display_from_message(&unrelated),
        None,
        "only the wayland.src message may be trusted for the display name"
    );
}

// ---------------------------------------------------------------------------
// Encoder ranking. webrtcsink enumerates codecs ONCE into a LazyLock, so a late
// promotion is silently ineffective and the stream runs on the CPU.
// ---------------------------------------------------------------------------

#[test]
fn hardware_encoders_are_promoted_above_the_marginal_floor() {
    init();
    let backend = rank::Backend::Va;
    if gst::ElementFactory::find("vah264enc").is_none() {
        eprintln!("skipping: no VA-API H.264 encoder on this host");
        return;
    }
    assert!(
        rank::ranking_is_still_pending(&backend),
        "vah264enc must start BELOW webrtcsink's MARGINAL floor — that is why promotion is needed"
    );
    let raised = rank::raise_hardware_encoders(Some(&backend));
    assert!(raised.contains(&"vah264enc".to_string()));
    assert!(
        !rank::ranking_is_still_pending(&backend),
        "after promotion the encoder must clear the floor"
    );
    let f = gst::ElementFactory::find("vah264enc").unwrap();
    assert!(
        f.rank() > gst::Rank::PRIMARY,
        "must outrank software encoders, not merely clear MARGINAL"
    );
}
