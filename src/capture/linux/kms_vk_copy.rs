//! KMS scanout readback on a GPU copy engine (NVIDIA).
//!
//! The GL readback runs on the 3D engine, which a GPU-bound game time-slices
//! for ~13 ms per frame on NVIDIA whatever the context priority. A
//! transfer-only Vulkan queue runs on a copy engine the game does not block:
//! each scanout buffer is imported once, copied into cached host memory, and
//! converted to NV12/BGRA on the CPU (`csc`).

use super::csc::{Converter, DstFormat, SrcFormat};
use crate::capture::{DmaBufPlane, FrameData, RamPool};
use ash::vk;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

/// Scanout buffers kept imported: compositor swapchains plus a fullscreen
/// game's directly scanned-out swapchain.
const MAX_IMPORTS: usize = 8;
/// Imports unseen for this many readbacks are dropped on the next new buffer,
/// so a retired swapchain isn't kept alive.
const STALE_IMPORT_TICKS: u64 = 64;
const DRM_FORMAT_MOD_INVALID: u64 = (1 << 56) - 1;
const WAIT_TIMEOUT_NS: u64 = 1_000_000_000;
/// Row bands copied as separate submissions, so the CPU converts one while
/// the copy engine moves the next.
const BANDS: usize = 4;

pub enum ReadbackError {
    /// This buffer can't take the Vulkan path; use the GL readback for it.
    Unsupported(String),
    Failed(String),
}

fn failed(what: &str) -> impl Fn(vk::Result) -> ReadbackError + '_ {
    move |e| ReadbackError::Failed(format!("{what}: {e}"))
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var("ST_KMS_VK_COPY").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

fn vk_format(fmt: SrcFormat) -> vk::Format {
    // Same texel memory layout as the DRM fourcc; the copy moves raw bytes.
    match fmt {
        SrcFormat::Bgrx8 => vk::Format::B8G8R8A8_UNORM,
        SrcFormat::Rgbx8 => vk::Format::R8G8B8A8_UNORM,
        SrcFormat::Xrgb10 => vk::Format::A2R10G10B10_UNORM_PACK32,
        SrcFormat::Xbgr10 => vk::Format::A2B10G10R10_UNORM_PACK32,
        SrcFormat::Rgba16f => vk::Format::R16G16B16A16_SFLOAT,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ImportKey {
    dev: u64,
    ino: u64,
    fourcc: u32,
    modifier: u64,
    width: u32,
    height: u32,
    offset: u32,
    pitch: u32,
}

struct Import {
    key: ImportKey,
    image: vk::Image,
    memory: vk::DeviceMemory,
    last_used: u64,
}

struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    size: u64,
    coherent: bool,
}

pub struct VkReadback {
    _entry: ash::Entry,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    memory_fd: ash::khr::external_memory_fd::Device,
    fence_fd: ash::khr::external_fence_fd::Device,
    queue: vk::Queue,
    queue_family: u32,
    command_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,
    /// One per band, exported as a sync_file and polled so the realtime
    /// capture thread sleeps in the kernel.
    fences: Vec<vk::Fence>,
    memory_props: vk::PhysicalDeviceMemoryProperties,
    imports: Vec<Import>,
    staging: Option<Staging>,
    modifier_support: Vec<(vk::Format, u64, bool)>,
    converter: Converter,
    ram_pool: RamPool,
    tick: u64,
}

const DEVICE_EXTENSIONS: [&std::ffi::CStr; 5] = [
    ash::khr::external_memory_fd::NAME,
    ash::khr::external_fence_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

impl VkReadback {
    /// Opens the Vulkan device behind `render_node` with a transfer-only
    /// queue. Fails where there is no such queue family or DMA-BUF import.
    pub fn new(render_node: &str) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let rdev = std::fs::metadata(render_node)
            .map_err(|e| format!("stat {render_node}: {e}"))?
            .rdev();
        let node = (libc::major(rdev) as i64, libc::minor(rdev) as i64);
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("load libvulkan: {e}"))?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"st-server")
            .api_version(vk::API_VERSION_1_2);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
        }
        .map_err(|e| format!("vkCreateInstance: {e}"))?;
        let opened = unsafe { open_device(&instance, node) };
        let (physical, device, queue_family) = match opened {
            Ok(v) => v,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let mut this = Self {
            memory_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
            fence_fd: ash::khr::external_fence_fd::Device::new(&instance, &device),
            queue: unsafe { device.get_device_queue(queue_family, 0) },
            memory_props: unsafe { instance.get_physical_device_memory_properties(physical) },
            _entry: entry,
            instance,
            physical,
            device,
            queue_family,
            command_pool: vk::CommandPool::null(),
            command_buffers: Vec::new(),
            fences: Vec::new(),
            imports: Vec::new(),
            staging: None,
            modifier_support: Vec::new(),
            converter: Converter::new(Converter::default_threads()),
            ram_pool: RamPool::default(),
            tick: 0,
        };
        unsafe {
            this.command_pool = this
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(queue_family),
                    None,
                )
                .map_err(|e| format!("vkCreateCommandPool: {e}"))?;
            this.command_buffers = this
                .device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(this.command_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(BANDS as u32),
                )
                .map_err(|e| format!("vkAllocateCommandBuffers: {e}"))?;
            for _ in 0..BANDS {
                let mut export = vk::ExportFenceCreateInfo::default()
                    .handle_types(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
                let fence = this
                    .device
                    .create_fence(&vk::FenceCreateInfo::default().push_next(&mut export), None)
                    .map_err(|e| format!("vkCreateFence(sync_fd): {e}"))?;
                this.fences.push(fence);
            }
        }
        Ok(this)
    }

    /// Copy the scanout `plane` off the GPU and convert it to `dst`.
    pub fn readback(
        &mut self,
        plane: &DmaBufPlane,
        fourcc: u32,
        width: u32,
        height: u32,
        dst: DstFormat,
    ) -> Result<FrameData, ReadbackError> {
        let fmt = SrcFormat::from_drm(fourcc)
            .ok_or_else(|| ReadbackError::Unsupported(format!("fourcc {fourcc:#010x}")))?;
        self.tick += 1;
        let image = self.import(plane, fourcc, fmt, width, height)?;
        let bpp = fmt.bytes_per_pixel();
        let size = width as u64 * height as u64 * bpp as u64;
        let bands = band_rows(height as usize);
        unsafe { self.ensure_staging(size)? };
        let submitted = unsafe { self.submit_bands(image, width, bpp as u64, &bands) };
        let result = submitted.and_then(|()| self.convert_bands(fmt, dst, width, height, bands));
        if result.is_err() {
            // Fences and command buffers may still be pending; settle them
            // so the next frame can reuse both.
            unsafe {
                let _ = self.device.device_wait_idle();
                let _ = self.device.reset_fences(&self.fences);
            }
        }
        result
    }

    /// Wait for each band's copy and convert it while later bands copy.
    fn convert_bands(
        &mut self,
        fmt: SrcFormat,
        dst: DstFormat,
        width: u32,
        height: u32,
        bands: Vec<std::ops::Range<usize>>,
    ) -> Result<FrameData, ReadbackError> {
        let bpp = fmt.bytes_per_pixel();
        let (w, h) = (width as usize, height as usize);
        let size = w * h * bpp;
        let mut out = self.ram_pool.take(dst.len(w, h));
        let staging = self.staging.as_ref().expect("staging");
        let src = unsafe { std::slice::from_raw_parts(staging.ptr, size) };
        for (&fence, rows) in self.fences.iter().zip(bands) {
            // Exporting a SYNC_FD moves the payload out, leaving the fence
            // unsignalled for the next frame.
            let fd = unsafe {
                self.fence_fd.get_fence_fd(
                    &vk::FenceGetFdInfoKHR::default()
                        .fence(fence)
                        .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD),
                )
            }
            .map_err(failed("vkGetFenceFdKHR"))?;
            // -1: already signalled.
            if fd >= 0 {
                wait_sync_file(unsafe { OwnedFd::from_raw_fd(fd) })?;
            }
            if !staging.coherent {
                let range = vk::MappedMemoryRange::default()
                    .memory(staging.memory)
                    .size(vk::WHOLE_SIZE);
                unsafe { self.device.invalidate_mapped_memory_ranges(&[range]) }
                    .map_err(failed("vkInvalidateMappedMemoryRanges"))?;
            }
            self.converter
                .convert_rows(fmt, dst, src, w * bpp, &mut out, w, h, rows);
        }
        Ok(match dst {
            DstFormat::Nv12 => FrameData::RamNv12(out),
            DstFormat::Bgra => FrameData::Ram(out),
        })
    }

    /// Queue one copy per row band, each signalling its own fence.
    unsafe fn submit_bands(
        &mut self,
        image: vk::Image,
        width: u32,
        bpp: u64,
        bands: &[std::ops::Range<usize>],
    ) -> Result<(), ReadbackError> {
        let device = &self.device;
        let buffer = self.staging.as_ref().expect("staging").buffer;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let row_bytes = width as u64 * bpp;
        for (i, rows) in bands.iter().enumerate() {
            let cmd = self.command_buffers[i];
            device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(failed("vkBeginCommandBuffer"))?;
            if i == 0 {
                // The compositor owns the buffer: acquire it from the foreign
                // queue in GENERAL (contents preserved), release it after.
                let acquire = vk::ImageMemoryBarrier::default()
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                    .dst_queue_family_index(self.queue_family)
                    .image(image)
                    .subresource_range(range);
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire],
                );
            }
            let region = vk::BufferImageCopy::default()
                .buffer_offset(rows.start as u64 * row_bytes)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_offset(vk::Offset3D {
                    x: 0,
                    y: rows.start as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height: rows.len() as u32,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            let host = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(buffer)
                .offset(region.buffer_offset)
                .size(rows.len() as u64 * row_bytes);
            let release = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(image)
                .subresource_range(range);
            let last = i + 1 == bands.len();
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[host],
                if last {
                    std::slice::from_ref(&release)
                } else {
                    &[]
                },
            );
            device
                .end_command_buffer(cmd)
                .map_err(failed("vkEndCommandBuffer"))?;
        }
        for (cmd, &fence) in self
            .command_buffers
            .iter()
            .zip(&self.fences)
            .take(bands.len())
        {
            device
                .queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(cmd))],
                    fence,
                )
                .map_err(failed("vkQueueSubmit"))?;
        }
        Ok(())
    }

    /// The Vulkan image for this scanout buffer, imported on first sight.
    fn import(
        &mut self,
        plane: &DmaBufPlane,
        fourcc: u32,
        fmt: SrcFormat,
        width: u32,
        height: u32,
    ) -> Result<vk::Image, ReadbackError> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(plane.fd.as_raw_fd(), &mut st) } != 0 {
            return Err(ReadbackError::Failed(format!(
                "fstat dmabuf: {}",
                std::io::Error::last_os_error()
            )));
        }
        // PRIME re-exports of one GEM object share a dma-buf inode, and the
        // import keeps that dma-buf alive, so the inode can't be recycled.
        let key = ImportKey {
            dev: st.st_dev,
            ino: st.st_ino,
            fourcc,
            modifier: plane.modifier,
            width,
            height,
            offset: plane.offset,
            pitch: plane.pitch,
        };
        let tick = self.tick;
        if let Some(import) = self.imports.iter_mut().find(|i| i.key == key) {
            import.last_used = tick;
            return Ok(import.image);
        }
        let format = vk_format(fmt);
        if plane.modifier == DRM_FORMAT_MOD_INVALID
            || !self.modifier_supported(format, plane.modifier)
        {
            return Err(ReadbackError::Unsupported(format!(
                "fourcc {fourcc:#010x} modifier {:#018x}",
                plane.modifier
            )));
        }
        let (device, imports) = (&self.device, &mut self.imports);
        imports.retain(|import| {
            let fresh = tick - import.last_used <= STALE_IMPORT_TICKS;
            if !fresh {
                unsafe {
                    device.destroy_image(import.image, None);
                    device.free_memory(import.memory, None);
                }
            }
            fresh
        });
        if self.imports.len() >= MAX_IMPORTS {
            let oldest = (0..self.imports.len())
                .min_by_key(|&i| self.imports[i].last_used)
                .expect("imports");
            let import = self.imports.swap_remove(oldest);
            unsafe { self.destroy_import(import) };
        }
        let (image, memory) = unsafe { self.import_dmabuf(plane, format, width, height)? };
        self.imports.push(Import {
            key,
            image,
            memory,
            last_used: tick,
        });
        Ok(image)
    }

    unsafe fn import_dmabuf(
        &self,
        plane: &DmaBufPlane,
        format: vk::Format,
        width: u32,
        height: u32,
    ) -> Result<(vk::Image, vk::DeviceMemory), ReadbackError> {
        let layouts = [vk::SubresourceLayout {
            offset: plane.offset as u64,
            size: 0,
            row_pitch: plane.pitch as u64,
            array_pitch: 0,
            depth_pitch: 0,
        }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(plane.modifier)
            .plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);
        let image = self
            .device
            .create_image(&info, None)
            .map_err(failed("import vkCreateImage"))?;
        let result = (|| {
            let fd =
                OwnedFd::from_raw_fd(libc::fcntl(plane.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0));
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            self.memory_fd
                .get_memory_fd_properties(
                    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                    fd.as_raw_fd(),
                    &mut fd_props,
                )
                .map_err(failed("vkGetMemoryFdPropertiesKHR"))?;
            let reqs = self.device.get_image_memory_requirements(image);
            let bits = reqs.memory_type_bits & fd_props.memory_type_bits;
            if bits == 0 {
                return Err(ReadbackError::Unsupported(
                    "no memory type accepts the scanout dma-buf".into(),
                ));
            }
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let raw_fd = fd.into_raw_fd();
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(raw_fd);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(bits.trailing_zeros())
                .push_next(&mut import)
                .push_next(&mut dedicated);
            // A successful import owns the fd; a failed one leaves it with us.
            let memory = self.device.allocate_memory(&alloc, None).map_err(|e| {
                libc::close(raw_fd);
                ReadbackError::Failed(format!("import vkAllocateMemory: {e}"))
            })?;
            if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
                self.device.free_memory(memory, None);
                return Err(ReadbackError::Failed(format!("vkBindImageMemory: {e}")));
            }
            Ok(memory)
        })();
        match result {
            Ok(memory) => Ok((image, memory)),
            Err(e) => {
                self.device.destroy_image(image, None);
                Err(e)
            }
        }
    }

    fn modifier_supported(&mut self, format: vk::Format, modifier: u64) -> bool {
        if let Some(&(_, _, ok)) = self
            .modifier_support
            .iter()
            .find(|(f, m, _)| *f == format && *m == modifier)
        {
            return ok;
        }
        let count = {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            unsafe {
                self.instance.get_physical_device_format_properties2(
                    self.physical,
                    format,
                    &mut props,
                )
            };
            list.drm_format_modifier_count as usize
        };
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            unsafe {
                self.instance.get_physical_device_format_properties2(
                    self.physical,
                    format,
                    &mut props,
                )
            };
        }
        let ok = mods.iter().any(|m| {
            m.drm_format_modifier == modifier
                && m.drm_format_modifier_plane_count == 1
                && m.drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::TRANSFER_SRC)
        });
        self.modifier_support.push((format, modifier, ok));
        ok
    }

    unsafe fn ensure_staging(&mut self, size: u64) -> Result<(), ReadbackError> {
        if self.staging.as_ref().is_some_and(|s| s.size >= size) {
            return Ok(());
        }
        if let Some(old) = self.staging.take() {
            self.destroy_staging(old);
        }
        let buffer = self
            .device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
            .map_err(failed("staging vkCreateBuffer"))?;
        let reqs = self.device.get_buffer_memory_requirements(buffer);
        // CPU reads every byte: cached host memory, not write-combined BAR.
        let visible = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
        let find = |want: vk::MemoryPropertyFlags| {
            (0..self.memory_props.memory_type_count).find(|&i| {
                reqs.memory_type_bits & (1 << i) != 0
                    && self.memory_props.memory_types[i as usize]
                        .property_flags
                        .contains(want)
            })
        };
        let Some(type_index) =
            find(visible | vk::MemoryPropertyFlags::HOST_COHERENT).or_else(|| find(visible))
        else {
            self.device.destroy_buffer(buffer, None);
            return Err(ReadbackError::Failed("no cached host memory type".into()));
        };
        let coherent = self.memory_props.memory_types[type_index as usize]
            .property_flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT);
        let memory = match self.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                self.device.destroy_buffer(buffer, None);
                return Err(ReadbackError::Failed(format!(
                    "staging vkAllocateMemory: {e}"
                )));
            }
        };
        let mapped = self
            .device
            .bind_buffer_memory(buffer, memory, 0)
            .and_then(|()| {
                self.device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            });
        match mapped {
            Ok(ptr) => {
                self.staging = Some(Staging {
                    buffer,
                    memory,
                    ptr: ptr as *mut u8,
                    size,
                    coherent,
                });
                Ok(())
            }
            Err(e) => {
                self.device.destroy_buffer(buffer, None);
                self.device.free_memory(memory, None);
                Err(ReadbackError::Failed(format!("staging map: {e}")))
            }
        }
    }

    unsafe fn destroy_import(&self, import: Import) {
        self.device.destroy_image(import.image, None);
        self.device.free_memory(import.memory, None);
    }

    unsafe fn destroy_staging(&self, staging: Staging) {
        self.device.unmap_memory(staging.memory);
        self.device.destroy_buffer(staging.buffer, None);
        self.device.free_memory(staging.memory, None);
    }
}

impl Drop for VkReadback {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for import in std::mem::take(&mut self.imports) {
                self.destroy_import(import);
            }
            if let Some(staging) = self.staging.take() {
                self.destroy_staging(staging);
            }
            for &fence in &self.fences {
                self.device.destroy_fence(fence, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn wait_sync_file(fd: OwnedFd) -> Result<(), ReadbackError> {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = (WAIT_TIMEOUT_NS / 1_000_000) as i32;
    loop {
        match unsafe { libc::poll(&mut pfd, 1, timeout_ms) } {
            n if n > 0 => return Ok(()),
            0 => return Err(ReadbackError::Failed("copy timed out".into())),
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => {
                return Err(ReadbackError::Failed(format!(
                    "poll sync_file: {}",
                    std::io::Error::last_os_error()
                )))
            }
        }
    }
}

/// Even-aligned row bands covering `height`.
fn band_rows(height: usize) -> Vec<std::ops::Range<usize>> {
    let step = (height.div_ceil(BANDS) + 1) & !1;
    (0..height)
        .step_by(step.max(2))
        .map(|start| start..(start + step).min(height))
        .collect()
}

/// The physical device behind DRM render node `node` (major, minor), with a
/// transfer-only queue family and every extension the import path needs.
unsafe fn open_device(
    instance: &ash::Instance,
    node: (i64, i64),
) -> Result<(vk::PhysicalDevice, ash::Device, u32), String> {
    let devices = instance
        .enumerate_physical_devices()
        .map_err(|e| format!("vkEnumeratePhysicalDevices: {e}"))?;
    let physical = devices
        .into_iter()
        .find(|&pd| {
            let Ok(exts) = instance.enumerate_device_extension_properties(pd) else {
                return false;
            };
            let has = |name: &std::ffi::CStr| {
                exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
            };
            if !has(ash::ext::physical_device_drm::NAME)
                || !DEVICE_EXTENSIONS.iter().all(|n| has(n))
            {
                return false;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(pd, &mut props);
            drm.has_render == vk::TRUE && (drm.render_major, drm.render_minor) == node
        })
        .ok_or("no Vulkan device with DMA-BUF import behind the render node")?;
    let families = instance.get_physical_device_queue_family_properties(physical);
    let busy = vk::QueueFlags::GRAPHICS
        | vk::QueueFlags::COMPUTE
        | vk::QueueFlags::VIDEO_ENCODE_KHR
        | vk::QueueFlags::VIDEO_DECODE_KHR;
    let family = families
        .iter()
        .position(|f| {
            f.queue_flags.contains(vk::QueueFlags::TRANSFER) && !f.queue_flags.intersects(busy)
        })
        .ok_or("no transfer-only queue family")? as u32;
    let priorities = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(family)
        .queue_priorities(&priorities)];
    let extensions = DEVICE_EXTENSIONS.map(|n| n.as_ptr());
    let device = instance
        .create_device(
            physical,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&extensions),
            None,
        )
        .map_err(|e| format!("vkCreateDevice: {e}"))?;
    Ok((physical, device, family))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Banded readback of a 1440p FP16 scanout-like buffer, idle and with the
    /// GPU saturated: latency and calling-thread CPU per frame.
    /// `ST_TEST_KMS_COPY=1 cargo test --release copy_engine_latency -- --nocapture`
    /// (`ST_TEST_COPY_GAP_US=100000`: sparse frames find the GPU idle, ~9.6 ms)
    #[test]
    fn copy_engine_latency() {
        use crate::capture::linux::kms_gpu_copy::tests as kms;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        if std::env::var_os("ST_TEST_KMS_COPY").is_none() {
            return;
        }
        let node =
            std::env::var("ST_TEST_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let (w, h) = (2560u32, 1440u32);
        let stab = kms::nv12_stabilizer(&node, false);
        let src = kms::scanout_like_source(&stab, w, h, 0x4834_4241).expect("FP16 source");
        let plane = kms::source_plane(&src);
        let mut vk = VkReadback::new(&node).expect("copy engine");
        let gap = Duration::from_micros(
            std::env::var("ST_TEST_COPY_GAP_US")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6000),
        );
        for loaded in [false, true] {
            let stop = Arc::new(AtomicBool::new(false));
            let load = loaded.then(|| kms::spawn_gpu_load(node.clone(), w, h, Arc::clone(&stop)));
            std::thread::sleep(Duration::from_millis(300));
            let mut times = Vec::new();
            let cpu = kms::thread_cpu_time();
            for _ in 0..200 {
                let t = Instant::now();
                let frame = vk
                    .readback(plane, 0x4834_4241, w, h, DstFormat::Nv12)
                    .ok()
                    .unwrap();
                times.push(t.elapsed());
                drop(frame);
                std::thread::sleep(gap);
            }
            let cpu = (kms::thread_cpu_time() - cpu) / 200;
            stop.store(true, Ordering::Relaxed);
            if let Some(load) = load {
                load.join().unwrap();
            }
            times.sort();
            eprintln!(
                "[copy-engine] {} p50={:.2?} p95={:.2?} p99={:.2?} thread-cpu/frame={cpu:.2?}",
                if loaded { "gpu-load" } else { "idle" },
                times[100],
                times[190],
                times[198]
            );
        }
        kms::destroy_source(&stab, src);
    }

    /// Lists the DRM modifiers the copy engine can read per format.
    /// `ST_TEST_KMS_COPY=1 cargo test --release copy_engine_modifiers -- --nocapture`
    #[test]
    fn copy_engine_modifiers() {
        if std::env::var_os("ST_TEST_KMS_COPY").is_none() {
            return;
        }
        let node =
            std::env::var("ST_TEST_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
        let vk = VkReadback::new(&node).expect("copy engine");
        for format in [vk::Format::B8G8R8A8_UNORM, vk::Format::R16G16B16A16_SFLOAT] {
            let count = {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut props = vk::FormatProperties2::default().push_next(&mut list);
                unsafe {
                    vk.instance.get_physical_device_format_properties2(
                        vk.physical,
                        format,
                        &mut props,
                    )
                };
                list.drm_format_modifier_count as usize
            };
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            unsafe {
                vk.instance
                    .get_physical_device_format_properties2(vk.physical, format, &mut props)
            };
            for m in &mods {
                eprintln!(
                    "format {} {:#018x} planes={} transfer_src={}",
                    format.as_raw(),
                    m.drm_format_modifier,
                    m.drm_format_modifier_plane_count,
                    m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::TRANSFER_SRC)
                );
            }
        }
    }
}
