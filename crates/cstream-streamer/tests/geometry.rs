//! Geometry sanitising — the resize path's silent failures.
//!
//! GWD patch 1 fixed a resize that SNAPPED to 1280x720 with no error when the
//! requested size could not be honoured. Asserting "no error" therefore proves
//! nothing about a resize; the requested size has to be checked directly, and the
//! values that reach the caps have to be ones the encoder can represent.

use cstream_streamer::pipeline::Geometry;

#[test]
fn odd_sizes_round_to_eight() {
    let g = Geometry { width: 1919, height: 1079, framerate: 60 }.sanitised();
    assert_eq!(g.width % 8, 0, "width must round to 8: {}", g.width);
    assert_eq!(g.height % 8, 0, "height must round to 8: {}", g.height);
    assert_eq!((g.width, g.height), (1912, 1072));
}

#[test]
fn sizes_clamp_to_the_supported_range() {
    let small = Geometry { width: 1, height: 1, framerate: 60 }.sanitised();
    assert_eq!((small.width, small.height), (640, 480));

    let huge = Geometry { width: 99999, height: 99999, framerate: 1000 }.sanitised();
    assert_eq!((huge.width, huge.height), (3840, 2160));
    assert_eq!(huge.framerate, 240);
}

#[test]
fn the_default_is_the_spec_geometry() {
    let g = Geometry::default().sanitised();
    assert_eq!((g.width, g.height, g.framerate), (1920, 1080, 60));
}

#[test]
fn caps_carry_the_dmabuf_feature_which_forces_zero_copy() {
    gst::init().unwrap();
    let caps = Geometry::default().dmabuf_caps();
    let s = caps.structure(0).unwrap();
    assert_eq!(s.name(), "video/x-raw");
    assert_eq!(s.get::<i32>("width").unwrap(), 1920);
    assert!(
        caps.features(0).unwrap().contains("memory:DMABuf"),
        "without the DMABuf feature the chain silently becomes a CPU copy that still works"
    );
}
