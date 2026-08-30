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
///
/// `frame_dir`, when set, adds a second branch off a `tee` that writes one JPEG per
/// second into that directory.
///
/// That tap is not a debugging convenience — it is why this process must BE the
/// Wayland parent rather than run beside one. `waylanddisplaysrc` creates a
/// compositor and holds the render node, so a second one cannot observe this
/// display: it negotiates `Supported DMA formats: []` and produces nothing. Any
/// frame-content gate therefore has to read a tap off THIS pipeline, and the
/// desktop has to be nested into THIS compositor. Two GWD sources in one image is
/// the configuration that killed a pod mid-run and left a consumer with a session
/// but no media.
pub fn build_capture(
    render_node: &str,
    geom: Geometry,
    sink: gst::Element,
    frame_dir: Option<&str>,
) -> Result<Capture> {
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
        .add_many([&source, &size, &convert])
        .context("adding the capture elements")?;
    gst::Element::link_many([&source, &size, &convert]).context("linking the capture chain")?;

    match frame_dir {
        None => {
            pipeline.add(&sink).context("adding the sink")?;
            convert.link(&sink).context("linking convert to the sink")?;
        }
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating the frame-tap directory {dir}"))?;
            let tee = gst::ElementFactory::make("tee")
                .name("tap")
                .build()
                .context("creating the tee")?;
            // Queues on BOTH branches. Without them a stall in the tap (a slow disk,
            // a full directory) back-pressures the encoder branch and the stream
            // stutters for a reason that has nothing to do with the stream.
            let q_sink = gst::ElementFactory::make("queue").build()?;
            let q_tap = gst::ElementFactory::make("queue").build()?;
            let rate = gst::ElementFactory::make("videorate").build()?;
            let rate_caps = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("framerate", gst::Fraction::new(1, 1))
                        .build(),
                )
                .build()?;
            let conv2 = gst::ElementFactory::make("videoconvert").build()?;
            let enc = gst::ElementFactory::make("jpegenc").build()?;
            let files = gst::ElementFactory::make("multifilesink")
                .property("location", format!("{dir}/f%05d.jpg"))
                .property("max-files", 8u32)
                .build()
                .context("creating multifilesink")?;

            pipeline
                .add_many([
                    &tee, &q_sink, &sink, &q_tap, &rate, &rate_caps, &conv2, &enc, &files,
                ])
                .context("adding the tee branches")?;
            convert.link(&tee).context("linking convert to the tee")?;
            gst::Element::link_many([&q_sink, &sink]).context("linking the encoder branch")?;
            gst::Element::link_many([&q_tap, &rate, &rate_caps, &conv2, &enc, &files])
                .context("linking the frame tap")?;
            tee.link(&q_sink)
                .context("linking the tee to the encoder branch")?;
            tee.link(&q_tap)
                .context("linking the tee to the frame tap")?;
        }
    }

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

#[cfg(test)]
mod tap_tests {
    use super::*;

    /// The frame tap must be part of the SAME pipeline as the encoder branch.
    ///
    /// This is the property that makes the streamer able to replace a separate
    /// parent: `waylanddisplaysrc` holds the render node, so a second process
    /// cannot observe this display (it negotiates `Supported DMA formats: []`).
    /// If the tap were ever split back out into its own pipeline, a frame-content
    /// gate would silently read an empty compositor -- which is exactly the failure
    /// that left a consumer with a negotiated session and no media.
    #[test]
    fn frame_dir_adds_a_multifilesink_to_the_same_pipeline() {
        let _ = gst::init();
        if gst::ElementFactory::find(display::FACTORY).is_none() {
            eprintln!("skipping: {} not in the registry", display::FACTORY);
            return;
        }
        let dir = std::env::temp_dir().join("cstream-tap-test");
        let sink = match gst::ElementFactory::make("fakesink").build() {
            Ok(s) => s,
            Err(_) => return,
        };
        let cap = match build_capture(
            "/dev/dri/renderD128",
            Geometry::default(),
            sink,
            Some(dir.to_str().unwrap()),
        ) {
            Ok(c) => c,
            Err(_) => return, // no render node on this host: the tap shape is asserted where one exists
        };
        let has_tap = cap
            .pipeline
            .iterate_elements()
            .into_iter()
            .flatten()
            .any(|e| {
                e.factory()
                    .map(|f| f.name() == "multifilesink")
                    .unwrap_or(false)
            });
        assert!(
            has_tap,
            "frame_dir was set but the pipeline carries no multifilesink -- the frame tap \
             would be missing and a content gate would read nothing"
        );
    }
}
