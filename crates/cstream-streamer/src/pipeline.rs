//! The capture pipeline.
//!
//! The graph is `waylanddisplaysrc ! DMABuf caps ! vapostproc ! <encoder>`, and the
//! DMABuf caps filter is the load-bearing part: it is what forces the zero-copy path.
//! Without it the source may negotiate system memory and the whole chain silently
//! becomes a CPU copy that still "works".

use anyhow::{Context, Result};
use gst::prelude::*;

use crate::display;

/// Video geometry for the capture chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            framerate: 60,
        }
    }
}

impl Geometry {
    /// Clamp and round to what the compositor can actually configure.
    ///
    /// Rounding to 8 matters: an odd width reaches the encoder as a size it cannot
    /// represent, and the failure surfaces far from the resize that caused it.
    pub fn sanitised(self) -> Self {
        let w = self.width.clamp(640, 3840) & !7;
        let h = self.height.clamp(480, 2160) & !7;
        Self {
            width: w,
            height: h,
            framerate: self.framerate.clamp(1, 240),
        }
    }

    /// The DMABuf caps that force the zero-copy path.
    pub fn dmabuf_caps(&self) -> gst::Caps {
        gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("width", self.width as i32)
            .field("height", self.height as i32)
            .field("framerate", gst::Fraction::new(self.framerate as i32, 1))
            .build()
    }
}

/// The capture half of the graph, with the `size` capsfilter exposed for resize.
pub struct Capture {
    pub pipeline: gst::Pipeline,
    pub source: gst::Element,
    /// The caps filter a resize rewrites. Named `size` so it is findable by name too.
    pub size: gst::Element,
}

/// Build `waylanddisplaysrc ! DMABuf caps ! vapostproc`, ending at `sink`.
pub fn build_capture(render_node: &str, geom: Geometry, sink: gst::Element) -> Result<Capture> {
    let geom = geom.sanitised();
    let pipeline = gst::Pipeline::new();
    let source = display::make_source(render_node)?;

    let size = gst::ElementFactory::make("capsfilter")
        .name("size")
        .property("caps", geom.dmabuf_caps())
        .build()
        .context("creating the size capsfilter")?;

    let convert = gst::ElementFactory::make("vapostproc")
        .build()
        .context("creating vapostproc — is gstreamer-vaapi/va installed?")?;

    pipeline
        .add_many([&source, &size, &convert, &sink])
        .context("adding elements")?;
    gst::Element::link_many([&source, &size, &convert, &sink])
        .context("linking the capture chain")?;

    Ok(Capture {
        pipeline,
        source,
        size,
    })
}

/// Apply a new geometry to a running capture by rewriting the `size` filter.
pub fn apply_geometry(capture: &Capture, geom: Geometry) -> Geometry {
    let geom = geom.sanitised();
    capture.size.set_property("caps", geom.dmabuf_caps());
    geom
}
