//! Geometry sanitising — the resize path's silent failures.
//!
//! GWD patch 1 fixed a resize that SNAPPED to 1280x720 with no error when the
//! requested size could not be honoured. Asserting "no error" therefore proves
//! nothing about a resize; the requested size has to be checked directly, and the
//! values that reach the caps have to be ones the chosen encoder can represent --
//! its own limits, read from its sink template, not a constant.

use cstream_streamer::pipeline::{Geometry, Memory, SizeLimits};

/// NVENC H.264's measured sink-template limits (gst-inspect-1.0 nvh264enc).
const NVENC_H264: SizeLimits = SizeLimits {
    width: (160, 4096),
    height: (64, 4096),
};

fn fit(width: u32, height: u32, limits: &SizeLimits) -> (u32, u32) {
    let g = Geometry {
        width,
        height,
        framerate: 60,
    }
    .sanitised(limits);
    (g.width, g.height)
}

/// Every even size inside the encoder's limits applies EXACTLY -- real browser windows
/// are 1366x768, portrait 390x844, ultrawide 2560x1080 -- because the only constraint
/// below the encoder is 4:2:0's even dimensions; H.264 codes the rest by cropping.
#[test]
fn even_sizes_inside_the_limits_apply_exactly() {
    for (w, h) in [
        (1920, 1080),
        (1366, 768),
        (390, 844),
        (2560, 1080),
        (1600, 900),
    ] {
        assert_eq!(fit(w, h, &NVENC_H264), (w, h));
    }
}

#[test]
fn odd_sizes_drop_to_even() {
    assert_eq!(fit(1919, 1079, &NVENC_H264), (1918, 1078));
}

/// The limits are the encoder's own, so they differ per encoder, and an odd minimum
/// rounds UP to the smallest even size the encoder takes.
#[test]
fn sizes_clamp_to_the_encoders_own_limits() {
    assert_eq!(fit(1, 1, &NVENC_H264), (160, 64));
    assert_eq!(fit(99999, 99999, &NVENC_H264), (4096, 4096));
    let nvenc_h265 = SizeLimits {
        width: (144, 8192),
        height: (48, 8192),
    };
    assert_eq!(
        fit(7680, 4320, &nvenc_h265),
        (7680, 4320),
        "8K fits H.265, not H.264"
    );
    let odd_min = SizeLimits {
        width: (145, 4096),
        height: (33, 4096),
    };
    assert_eq!(fit(1, 1, &odd_min), (146, 34));
    let huge = Geometry {
        width: 1920,
        height: 1080,
        framerate: 1000,
    }
    .sanitised(&NVENC_H264);
    assert_eq!(huge.framerate, 240);
}

#[test]
fn the_default_is_the_spec_geometry() {
    let g = Geometry::default().sanitised(&NVENC_H264);
    assert_eq!((g.width, g.height, g.framerate), (1920, 1080, 60));
}

/// Limits are read from an encoder's sink template: the union across its structures,
/// and nothing when the caps bound no size.
#[test]
fn limits_are_read_from_caps_ranges() {
    gst::init().unwrap();
    let caps: gst::Caps = "video/x-raw, width=(int)[ 160, 4096 ], height=(int)[ 64, 4096 ]; \
                           video/x-raw(memory:CUDAMemory), width=(int)[ 144, 8192 ], height=(int)[ 48, 8192 ]"
        .parse()
        .unwrap();
    assert_eq!(
        SizeLimits::from_caps(&caps),
        Some(SizeLimits {
            width: (144, 8192),
            height: (48, 8192)
        })
    );
    let unbounded: gst::Caps = "video/x-raw, format=(string)NV12".parse().unwrap();
    assert_eq!(SizeLimits::from_caps(&unbounded), None);
}

/// Each capture memory puts its feature on the caps -- that feature is what forces
/// the zero-copy path; without it the chain silently becomes a CPU copy that still
/// works. System memory (software encode) carries none.
#[test]
fn caps_carry_the_memory_feature_which_forces_zero_copy() {
    gst::init().unwrap();
    for (memory, feature) in [
        (Memory::DmaBuf, Some("memory:DMABuf")),
        (Memory::Cuda, Some("memory:CUDAMemory")),
        (Memory::System, None),
    ] {
        let caps = Geometry::default().caps(memory);
        let s = caps.structure(0).unwrap();
        assert_eq!(s.name(), "video/x-raw");
        assert_eq!(s.get::<i32>("width").unwrap(), 1920);
        let f = caps.features(0).unwrap();
        match feature {
            Some(want) => assert!(f.contains(want), "{memory:?} caps lack {want}"),
            None => assert!(
                f.is_any() || f.contains("memory:SystemMemory") || f.size() == 0,
                "{memory:?} caps carry {f:?}"
            ),
        }
    }
}

#[test]
fn the_backend_decides_the_capture_memory() {
    use cstream_streamer::rank::Backend;
    assert_eq!(Memory::for_backend(Some(&Backend::Va)), Memory::DmaBuf);
    assert_eq!(
        Memory::for_backend(Some(&Backend::Cuda { device_id: 0 })),
        Memory::Cuda
    );
    assert_eq!(Memory::for_backend(None), Memory::System);
}
