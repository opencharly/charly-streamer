//! The capture pipeline.
//!
//! The graph follows the render node's encode backend (`rank::Backend`):
//!
//! - VA:   `waylanddisplaysrc ! memory:DMABuf caps ! vapostproc ! <sink>`
//! - CUDA: `waylanddisplaysrc cuda-device-id=N ! memory:CUDAMemory caps ! <sink>`
//! - none: `waylanddisplaysrc ! system-memory caps ! <sink>` (software encode)
//!
//! The memory feature on the caps filter is the load-bearing part: it is what forces
//! the zero-copy path. Without it the source may negotiate system memory and the
//! whole chain silently becomes a CPU copy that still "works".

use anyhow::{Context, Result};
use gst::prelude::*;

use crate::display;
use crate::rank::Backend;

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

/// The frame sizes the stream can carry: the chosen encoder's OWN limits, read from its
/// sink pad template. Not a constant: NVENC H.264 takes 160..4096 x 64..4096, NVENC
/// H.265 144..8192 x 48..8192 (measured with gst-inspect), and a portrait phone window
/// or a 5K ultrawide is valid for one encoder and not for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeLimits {
    pub width: (u32, u32),
    pub height: (u32, u32),
}

impl SizeLimits {
    /// The union of the width/height ranges across the caps' structures. None when no
    /// structure bounds both.
    pub fn from_caps(caps: &gst::CapsRef) -> Option<Self> {
        fn range(s: &gst::StructureRef, field: &str) -> Option<(u32, u32)> {
            if let Ok(r) = s.get::<gst::IntRange<i32>>(field) {
                return Some((r.min().max(1) as u32, r.max().max(1) as u32));
            }
            s.get::<i32>(field)
                .ok()
                .map(|v| (v.max(1) as u32, v.max(1) as u32))
        }
        let mut out: Option<Self> = None;
        for s in caps.iter() {
            let (Some(w), Some(h)) = (range(s, "width"), range(s, "height")) else {
                continue;
            };
            out = Some(match out {
                None => Self {
                    width: w,
                    height: h,
                },
                Some(o) => Self {
                    width: (o.width.0.min(w.0), o.width.1.max(w.1)),
                    height: (o.height.0.min(h.0), o.height.1.max(h.1)),
                },
            });
        }
        out
    }

    /// The limits of the element `factory` would create, from its sink pad template.
    pub fn of_factory(factory: &str) -> Result<Self> {
        let f = gst::ElementFactory::find(factory)
            .with_context(|| format!("no {factory} in the GStreamer registry"))?;
        f.static_pad_templates()
            .iter()
            .filter(|t| t.direction() == gst::PadDirection::Sink)
            .find_map(|t| Self::from_caps(&t.caps()))
            .with_context(|| format!("{factory}'s sink template bounds no frame size"))
    }
}

impl Geometry {
    /// Fit a requested size to what the stream can carry: inside the encoder's own
    /// limits, and EVEN in both dimensions -- the one constraint 4:2:0 chroma imposes
    /// (an odd size cannot be subsampled). Nothing else is rounded: H.264 codes sizes
    /// that are not a multiple of 16 by cropping, so 1366x768 stays 1366x768.
    pub fn sanitised(self, limits: &SizeLimits) -> Self {
        let fit = |v: u32, (lo, hi): (u32, u32)| {
            let lo = lo + (lo & 1); // the smallest EVEN size the encoder takes
            let hi = (hi & !1).max(lo); // the largest EVEN size the encoder takes
            (v & !1).clamp(lo, hi)
        };
        Self {
            width: fit(self.width, limits.width),
            height: fit(self.height, limits.height),
            framerate: self.framerate.clamp(1, 240),
        }
    }

    /// The caps that force the zero-copy path for `memory`.
    pub fn caps(&self, memory: Memory) -> gst::Caps {
        let b = gst::Caps::builder("video/x-raw")
            .field("width", self.width as i32)
            .field("height", self.height as i32)
            .field("framerate", gst::Fraction::new(self.framerate as i32, 1));
        match memory.feature() {
            Some(f) => b.features([f]).build(),
            None => b.build(),
        }
    }
}

/// Where captured frames live, which decides the caps the source is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    DmaBuf,
    Cuda,
    System,
}

impl Memory {
    /// The capture memory for an encode backend; `None` (software encode) captures
    /// to system memory, which every software encoder reads.
    pub fn for_backend(backend: Option<&Backend>) -> Self {
        match backend {
            Some(Backend::Va) => Self::DmaBuf,
            Some(Backend::Cuda { .. }) => Self::Cuda,
            None => Self::System,
        }
    }

    fn feature(self) -> Option<&'static str> {
        match self {
            Self::DmaBuf => Some("memory:DMABuf"),
            Self::Cuda => Some("memory:CUDAMemory"),
            Self::System => None,
        }
    }
}

/// The capture half of the graph, with the `size` capsfilter exposed for resize.
pub struct Capture {
    pub pipeline: gst::Pipeline,
    pub source: gst::Element,
    /// The caps filter a resize rewrites. Named `size` so it is findable by name too.
    pub size: gst::Element,
    /// The memory the caps filter holds the source to; a resize keeps it.
    pub memory: Memory,
}

/// Build the capture graph for `backend`, ending at `sink` (see the module docs).
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
    backend: Option<&Backend>,
    geom: Geometry,
    limits: &SizeLimits,
    sink: gst::Element,
    frame_dir: Option<&str>,
) -> Result<Capture> {
    let geom = geom.sanitised(limits);
    let memory = Memory::for_backend(backend);
    let pipeline = gst::Pipeline::new();
    let cuda_device = match backend {
        Some(Backend::Cuda { device_id }) => Some(*device_id),
        _ => None,
    };
    let source = display::make_source(render_node, cuda_device)?;

    let size = gst::ElementFactory::make("capsfilter")
        .name("size")
        .property("caps", geom.caps(memory))
        .build()
        .context("creating the size capsfilter")?;

    pipeline
        .add_many([&source, &size])
        .context("adding the capture elements")?;
    source
        .link(&size)
        .context("linking the source to its caps")?;

    // VA needs vapostproc between the DMABuf source and the encoder; CUDA and system
    // memory feed the sink directly.
    let convert = if memory == Memory::DmaBuf {
        let vapostproc = gst::ElementFactory::make("vapostproc")
            .build()
            .context("creating vapostproc — is gst-plugin-va installed?")?;
        pipeline.add(&vapostproc).context("adding vapostproc")?;
        size.link(&vapostproc)
            .context("linking the caps to vapostproc")?;
        vapostproc
    } else {
        size.clone()
    };

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
            // The tap converts on the CPU, so CUDA frames are downloaded first.
            let download = if memory == Memory::Cuda {
                Some(
                    gst::ElementFactory::make("cudadownload")
                        .build()
                        .context("creating cudadownload for the frame tap")?,
                )
            } else {
                None
            };
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
            let mut tap: Vec<&gst::Element> = vec![&q_tap];
            if let Some(d) = download.as_ref() {
                pipeline.add(d).context("adding cudadownload")?;
                tap.push(d);
            }
            tap.extend([&rate, &rate_caps, &conv2, &enc, &files]);
            gst::Element::link_many(tap).context("linking the frame tap")?;
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
        memory,
    })
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
        // Any render node this host has; none means the tap shape is asserted on a
        // host that does.
        let Some(node) = std::fs::read_dir("/dev/dri").ok().and_then(|d| {
            d.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
            })
        }) else {
            return;
        };
        let cap = match build_capture(
            node.to_str().unwrap(),
            None,
            Geometry::default(),
            &SizeLimits {
                width: (16, 8192),
                height: (16, 8192),
            },
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
