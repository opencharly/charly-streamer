//! Contract tests for the three things that fail SILENTLY if they regress.

use cstream_streamer::{display, input, rank};
use gst::prelude::*;

fn init() {
    // gst::init is idempotent and safe to call from every test.
    gst::init().expect("gst init");
}

// ---------------------------------------------------------------------------
// Input field types. GWD `.expect()`s each field at a specific type; a mismatch
// panics the element thread rather than degrading, so the types ARE the contract.
// ---------------------------------------------------------------------------

#[test]
fn pointer_motion_fields_are_f64() {
    init();
    let s = input::pointer_motion(12.5, 7.25);
    assert_eq!(s.get::<f64>("pointer_x").unwrap(), 12.5);
    assert_eq!(s.get::<f64>("pointer_y").unwrap(), 7.25);
    // The wrong type must NOT silently coerce.
    assert!(
        s.get::<i32>("pointer_x").is_err(),
        "pointer_x must be f64, not an integer"
    );
}

#[test]
fn absolute_position_fields_are_f64() {
    init();
    let s = input::absolute_position(1.0, 2.0);
    assert_eq!(s.get::<f64>("x").unwrap(), 1.0);
    assert_eq!(s.get::<f64>("y").unwrap(), 2.0);
    assert!(s.get::<i32>("x").is_err(), "x must be f64");
}

#[test]
fn button_is_u32_and_pressed_is_bool() {
    init();
    let s = input::pointer_button(272, true);
    assert_eq!(s.get::<u32>("button").unwrap(), 272);
    assert!(s.get::<bool>("pressed").unwrap());
    assert!(
        s.get::<f64>("button").is_err(),
        "button must be u32, not f64"
    );
    assert!(
        s.get::<u32>("pressed").is_err(),
        "pressed must be bool, not u32"
    );
}

#[test]
fn key_is_u32() {
    init();
    let s = input::keyboard_key(30, false);
    assert_eq!(s.get::<u32>("key").unwrap(), 30);
    assert!(!s.get::<bool>("pressed").unwrap());
    assert!(s.get::<f64>("key").is_err(), "key must be u32");
}

#[test]
fn input_rides_as_an_upstream_event() {
    init();
    let ev = input::as_upstream_event(input::pointer_motion(1.0, 1.0));
    assert!(
        ev.is_upstream(),
        "input must travel UPSTREAM to reach the source"
    );
}

// ---------------------------------------------------------------------------
// Render node. A software node cannot host a nested compositor, so an empty
// node is a configuration error and must fail loudly rather than degrade.
// ---------------------------------------------------------------------------

#[test]
fn empty_render_node_is_rejected_with_a_named_reason() {
    init();
    let err = display::make_source("").expect_err("an empty render node must be rejected");
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
    if !rank::has_hardware_h264() {
        eprintln!("skipping: no VA-API H.264 encoder on this host");
        return;
    }
    assert!(
        rank::ranking_is_still_pending(),
        "vah264enc must start BELOW webrtcsink's MARGINAL floor — that is why promotion is needed"
    );
    let raised = rank::raise_hardware_encoders();
    assert!(raised.contains(&"vah264enc".to_string()));
    assert!(
        !rank::ranking_is_still_pending(),
        "after promotion the encoder must clear the floor"
    );
    let f = gst::ElementFactory::find("vah264enc").unwrap();
    assert!(
        f.rank() > gst::Rank::PRIMARY,
        "must outrank software encoders, not merely clear MARGINAL"
    );
}
