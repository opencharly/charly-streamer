//! Hardware encode backends, and the encoder ranking each one needs.
//!
//! The backend is a property of the RENDER NODE, not of the host: a host with an
//! NVIDIA and an AMD GPU (both injected through CDI) has both encoders registered,
//! and the right one is the one on the card that renders the desktop. The kernel
//! driver bound to that node decides it:
//!
//! | driver            | backend | capture memory     | encoder     |
//! |-------------------|---------|--------------------|-------------|
//! | `nvidia`          | `Cuda`  | `memory:CUDAMemory` | `nvh264enc` |
//! | `amdgpu` `i915` `xe` | `Va` | `memory:DMABuf` + vapostproc | `vah264enc` |
//!
//! Both paths were measured inside a pod-cdi pod with only CDI-injected driver
//! user-space: CUDAMemory straight from waylanddisplaysrc into `nvh264enc` encodes
//! (DMABuf into `nvh264enc` does not negotiate), and DMABuf through `vapostproc`
//! into `vah264enc` encodes on radeonsi.
//!
//! Ranking: webrtcsink enumerates encoders ONCE, into a `LazyLock` `CODECS` table,
//! and only considers those at `Rank::MARGINAL` or above. `vah264enc` registers at
//! `GST_RANK_NONE`, so without promotion webrtcsink never sees it and silently picks
//! a software encoder. Because `CODECS` is a one-shot `LazyLock`, raising the rank
//! after the first webrtcsink exists has no effect: `raise_hardware_encoders` must
//! run first. Only the chosen backend's encoders are raised, so webrtcsink cannot
//! pick an encoder on a GPU nobody asked for.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use std::path::Path;

/// The hardware encode path for one render node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// VA-API (AMD, Intel): DMABuf, converted by `vapostproc`.
    Va,
    /// NVENC through CUDA (NVIDIA): CUDAMemory from the source.
    ///
    /// `device_id` is the CUDA ordinal of THIS node's GPU under
    /// `CUDA_DEVICE_ORDER=PCI_BUS_ID`, which the streamer sets before CUDA loads.
    /// CUDA's default order is "fastest first", which would make the index depend
    /// on which other cards happen to be present.
    Cuda { device_id: i32 },
}

impl Backend {
    /// The encoders this backend provides, in preference order.
    pub fn encoders(&self) -> &'static [&'static str] {
        match self {
            Self::Va => &["vah264enc", "vah265enc"],
            Self::Cuda { .. } => &["nvh264enc", "nvh265enc"],
        }
    }

    /// Resolve the backend of `render_node` from sysfs (`sysfs` is `/sys` in
    /// production, a fixture tree in tests).
    ///
    /// `Ok(None)` is a render node whose driver has no hardware encode path here
    /// (virtio-gpu, a software node): `encode: auto` then uses software, and
    /// `encode: hardware` refuses.
    pub fn detect(render_node: &str, sysfs: &Path) -> Result<Option<Self>> {
        let name = Path::new(render_node)
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("render node {render_node:?} has no device name"))?;
        let driver = driver_of(sysfs, name)
            .with_context(|| format!("no DRM device {name} under {}", sysfs.display()))?;
        Ok(match driver.as_str() {
            "nvidia" => Some(Self::Cuda {
                device_id: cuda_index(sysfs, name)?,
            }),
            "amdgpu" | "i915" | "xe" => Some(Self::Va),
            _ => None,
        })
    }
}

/// The kernel driver bound to a DRM device, by name (`renderD128`).
fn driver_of(sysfs: &Path, drm_name: &str) -> Option<String> {
    let link =
        std::fs::read_link(sysfs.join("class/drm").join(drm_name).join("device/driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The PCI address of a DRM device (`0000:01:00.0`), from its `device` link.
fn pci_address(sysfs: &Path, drm_name: &str) -> Option<String> {
    let link = std::fs::read_link(sysfs.join("class/drm").join(drm_name).join("device")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// The CUDA ordinal of an NVIDIA render node under `CUDA_DEVICE_ORDER=PCI_BUS_ID`:
/// its position among every NVIDIA GPU's PCI address, sorted.
fn cuda_index(sysfs: &Path, drm_name: &str) -> Result<i32> {
    let ours = pci_address(sysfs, drm_name)
        .with_context(|| format!("{drm_name} has no PCI device link"))?;
    let mut nvidia: Vec<String> = std::fs::read_dir(sysfs.join("class/drm"))
        .with_context(|| format!("listing {}", sysfs.join("class/drm").display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("renderD"))
        .filter(|n| driver_of(sysfs, n).as_deref() == Some("nvidia"))
        .filter_map(|n| pci_address(sysfs, &n))
        .collect();
    nvidia.sort();
    nvidia.dedup();
    match nvidia.iter().position(|a| *a == ours) {
        Some(i) => Ok(i as i32),
        None => bail!("{drm_name} ({ours}) is not among the NVIDIA GPUs {nvidia:?}"),
    }
}

/// Raise the backend's encoders above webrtcsink's MARGINAL floor.
///
/// Returns the elements actually promoted. A missing element is not an error here:
/// `select_encoder` decides whether the policy can be met, and says why if not.
pub fn raise_hardware_encoders(backend: Option<&Backend>) -> Vec<String> {
    let mut raised = Vec::new();
    for name in backend.map(Backend::encoders).unwrap_or_default() {
        if let Some(factory) = gst::ElementFactory::find(name) {
            // PRIMARY + 1 puts it above every software encoder, which sit at or below
            // PRIMARY. MARGINAL alone would clear webrtcsink's floor but would not
            // guarantee it wins the pick.
            factory.set_rank(gst::Rank::PRIMARY + 1);
            raised.push((*name).to_string());
        }
    }
    raised
}

/// True while none of the backend's encoders sits at or above webrtcsink's floor.
pub fn ranking_is_still_pending(backend: &Backend) -> bool {
    backend.encoders().iter().all(|name| {
        gst::ElementFactory::find(name)
            .map(|f| f.rank() < gst::Rank::MARGINAL)
            .unwrap_or(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    /// A sysfs fixture: each render node links to a PCI device whose driver link
    /// names its kernel driver -- the same shape as the real /sys.
    fn sysfs(nodes: &[(&str, &str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cstream-sysfs-{}-{}",
            std::process::id(),
            nodes.len() * 1000 + nodes.iter().map(|n| n.1.len()).sum::<usize>()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (node, pci, driver) in nodes {
            let dev = root.join("devices/pci0000:00").join(pci);
            let drv = root.join("bus/pci/drivers").join(driver);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::create_dir_all(&drv).unwrap();
            let _ = symlink(&drv, dev.join("driver"));
            let class = root.join("class/drm").join(node);
            std::fs::create_dir_all(&class).unwrap();
            symlink(&dev, class.join("device")).unwrap();
        }
        root
    }

    #[test]
    fn the_driver_decides_the_backend() {
        let root = sysfs(&[
            ("renderD128", "0000:01:00.0", "nvidia"),
            ("renderD129", "0000:73:00.0", "amdgpu"),
            ("renderD130", "0000:00:02.0", "virtio-pci"),
        ]);
        assert_eq!(
            Backend::detect("/dev/dri/renderD128", &root).unwrap(),
            Some(Backend::Cuda { device_id: 0 })
        );
        assert_eq!(
            Backend::detect("/dev/dri/renderD129", &root).unwrap(),
            Some(Backend::Va)
        );
        assert_eq!(Backend::detect("/dev/dri/renderD130", &root).unwrap(), None);
        assert!(Backend::detect("/dev/dri/renderD131", &root).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Two NVIDIA cards: the CUDA ordinal follows PCI bus order, not the order the
    /// render nodes were numbered in, so it matches CUDA_DEVICE_ORDER=PCI_BUS_ID.
    #[test]
    fn the_cuda_ordinal_follows_pci_bus_order() {
        let root = sysfs(&[
            ("renderD128", "0000:41:00.0", "nvidia"),
            ("renderD129", "0000:01:00.0", "nvidia"),
        ]);
        assert_eq!(
            Backend::detect("/dev/dri/renderD128", &root).unwrap(),
            Some(Backend::Cuda { device_id: 1 })
        );
        assert_eq!(
            Backend::detect("/dev/dri/renderD129", &root).unwrap(),
            Some(Backend::Cuda { device_id: 0 })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn each_backend_names_its_own_encoders() {
        assert_eq!(Backend::Va.encoders()[0], "vah264enc");
        assert_eq!(Backend::Cuda { device_id: 0 }.encoders()[0], "nvh264enc");
    }
}
