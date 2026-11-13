//! Graphics backend: the platform's native API, nothing else.
//!
//! iced opens a wgpu instance on every compiled backend, so on Windows both the Vulkan and the DX12
//! driver stacks get loaded. Measured on an RTX 4080 (release, private working set): 183 MB default,
//! 134-150 MB DX12 only, 165 MB Vulkan only (NVIDIA's Vulkan ICD presents through DXGI, so it loads
//! the D3D12 driver as well). Each OS therefore gets its own native API, then the portable fallbacks:
//! - Windows: DX12, then Vulkan, then GL;
//! - macOS: Metal (Vulkan there is MoltenVK, a translation layer onto Metal);
//! - Linux and the BSDs: Vulkan, then GL (VMs and GPUs without a Vulkan driver).
//!
//! If none has a hardware adapter, nothing is pinned: iced tries every backend, then its tiny-skia
//! CPU renderer. A `WGPU_BACKEND` or `ICED_BACKEND` set by the user always wins.

use iced::wgpu::{self, Backends, DeviceType};

/// Native backend first, then the fallbacks, as `WGPU_BACKEND` values.
const ORDER: &[(Backends, &str)] = if cfg!(windows) {
    &[(Backends::DX12, "dx12"), (Backends::VULKAN, "vulkan"), (Backends::GL, "gl")]
} else if cfg!(target_vendor = "apple") {
    &[(Backends::METAL, "metal")]
} else {
    &[(Backends::VULKAN, "vulkan"), (Backends::GL, "gl")]
};

/// Pins `WGPU_BACKEND` to the first backend in [`ORDER`] with a hardware adapter.
///
/// Must run before the first thread is spawned: it writes the process environment.
pub fn pin_native_backend() {
    if std::env::var_os("WGPU_BACKEND").is_some() || std::env::var_os("ICED_BACKEND").is_some() {
        return;
    }
    if let Some((_, name)) = ORDER.iter().find(|(backends, _)| has_hardware_adapter(*backends)) {
        // SAFETY: called first thing in `main`, before any other thread exists.
        unsafe { std::env::set_var("WGPU_BACKEND", name) };
    }
}

/// True when `backends` exposes a GPU (a software adapter such as WARP or llvmpipe does not count).
fn has_hardware_adapter(backends: Backends) -> bool {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
    instance.enumerate_adapters(backends).iter().any(|a| a.get_info().device_type != DeviceType::Cpu)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_backend_comes_first() {
        let native = if cfg!(windows) {
            Backends::DX12
        } else if cfg!(target_vendor = "apple") {
            Backends::METAL
        } else {
            Backends::VULKAN
        };
        assert_eq!(ORDER[0].0, native);
        for (backends, name) in ORDER {
            assert_eq!(Backends::from_comma_list(name), *backends, "{name}");
        }
    }
}
