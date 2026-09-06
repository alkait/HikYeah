// gpu.rs — zero-copy video on Linux: the wgpu device is created on Vulkan
// with the DMA-BUF import extensions, and a VAAPI-decoded surface (exported
// by the decoder as a DMA-BUF) becomes a pair of wgpu textures without a
// single CPU copy. This is the VideoToolbox arrangement done with Linux
// primitives: decode on the iGPU's media engine, sample it in the shader.
//
// Gated on the render adapter being the Intel integrated GPU that also owns
// the VAAPI device — the same memory, no PCIe crossing. Anything else (other
// platforms, a discrete adapter, a driver without the extensions) keeps the
// download path in decode.rs.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// One exported VAAPI surface: a DMA-BUF holding NV12 as two planes.
#[cfg(target_os = "linux")]
pub struct DmaFrame {
    pub fd: std::os::fd::OwnedFd,
    /// Identity of the buffer behind the fd (its inode): surfaces are
    /// pooled by the decoder, so the same buffer comes around again and its
    /// imported textures can be reused.
    pub ino: u64,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    /// (byte offset, row pitch) of the Y and UV planes.
    pub planes: [(u64, u64); 2],
    /// Keeps the decoder's surface alive while this frame exists.
    pub keep: Box<dyn Send + Sync>,
}

/// Zero-copy frames don't exist off Linux; the type keeps `Frame` uniform.
#[cfg(not(target_os = "linux"))]
pub struct DmaFrame {
    pub ino: u64,
    pub width: u32,
    pub height: u32,
}

/// Runtime switch for the zero-copy path: on when the device supports it,
/// flipped off for good the first time an import fails, so decoders fall
/// back to downloading frames without a restart.
pub type ZeroCopy = Arc<AtomicBool>;

#[cfg(target_os = "linux")]
mod linux {
    use super::DmaFrame;
    use ash::vk;
    use eframe::egui_wgpu::WgpuSetupExisting;
    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use wgpu::hal;
    use wgpu::hal::Instance as _;

    pub struct Setup {
        pub existing: WgpuSetupExisting,
        pub zero_copy: bool,
        pub adapter_names: Vec<String>,
    }

    const DMABUF_EXTENSIONS: [&CStr; 3] = [
        ash::khr::external_memory_fd::NAME,
        ash::ext::external_memory_dma_buf::NAME,
        ash::ext::image_drm_format_modifier::NAME,
    ];

    /// The Vulkan device wgpu would have made, plus the DMA-BUF extensions
    /// when the chosen adapter is an Intel iGPU that has them.
    pub fn create(want: Option<&str>) -> Result<Setup, String> {
        let desc = hal::InstanceDescriptor {
            name: "hikyeah",
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        };
        let hal_instance =
            unsafe { hal::vulkan::Instance::init(&desc) }.map_err(|e| format!("vulkan: {e}"))?;
        let mut exposed = unsafe { hal_instance.enumerate_adapters(None) };
        if exposed.is_empty() {
            return Err("no vulkan adapter".into());
        }
        let mut adapter_names: Vec<String> = Vec::new();
        for a in &exposed {
            if !adapter_names.contains(&a.info.name) {
                adapter_names.push(a.info.name.clone());
            }
        }
        let idx = want
            .and_then(|w| exposed.iter().position(|a| a.info.name == w))
            .unwrap_or(0);
        let exposed = exposed.swap_remove(idx);

        let raw_instance = hal_instance.shared_instance().raw_instance();
        let phd = exposed.adapter.raw_physical_device();
        let available = unsafe { raw_instance.enumerate_device_extension_properties(phd) }
            .map_err(|e| e.to_string())?;
        let has_ext = |name: &CStr| {
            available
                .iter()
                .any(|p| p.extension_name_as_c_str().is_ok_and(|n| n == name))
        };
        let zero_copy = exposed.info.device_type == wgpu::DeviceType::IntegratedGpu
            && exposed.info.vendor == 0x8086
            && DMABUF_EXTENSIONS.iter().all(|e| has_ext(e));

        let features = wgpu::Features::empty();
        let limits = wgpu::Limits::default();
        let callback: Option<Box<hal::vulkan::CreateDeviceCallback<'_>>> = zero_copy.then(|| {
            Box::new(|args: hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
                for ext in DMABUF_EXTENSIONS {
                    if !args.extensions.contains(&ext) {
                        args.extensions.push(ext);
                    }
                }
            }) as Box<hal::vulkan::CreateDeviceCallback<'_>>
        });
        let open = unsafe {
            exposed.adapter.open_with_callback(
                features,
                &limits,
                &wgpu::MemoryHints::default(),
                callback,
            )
        }
        .map_err(|e| format!("open device: {e}"))?;
        let instance = unsafe { wgpu::Instance::from_hal::<hal::api::Vulkan>(hal_instance) };
        let adapter = unsafe { instance.create_adapter_from_hal(exposed) };
        let (device, queue) = unsafe {
            adapter.create_device_from_hal(
                open,
                &wgpu::DeviceDescriptor {
                    label: Some("hikyeah"),
                    required_features: features,
                    required_limits: limits,
                    ..Default::default()
                },
            )
        }
        .map_err(|e| format!("device: {e}"))?;
        Ok(Setup {
            existing: WgpuSetupExisting {
                instance,
                adapter,
                device,
                queue,
            },
            zero_copy,
            adapter_names,
        })
    }

    /// Import the two planes of an exported surface as R8 and RG8 textures
    /// bound to the DMA-BUF's memory. Vulkan takes ownership of a dup of the
    /// fd; the frame's own fd stays with the frame.
    pub fn import(device: &wgpu::Device, dma: &DmaFrame) -> Result<[wgpu::Texture; 2], String> {
        let hal_dev =
            unsafe { device.as_hal::<hal::api::Vulkan>() }.ok_or("not a vulkan device")?;
        let raw = hal_dev.raw_device().clone();
        let instance = hal_dev.shared_instance().raw_instance();
        let phd = hal_dev.raw_physical_device();
        let mem_props = unsafe { instance.get_physical_device_memory_properties(phd) };
        let fd_ext = ash::khr::external_memory_fd::Device::new(instance, &raw);

        let mut out: Vec<wgpu::Texture> = Vec::with_capacity(2);
        for plane in 0..2usize {
            let (offset, pitch) = dma.planes[plane];
            let (w, h, vk_fmt, fmt) = if plane == 0 {
                (
                    dma.width,
                    dma.height,
                    vk::Format::R8_UNORM,
                    wgpu::TextureFormat::R8Unorm,
                )
            } else {
                (
                    dma.width.div_ceil(2),
                    dma.height.div_ceil(2),
                    vk::Format::R8G8_UNORM,
                    wgpu::TextureFormat::Rg8Unorm,
                )
            };
            let layouts = [vk::SubresourceLayout {
                offset,
                size: 0,
                row_pitch: pitch,
                array_pitch: 0,
                depth_pitch: 0,
            }];
            let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(dma.modifier)
                .plane_layouts(&layouts);
            let mut external = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk_fmt)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut modifier_info)
                .push_next(&mut external);
            let image = unsafe { raw.create_image(&info, None) }
                .map_err(|e| format!("plane {plane} image: {e}"))?;
            let req = unsafe { raw.get_image_memory_requirements(image) };

            let dup = unsafe { libc::dup(dma.fd.as_raw_fd()) };
            if dup < 0 {
                unsafe { raw.destroy_image(image, None) };
                return Err("dup failed".into());
            }
            let dup = unsafe { OwnedFd::from_raw_fd(dup) };
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            if let Err(e) = unsafe {
                fd_ext.get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    dup.as_raw_fd(),
                    &mut fd_props,
                )
            } {
                unsafe { raw.destroy_image(image, None) };
                return Err(format!("fd properties: {e}"));
            }
            let bits = req.memory_type_bits & fd_props.memory_type_bits;
            let Some(index) = (0..mem_props.memory_type_count).find(|i| bits & (1 << i) != 0)
            else {
                unsafe { raw.destroy_image(image, None) };
                return Err("no memory type for the dma-buf".into());
            };
            let raw_fd = dup.into_raw_fd(); // Vulkan owns it once the import succeeds
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(raw_fd);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(index)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = match unsafe { raw.allocate_memory(&alloc, None) } {
                Ok(m) => m,
                Err(e) => {
                    unsafe {
                        libc::close(raw_fd);
                        raw.destroy_image(image, None);
                    }
                    return Err(format!("import memory: {e}"));
                }
            };
            if let Err(e) = unsafe { raw.bind_image_memory(image, memory, 0) } {
                unsafe {
                    raw.free_memory(memory, None);
                    raw.destroy_image(image, None);
                }
                return Err(format!("bind: {e}"));
            }

            let size = wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            };
            let hal_desc = hal::TextureDescriptor {
                label: Some("dma-buf plane"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUses::RESOURCE,
                memory_flags: hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            let cleanup = raw.clone();
            let hal_tex = unsafe {
                hal_dev.texture_from_raw(
                    image,
                    &hal_desc,
                    Some(Box::new(move || {
                        cleanup.destroy_image(image, None);
                        cleanup.free_memory(memory, None);
                    })),
                    hal::vulkan::TextureMemory::External,
                )
            };
            let desc = wgpu::TextureDescriptor {
                label: Some("dma-buf plane"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            out.push(unsafe {
                device.create_texture_from_hal::<hal::api::Vulkan>(
                    hal_tex,
                    &desc,
                    wgpu::TextureUses::UNINITIALIZED,
                )
            });
        }
        let uv = out.pop().unwrap();
        let y = out.pop().unwrap();
        Ok([y, uv])
    }
}

#[cfg(target_os = "linux")]
pub use linux::{create, import};

#[cfg(not(target_os = "linux"))]
pub struct Setup {
    pub existing: eframe::egui_wgpu::WgpuSetupExisting,
    pub zero_copy: bool,
    pub adapter_names: Vec<String>,
}

#[cfg(not(target_os = "linux"))]
pub fn create(_want: Option<&str>) -> Result<Setup, String> {
    Err("zero-copy video is Linux-only for now".into())
}

#[cfg(not(target_os = "linux"))]
pub fn import(_device: &wgpu::Device, _dma: &DmaFrame) -> Result<[wgpu::Texture; 2], String> {
    Err("unsupported".into())
}
