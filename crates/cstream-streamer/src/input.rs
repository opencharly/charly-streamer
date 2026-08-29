//! Input injection as upstream GStreamer events.
//!
//! GWD `.expect()`s each field at a SPECIFIC type when it reads the event structure.
//! A wrong type does not degrade — it panics the element's thread and takes the
//! pipeline with it. The types below are therefore part of the wire contract, not a
//! style choice, and the tests assert each one.
//!
//!   pointer_x / pointer_y : f64
//!   x / y                 : f64
//!   button                : u32
//!   key                   : u32
//!   pressed               : bool

/// A pointer motion in absolute compositor coordinates.
pub fn pointer_motion(x: f64, y: f64) -> gst::Structure {
    gst::Structure::builder("wayland.pointer.motion")
        .field("pointer_x", x)
        .field("pointer_y", y)
        .build()
}

/// A pointer button transition.
pub fn pointer_button(button: u32, pressed: bool) -> gst::Structure {
    gst::Structure::builder("wayland.pointer.button")
        .field("button", button)
        .field("pressed", pressed)
        .build()
}

/// A keyboard key transition.
pub fn keyboard_key(key: u32, pressed: bool) -> gst::Structure {
    gst::Structure::builder("wayland.keyboard.key")
        .field("key", key)
        .field("pressed", pressed)
        .build()
}

/// A touch/absolute position event.
pub fn absolute_position(x: f64, y: f64) -> gst::Structure {
    gst::Structure::builder("wayland.pointer.position")
        .field("x", x)
        .field("y", y)
        .build()
}

/// Wrap an input structure as the upstream custom event GWD consumes.
pub fn as_upstream_event(s: gst::Structure) -> gst::Event {
    gst::event::CustomUpstream::new(s)
}
