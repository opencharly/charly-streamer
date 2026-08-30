//! Runtime control of a running pipeline.
//!
//! Today this is resize. The surface is a line-oriented Unix socket rather than a
//! resize-shaped entry point, because every other control the architecture lists
//! (stats, volume, clipboard, idle) needs the same seam, and inventing a private
//! one per control is how a stack ends up with four.
//!
//! WHY A SOCKET AND NOT A SIGNAL OR A FILE: a control has to be invocable by a
//! check step running as an ordinary user inside the venue, and it has to be able
//! to REPLY -- a resize that silently clamps is the failure mode this whole
//! package exists to make visible (see `Geometry::sanitised`), so the caller must
//! be told what was actually applied.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;

use crate::pipeline::Geometry;

/// Minimum interval between APPLIED resizes.
///
/// A caps renegotiation tears down and rebuilds the encoder's input, so a client
/// that resizes on every pointer motion during a window drag would spend the whole
/// drag renegotiating and deliver nothing. Two per second is the plan's figure.
const MIN_RESIZE_INTERVAL: Duration = Duration::from_millis(500);

/// Owns the one element a resize rewrites, plus the rate-limit state.
pub struct Control {
    size: gst::Element,
    last_applied: Option<Instant>,
    current: Geometry,
}

impl Control {
    pub fn new(size: gst::Element, initial: Geometry) -> Self {
        Self {
            size,
            last_applied: None,
            current: initial.sanitised(),
        }
    }

    /// The geometry currently on the capsfilter.
    pub fn current(&self) -> Geometry {
        self.current
    }

    /// Apply a resize, returning the geometry ACTUALLY applied.
    ///
    /// The return value is the point of this signature. `sanitised()` clamps to
    /// 640x480..3840x2160 and rounds to a multiple of 8, so a request is routinely
    /// not what lands -- and a caller that assumes otherwise writes a gate that
    /// passes on a compositor which quietly ignored it. The plan records the
    /// concrete instance: Aquamarine snaps an unsatisfiable configure to 1280x720
    /// with no error at all.
    pub fn resize(&mut self, width: u32, height: u32, now: Instant) -> Result<Geometry> {
        let want = Geometry {
            width,
            height,
            framerate: self.current.framerate,
        }
        .sanitised();

        // A no-op resize is not rate-limited: it applies nothing, so it cannot
        // cost a renegotiation, and refusing it would make an idempotent retry
        // look like a failure.
        if want == self.current {
            return Ok(self.current);
        }
        if let Some(last) = self.last_applied {
            let since = now.duration_since(last);
            if since < MIN_RESIZE_INTERVAL {
                anyhow::bail!(
                    "resize rate-limited: {}ms since the last applied resize, minimum {}ms \
                     (current geometry {}x{} is unchanged)",
                    since.as_millis(),
                    MIN_RESIZE_INTERVAL.as_millis(),
                    self.current.width,
                    self.current.height
                );
            }
        }
        self.size.set_property("caps", want.dmabuf_caps());
        self.last_applied = Some(now);
        self.current = want;
        Ok(want)
    }
}

/// Serve the control socket until the listener is dropped.
///
/// Errors on a single connection are reported to that caller and do not take the
/// listener down: one malformed command must not cost the session its only control
/// channel.
pub fn serve(listener: UnixListener, control: Arc<Mutex<Control>>) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle(s, &control) {
                    eprintln!("cstream-control: connection error: {e:#}");
                }
            }
            Err(e) => eprintln!("cstream-control: accept failed: {e:#}"),
        }
    }
}

fn handle(stream: UnixStream, control: &Arc<Mutex<Control>>) -> Result<()> {
    let mut out = stream.try_clone().context("cloning the control stream")?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("reading a control command")?;
        let reply = dispatch(line.trim(), control);
        writeln!(out, "{reply}").context("writing the control reply")?;
        out.flush().ok();
    }
    Ok(())
}

/// One command → one reply line. `OK ...` or `ERR ...`, so a check step can assert
/// on the reply rather than on an exit status that says nothing about what applied.
fn dispatch(line: &str, control: &Arc<Mutex<Control>>) -> String {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("resize") => {
            let w = parts.next().and_then(|v| v.parse::<u32>().ok());
            let h = parts.next().and_then(|v| v.parse::<u32>().ok());
            let (w, h) = match (w, h) {
                (Some(w), Some(h)) => (w, h),
                _ => return "ERR resize needs <width> <height> as positive integers".into(),
            };
            let mut c = match control.lock() {
                Ok(c) => c,
                Err(e) => return format!("ERR control lock poisoned: {e}"),
            };
            match c.resize(w, h, Instant::now()) {
                Ok(g) => format!("OK resize {}x{}", g.width, g.height),
                Err(e) => format!("ERR {e}"),
            }
        }
        Some("geometry") => match control.lock() {
            Ok(c) => {
                let g = c.current();
                format!("OK geometry {}x{}@{}", g.width, g.height, g.framerate)
            }
            Err(e) => format!("ERR control lock poisoned: {e}"),
        },
        Some(other) => format!("ERR unknown command {other:?}"),
        None => "ERR empty command".into(),
    }
}

/// Bind the control socket, removing a stale one first.
///
/// A leftover socket file from a crashed run makes `bind` fail with EADDRINUSE,
/// which reads as "already running" when nothing is.
pub fn bind(path: &str) -> Result<UnixListener> {
    if Path::new(path).exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("removing the stale control socket {path}"))?;
    }
    UnixListener::bind(path).with_context(|| format!("binding the control socket {path}"))
}

/// Where the control socket lives. Overridable so a second instance in a test does
/// not collide with the service's.
pub fn socket_path() -> String {
    std::env::var("CSTREAM_CONTROL_SOCKET")
        .unwrap_or_else(|_| "/tmp/cstream-control.sock".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_geometry(el: &gst::Element) -> (i32, i32) {
        let caps: gst::Caps = el.property("caps");
        let s = caps.structure(0).expect("caps have a structure");
        (
            s.get::<i32>("width").unwrap(),
            s.get::<i32>("height").unwrap(),
        )
    }

    fn fixture() -> (Control, gst::Element) {
        gst::init().unwrap();
        let start = Geometry {
            width: 1920,
            height: 1080,
            framerate: 60,
        };
        let size = gst::ElementFactory::make("capsfilter")
            .property("caps", start.dmabuf_caps())
            .build()
            .unwrap();
        (Control::new(size.clone(), start), size)
    }

    // 1600x900 is deliberately the example: 900 is NOT a multiple of 8, so the
    // applied height is 896. A gate that asserts the REQUESTED 900 fails against
    // correct behaviour -- which is why `resize` returns what applied and why the
    // bed asserts that value rather than the request.
    #[test]
    fn a_resize_rewrites_the_capsfilter_to_the_sanitised_size() {
        let (mut c, size) = fixture();
        let g = c.resize(1600, 900, Instant::now()).unwrap();
        assert_eq!((g.width, g.height), (1600, 896), "900 rounds down to 896");
        assert_eq!(caps_geometry(&size), (1600, 896));
    }

    // The clamp and the round are not cosmetic: an odd width reaches the encoder as
    // a size it cannot represent, and the failure surfaces far from the resize.
    #[test]
    fn an_out_of_range_request_is_clamped_and_the_caller_is_told_what_applied() {
        let (mut c, size) = fixture();
        let g = c.resize(99999, 99999, Instant::now()).unwrap();
        assert_eq!((g.width, g.height), (3840, 2160));
        assert_eq!(caps_geometry(&size), (3840, 2160));

        let (mut c2, size2) = fixture();
        let g2 = c2.resize(1601, 903, Instant::now()).unwrap();
        assert_eq!(
            (g2.width, g2.height),
            (1600, 896),
            "rounded down to a multiple of 8"
        );
        assert_eq!(caps_geometry(&size2), (1600, 896));
    }

    #[test]
    fn a_second_resize_inside_the_window_is_refused_and_leaves_the_caps_alone() {
        let (mut c, size) = fixture();
        let t0 = Instant::now();
        c.resize(1600, 900, t0).unwrap();
        let err = c
            .resize(1280, 720, t0 + Duration::from_millis(100))
            .unwrap_err();
        assert!(format!("{err}").contains("rate-limited"), "got: {err}");
        assert_eq!(
            caps_geometry(&size),
            (1600, 896),
            "the refused resize must not apply"
        );
    }

    #[test]
    fn the_window_reopens_after_the_interval() {
        let (mut c, size) = fixture();
        let t0 = Instant::now();
        c.resize(1600, 900, t0).unwrap();
        let g = c.resize(1280, 720, t0 + MIN_RESIZE_INTERVAL).unwrap();
        assert_eq!((g.width, g.height), (1280, 720));
        assert_eq!(caps_geometry(&size), (1280, 720));
    }

    // An idempotent retry must not read as a failure just because it arrived fast.
    #[test]
    fn a_no_op_resize_is_not_rate_limited() {
        let (mut c, _size) = fixture();
        let t0 = Instant::now();
        c.resize(1600, 900, t0).unwrap();
        let g = c.resize(1600, 900, t0 + Duration::from_millis(1)).unwrap();
        assert_eq!((g.width, g.height), (1600, 896));
    }

    #[test]
    fn the_reply_names_what_applied_not_what_was_asked() {
        gst::init().unwrap();
        let (c, _size) = fixture();
        let ctl = Arc::new(Mutex::new(c));
        assert_eq!(dispatch("resize 1601 903", &ctl), "OK resize 1600x896");
        assert!(dispatch("resize", &ctl).starts_with("ERR resize needs"));
        assert!(dispatch("wibble", &ctl).starts_with("ERR unknown command"));
        assert!(dispatch("geometry", &ctl).starts_with("OK geometry 1600x896@"));
    }
}
