//! NVENC zero-copy DMA-BUF path via EGL↔CUDA interop (Linux / NVIDIA).
//!
//! The default NVENC path (`encode.rs`) reads a captured DMA-BUF back to system
//! memory (`mmap`), converts BGRA→NV12 on the CPU (`swscale`), then hands the SW
//! frame to FFmpeg's NVENC wrapper which uploads it to VRAM again. Three costs on
//! the hot per-frame path: a PCIe read-back, a single-threaded CPU colour
//! convert, and a PCIe upload — 2-5 ms @1080p, 6-12 ms @4K.
//!
//! This module keeps the frame in VRAM end-to-end. The captured DMA-BUF is
//! imported as an `EGLImage` (the same `EGL_EXT_image_dma_buf_import` path the
//! KMS stabiliser already uses), registered into CUDA with
//! `cuGraphicsEGLRegisterImage`, and `cuMemcpy2D`'d (device→device, sub-ms) into
//! a CUDA frame allocated from FFmpeg's `AV_PIX_FMT_CUDA` frames pool whose
//! `sw_format` is `AV_PIX_FMT_BGR0`. NVENC then does the BGRA→NV12 conversion on
//! the GPU as part of the encode. No read-back, no CPU convert, no extra upload.
//!
//! libcuda is `dlopen`ed (matching the EGL/GL/gbm loaders in `kms_gpu_copy`); no
//! CUDA crate dependency. The FFmpeg CUDA context is created by
//! `av_hwdevice_ctx_create` and *borrowed* here — we never own a CUDA context.
//!
//! **What actually runs on NVIDIA.** The KMS stabiliser can't export a *linear*
//! DMA-BUF on NVIDIA (gbm rejects `GBM_BO_USE_LINEAR` for renderable targets) and
//! falls back to a `glReadPixels` RAM frame, so NVENC is fed `FrameData::Ram`.
//! The CUDA path still wins there: it uploads the RAM BGRA straight to a CUDA
//! frame (`cuMemcpy2D` host→device) and lets NVENC convert on the GPU, removing
//! the single-threaded CPU `swscale` (and freeing a core). The true zero-copy
//! DMA-BUF import is used when a real importable DMA-BUF reaches NVENC (e.g.
//! `ST_KMS_COPY=0` raw scanout, or a future tiled stabiliser output); NVIDIA's
//! CUDA-EGL interop rejects block-linear BOs, so that import is validated
//! best-effort and a DMA-BUF that can't be imported falls back to the CPU path.
//!
//! **Auto-enable + fallback (the CLAUDE.md rule).** `new()` builds the whole
//! chain and runs a real end-to-end self-test on this GPU before returning `Ok`:
//! a RAM→CUDA upload (required — it's the live NVIDIA path) plus a best-effort
//! DMA-BUF import probe. Any failure of the required path returns `Err` and the
//! caller silently uses the CPU path. A run of consecutive per-frame failures
//! trips [`mark_disabled`] so the next encoder rebuild stays on the CPU path.
//! `ST_NVENC_CUDA=0` force-disables.

use crate::capture::DmaBufPlane;
use khronos_egl as egl;
use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

extern crate ffmpeg_sys_next as ffi;

// --- runtime fallback latch -------------------------------------------------

/// Tripped after repeated per-frame import failures so a later encoder rebuild
/// (ABR / resolution change) stays on the proven CPU path for the session.
static CUDA_DISABLED: AtomicBool = AtomicBool::new(false);

/// Honour the `ST_NVENC_CUDA` escape hatch and the runtime fallback latch.
/// Default-on: only an explicit `0`/`false`/`no`/`off` disables it.
pub fn cuda_zero_copy_enabled() -> bool {
    if CUDA_DISABLED.load(Ordering::Relaxed) {
        return false;
    }
    !matches!(
        std::env::var("ST_NVENC_CUDA").ok().as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

fn mark_disabled(reason: &str) {
    if !CUDA_DISABLED.swap(true, Ordering::Relaxed) {
        eprintln!("[nvenc-cuda] disabling zero-copy path for this session: {reason}");
    }
}

// --- CUDA driver FFI (dlopen) ----------------------------------------------

type CUresult = i32;
type CUcontext = *mut c_void;
type CUgraphicsResource = *mut c_void;
type CUdeviceptr = u64;
type CUarray = *mut c_void;
const CUDA_SUCCESS: CUresult = 0;

// CUmemorytype
const CU_MEMORYTYPE_HOST: u32 = 1;
const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_MEMORYTYPE_ARRAY: u32 = 3;

// CUeglFrameType
const CU_EGL_FRAME_TYPE_ARRAY: u32 = 0;

// cuGraphicsEGLRegisterImage flags
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 0x01;

/// Mirror of CUDA's `CUeglFrame` (cudaEGL.h). The leading union of
/// `CUarray[3]` / `void*[3]` is three pointers wide.
#[repr(C)]
#[derive(Clone, Copy)]
struct CUeglFrame {
    frame: [*mut c_void; 3],
    width: u32,
    height: u32,
    depth: u32,
    pitch: u32,
    plane_count: u32,
    num_channels: u32,
    frame_type: u32,
    egl_color_format: u32,
    cu_format: u32,
}

/// Mirror of CUDA's `CUDA_MEMCPY2D_v2` (cuda.h). `repr(C)` reproduces the
/// padding after each `u32` enum so the layout matches the driver ABI.
#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: CUdeviceptr,
    src_array: CUarray,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: CUdeviceptr,
    dst_array: CUarray,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

type FnCtxPushCurrent = unsafe extern "C" fn(CUcontext) -> CUresult;
type FnCtxPopCurrent = unsafe extern "C" fn(*mut CUcontext) -> CUresult;
type FnGraphicsEGLRegisterImage =
    unsafe extern "C" fn(*mut CUgraphicsResource, *mut c_void, u32) -> CUresult;
type FnGraphicsResourceGetMappedEglFrame =
    unsafe extern "C" fn(*mut CUeglFrame, CUgraphicsResource, u32, u32) -> CUresult;
type FnGraphicsUnregisterResource = unsafe extern "C" fn(CUgraphicsResource) -> CUresult;
type FnMemcpy2D = unsafe extern "C" fn(*const CudaMemcpy2D) -> CUresult;
type FnCtxSynchronize = unsafe extern "C" fn() -> CUresult;
type FnMemAllocPitch =
    unsafe extern "C" fn(*mut CUdeviceptr, *mut usize, usize, usize, u32) -> CUresult;
type FnMemFree = unsafe extern "C" fn(CUdeviceptr) -> CUresult;

struct CudaLib {
    _lib: Library,
    ctx_push: FnCtxPushCurrent,
    ctx_pop: FnCtxPopCurrent,
    egl_register: FnGraphicsEGLRegisterImage,
    egl_get_frame: FnGraphicsResourceGetMappedEglFrame,
    unregister: FnGraphicsUnregisterResource,
    memcpy2d: FnMemcpy2D,
    ctx_sync: FnCtxSynchronize,
    // Validated foundation for the GL→CUDA stabiliser readback-elimination path
    // (exercised by tests via test_alloc_blue / make_frame_from_cuda_buffer);
    // wired into the live capture pipeline in the follow-up stage.
    #[allow(dead_code)]
    mem_alloc_pitch: FnMemAllocPitch,
    #[allow(dead_code)]
    mem_free: FnMemFree,
}

impl CudaLib {
    fn load() -> Result<Self, String> {
        unsafe {
            let lib = ["libcuda.so.1", "libcuda.so"]
                .iter()
                .find_map(|name| Library::new(name).ok())
                .ok_or_else(|| "libcuda.so.1 not found".to_string())?;
            // Resolve every symbol up front; the `'static` transmute is sound
            // because we keep `lib` alive in the same struct.
            fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
                unsafe {
                    let s: Symbol<T> = lib
                        .get(name)
                        .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(name)))?;
                    Ok(*s)
                }
            }
            let ctx_push = sym::<FnCtxPushCurrent>(&lib, b"cuCtxPushCurrent_v2\0")?;
            let ctx_pop = sym::<FnCtxPopCurrent>(&lib, b"cuCtxPopCurrent_v2\0")?;
            let egl_register =
                sym::<FnGraphicsEGLRegisterImage>(&lib, b"cuGraphicsEGLRegisterImage\0")?;
            let egl_get_frame = sym::<FnGraphicsResourceGetMappedEglFrame>(
                &lib,
                b"cuGraphicsResourceGetMappedEglFrame\0",
            )?;
            let unregister =
                sym::<FnGraphicsUnregisterResource>(&lib, b"cuGraphicsUnregisterResource\0")?;
            let memcpy2d = sym::<FnMemcpy2D>(&lib, b"cuMemcpy2D_v2\0")?;
            let ctx_sync = sym::<FnCtxSynchronize>(&lib, b"cuCtxSynchronize\0")?;
            let mem_alloc_pitch = sym::<FnMemAllocPitch>(&lib, b"cuMemAllocPitch_v2\0")?;
            let mem_free = sym::<FnMemFree>(&lib, b"cuMemFree_v2\0")?;
            Ok(Self {
                _lib: lib,
                ctx_push,
                ctx_pop,
                egl_register,
                egl_get_frame,
                unregister,
                memcpy2d,
                ctx_sync,
                mem_alloc_pitch,
                mem_free,
            })
        }
    }
}

// --- minimal gbm loader (EGL display device + self-test buffer) -------------

const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

type FnGbmCreateDevice = unsafe extern "C" fn(libc::c_int) -> *mut c_void;
type FnGbmDeviceDestroy = unsafe extern "C" fn(*mut c_void);
type FnGbmBoCreate = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void;
type FnGbmBoGetFd = unsafe extern "C" fn(*mut c_void) -> libc::c_int;
type FnGbmBoGetStride = unsafe extern "C" fn(*mut c_void) -> u32;
type FnGbmBoGetOffset = unsafe extern "C" fn(*mut c_void, libc::c_int) -> u32;
type FnGbmBoGetModifier = unsafe extern "C" fn(*mut c_void) -> u64;
type FnGbmBoDestroy = unsafe extern "C" fn(*mut c_void);

struct GbmLib {
    _lib: Library,
    create_device: FnGbmCreateDevice,
    device_destroy: FnGbmDeviceDestroy,
    bo_create: FnGbmBoCreate,
    bo_get_fd: FnGbmBoGetFd,
    bo_get_stride: FnGbmBoGetStride,
    bo_get_offset: FnGbmBoGetOffset,
    bo_get_modifier: FnGbmBoGetModifier,
    bo_destroy: FnGbmBoDestroy,
}

impl GbmLib {
    fn load() -> Result<Self, String> {
        unsafe {
            let lib = ["libgbm.so.1", "libgbm.so"]
                .iter()
                .find_map(|name| Library::new(name).ok())
                .ok_or_else(|| "libgbm.so.1 not found".to_string())?;
            fn sym<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, String> {
                unsafe {
                    let s: Symbol<T> = lib
                        .get(name)
                        .map_err(|e| format!("{}: {e}", String::from_utf8_lossy(name)))?;
                    Ok(*s)
                }
            }
            Ok(Self {
                create_device: sym(&lib, b"gbm_create_device\0")?,
                device_destroy: sym(&lib, b"gbm_device_destroy\0")?,
                bo_create: sym(&lib, b"gbm_bo_create\0")?,
                bo_get_fd: sym(&lib, b"gbm_bo_get_fd\0")?,
                bo_get_stride: sym(&lib, b"gbm_bo_get_stride\0")?,
                bo_get_offset: sym(&lib, b"gbm_bo_get_offset\0")?,
                bo_get_modifier: sym(&lib, b"gbm_bo_get_modifier\0")?,
                bo_destroy: sym(&lib, b"gbm_bo_destroy\0")?,
                _lib: lib,
            })
        }
    }
}

// --- EGL constants (mirror kms_gpu_copy) -----------------------------------

const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_WIDTH: u32 = 0x3057;
const EGL_HEIGHT: u32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: u32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: u32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: u32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: u32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: u32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: u32 = 0x3444;
const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// --- the zero-copy path -----------------------------------------------------

pub struct CudaZeroCopy {
    cuda: CudaLib,
    gbm: GbmLib,
    gbm_device: *mut c_void,
    gbm_fd: libc::c_int,
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    cuda_ctx: CUcontext,
    hw_device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    width: u32,
    height: u32,
    consecutive_failures: u32,
    /// Whether EGL→CUDA DMA-BUF import validated. False on NVIDIA (CUDA-EGL
    /// rejects block-linear BOs); then a DMA-BUF frame trips the latch so the
    /// encoder rebuilds onto the CPU path. The RAM upload path stays usable.
    dmabuf_import_ok: bool,
}

// All GPU handles live only on the single encode thread; the encoder asserts
// Send for the whole `NvencEncoder`.
unsafe impl Send for CudaZeroCopy {}

impl CudaZeroCopy {
    /// Build the full EGL→CUDA→NVENC-frames chain for `width`x`height` and prove
    /// it works end-to-end on this GPU before returning `Ok`. Any failure returns
    /// `Err` and the caller falls back to the CPU path.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let render_node = crate::capture::linux::probe_display_gpu_render_node()
            .unwrap_or_else(|| "/dev/dri/renderD128".to_string());

        let cuda = CudaLib::load()?;
        let gbm = GbmLib::load()?;

        // gbm device on the display GPU's render node (EGL display + test BO).
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&render_node)
            .map_err(|e| format!("open {render_node}: {e}"))?;
        let gbm_fd = {
            use std::os::fd::IntoRawFd;
            file.into_raw_fd()
        };
        let gbm_device = unsafe { (gbm.create_device)(gbm_fd) };
        if gbm_device.is_null() {
            unsafe { libc::close(gbm_fd) };
            return Err("gbm_create_device failed".into());
        }

        // Wrap the partial state so every early return cleans up gbm + any
        // FFmpeg refs allocated so far. Holds the `device_destroy` fn pointer by
        // value (Copy) rather than borrowing `gbm`, so `gbm` can move into `zc`.
        let mut me = ManualCleanup {
            device_destroy: gbm.device_destroy,
            gbm_device,
            gbm_fd,
            hw_device_ref: std::ptr::null_mut(),
            frames_ref: std::ptr::null_mut(),
            armed: true,
        };

        let (egl, display) = setup_egl(gbm_device)?;

        // FFmpeg-owned CUDA device + context (we borrow the CUcontext).
        let (hw_device_ref, cuda_ctx) = create_cuda_device()?;
        me.hw_device_ref = hw_device_ref;

        // CUDA frames pool, sw_format BGR0 so NVENC converts BGRA→NV12 on-GPU.
        let frames_ref = create_cuda_frames(hw_device_ref, width, height)?;
        me.frames_ref = frames_ref;

        let mut zc = CudaZeroCopy {
            cuda,
            gbm,
            gbm_device,
            gbm_fd,
            egl,
            display,
            cuda_ctx,
            hw_device_ref,
            frames_ref,
            width,
            height,
            consecutive_failures: 0,
            dmabuf_import_ok: true,
        };

        // Ownership of gbm/ffmpeg refs has moved into `zc`, whose Drop now frees
        // them on any path — disarm the partial-state cleanup before the
        // self-test so a self-test failure doesn't double-free.
        me.armed = false;

        // End-to-end self-test: a real import + device copy on this GPU. This is
        // the regression guard the CLAUDE.md rule demands before default-on — a
        // capability probe alone ("symbols resolve") is not enough. On failure
        // `zc` drops and frees everything correctly.
        zc.self_test().map_err(|e| format!("self-test: {e}"))?;

        println!(
            "[nvenc-cuda] zero-copy DMA-BUF path enabled ({width}x{height}, render node {render_node})"
        );
        Ok(zc)
    }

    /// `AVBufferRef*` for the CUDA frames pool; the codec context takes its own
    /// `av_buffer_ref` of this when opening.
    pub fn frames_ctx_ref(&self) -> *mut ffi::AVBufferRef {
        self.frames_ref
    }

    /// Whether true DMA-BUF zero-copy import validated (false on NVIDIA, where
    /// only the RAM→CUDA upload path is used). For diagnostics/logging.
    pub fn dmabuf_import_active(&self) -> bool {
        self.dmabuf_import_ok
    }

    /// Import a captured DMA-BUF and copy it (device→device) into a fresh CUDA
    /// pool frame. Returns a CUDA `AVFrame` the caller sends to NVENC and frees.
    pub fn make_frame_from_dmabuf(
        &mut self,
        planes: &[DmaBufPlane],
        drm_format: u32,
        width: u32,
        height: u32,
    ) -> Result<*mut ffi::AVFrame, String> {
        if !self.dmabuf_import_ok {
            // Validated-unsupported at init (NVIDIA): trip the latch so the
            // encoder rebuilds onto the CPU path that can mmap this buffer,
            // instead of skipping every DMA-BUF frame.
            mark_disabled("DMA-BUF import unsupported on this GPU");
            return Err("DMA-BUF import unsupported; falling back to CPU path".into());
        }
        if planes.is_empty() {
            return Err("DMA-BUF has no planes".into());
        }
        let plane = &planes[0];
        let mut modifier = plane.modifier;
        if modifier == DRM_FORMAT_MOD_INVALID {
            modifier = DRM_FORMAT_MOD_LINEAR;
        }

        let image = create_dmabuf_image(
            &self.egl,
            self.display,
            drm_format,
            width,
            height,
            plane.fd.as_raw_fd(),
            plane.offset,
            plane.pitch,
            modifier,
        )?;

        let result = self.copy_egl_image_into_pool_frame(image, width, height);
        let _ = self.egl.destroy_image(self.display, image);
        self.track(result)
    }

    /// Upload a RAM BGRA frame into a CUDA pool frame (host→device). Used when a
    /// capture backend hands the NVENC encoder a `FrameData::Ram` while the
    /// zero-copy path is active — still skips the CPU `swscale`, NVENC converts.
    pub fn make_frame_from_ram(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<*mut ffi::AVFrame, String> {
        let src_pitch = width as usize * 4;
        if data.len() < src_pitch * height as usize {
            return Err("RAM frame smaller than width*height*4".into());
        }
        let frame = self.alloc_pool_frame()?;
        let dst_device = unsafe { (*frame).data[0] } as CUdeviceptr;
        let dst_pitch = unsafe { (*frame).linesize[0] } as usize;

        let copy = CudaMemcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: data.as_ptr() as *const c_void,
            src_device: 0,
            src_array: std::ptr::null_mut(),
            src_pitch,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device,
            dst_array: std::ptr::null_mut(),
            dst_pitch,
            width_in_bytes: src_pitch,
            height: height as usize,
        };
        let result = self.run_copy(&copy).map(|()| frame);
        if result.is_err() {
            unsafe { ffi::av_frame_free(&mut { frame }) };
        }
        self.track(result)
    }

    /// Wrap an external CUDA device buffer (BGRA, in the same primary context)
    /// into an NVENC pool frame via a device→device copy. The buffer is produced
    /// by the KMS stabiliser's GL→CUDA path on the capture thread; because both
    /// it and NVENC share the device's primary context, the pointer is valid
    /// here on the encode thread. Validated foundation, wired into the live
    /// pipeline in the follow-up stage.
    #[allow(dead_code)]
    pub fn make_frame_from_cuda_buffer(
        &mut self,
        device_ptr: u64,
        src_pitch: u32,
        width: u32,
        height: u32,
    ) -> Result<*mut ffi::AVFrame, String> {
        let frame = self.alloc_pool_frame()?;
        let dst_device = unsafe { (*frame).data[0] } as CUdeviceptr;
        let dst_pitch = unsafe { (*frame).linesize[0] } as usize;

        let copy = CudaMemcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_host: std::ptr::null(),
            src_device: device_ptr,
            src_array: std::ptr::null_mut(),
            src_pitch: src_pitch as usize,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_host: std::ptr::null_mut(),
            dst_device,
            dst_array: std::ptr::null_mut(),
            dst_pitch,
            width_in_bytes: width as usize * 4,
            height: height as usize,
        };
        let result = self.run_copy(&copy).map(|()| frame);
        if result.is_err() {
            unsafe { ffi::av_frame_free(&mut { frame }) };
        }
        self.track(result)
    }

    /// Test-only: allocate a pitched device buffer in the shared primary context
    /// and fill it with solid-blue BGRA, simulating what the stabiliser's GL→CUDA
    /// path will hand the encoder. Returns `(device_ptr, pitch)`.
    #[cfg(test)]
    pub(crate) fn test_alloc_blue(&self, width: u32, height: u32) -> Result<(u64, u32), String> {
        unsafe {
            self.push_ctx()?;
            let mut ptr: CUdeviceptr = 0;
            let mut pitch: usize = 0;
            let r = (self.cuda.mem_alloc_pitch)(
                &mut ptr,
                &mut pitch,
                width as usize * 4,
                height as usize,
                16,
            );
            if r != CUDA_SUCCESS {
                self.pop_ctx();
                return Err(format!("cuMemAllocPitch failed ({r})"));
            }
            let mut host = vec![0u8; width as usize * 4 * height as usize];
            for px in host.chunks_exact_mut(4) {
                px[0] = 255; // B
                px[3] = 255; // A
            }
            let copy = CudaMemcpy2D {
                src_x_in_bytes: 0,
                src_y: 0,
                src_memory_type: CU_MEMORYTYPE_HOST,
                src_host: host.as_ptr() as *const c_void,
                src_device: 0,
                src_array: std::ptr::null_mut(),
                src_pitch: width as usize * 4,
                dst_x_in_bytes: 0,
                dst_y: 0,
                dst_memory_type: CU_MEMORYTYPE_DEVICE,
                dst_host: std::ptr::null_mut(),
                dst_device: ptr,
                dst_array: std::ptr::null_mut(),
                dst_pitch: pitch,
                width_in_bytes: width as usize * 4,
                height: height as usize,
            };
            let r = (self.cuda.memcpy2d)(&copy);
            let s = (self.cuda.ctx_sync)();
            self.pop_ctx();
            if r != CUDA_SUCCESS || s != CUDA_SUCCESS {
                return Err(format!("fill blue failed (copy {r}, sync {s})"));
            }
            Ok((ptr, pitch as u32))
        }
    }

    #[cfg(test)]
    pub(crate) fn test_free(&self, device_ptr: u64) {
        unsafe {
            if self.push_ctx().is_ok() {
                let _ = (self.cuda.mem_free)(device_ptr);
                self.pop_ctx();
            }
        }
    }

    /// Allocate one frame from the CUDA pool (`data[0]` = pitched CUdeviceptr).
    fn alloc_pool_frame(&self) -> Result<*mut ffi::AVFrame, String> {
        unsafe {
            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                return Err("av_frame_alloc failed".into());
            }
            let ret = ffi::av_hwframe_get_buffer(self.frames_ref, frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { frame });
                return Err(format!(
                    "av_hwframe_get_buffer: {}",
                    crate::encode::ffmpeg_err(ret)
                ));
            }
            Ok(frame)
        }
    }

    fn copy_egl_image_into_pool_frame(
        &self,
        image: egl::Image,
        width: u32,
        height: u32,
    ) -> Result<*mut ffi::AVFrame, String> {
        let frame = self.alloc_pool_frame()?;
        let dst_device = unsafe { (*frame).data[0] } as CUdeviceptr;
        let dst_pitch = unsafe { (*frame).linesize[0] } as usize;

        let result = (|| -> Result<(), String> {
            unsafe {
                self.push_ctx()?;
            }
            let copy_res = self.register_map_copy(image, dst_device, dst_pitch, width, height);
            unsafe {
                self.pop_ctx();
            }
            copy_res
        })();

        match result {
            Ok(()) => Ok(frame),
            Err(e) => {
                unsafe { ffi::av_frame_free(&mut { frame }) };
                Err(e)
            }
        }
    }

    /// Register the EGLImage with CUDA, map it, and `cuMemcpy2D` into `dst`.
    /// Assumes the CUDA context is already current.
    fn register_map_copy(
        &self,
        image: egl::Image,
        dst_device: CUdeviceptr,
        dst_pitch: usize,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        unsafe {
            let mut resource: CUgraphicsResource = std::ptr::null_mut();
            let r = (self.cuda.egl_register)(
                &mut resource,
                image.as_ptr(),
                CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY,
            );
            if r != CUDA_SUCCESS {
                return Err(format!("cuGraphicsEGLRegisterImage failed ({r})"));
            }

            let mut egl_frame: CUeglFrame = std::mem::zeroed();
            let r = (self.cuda.egl_get_frame)(&mut egl_frame, resource, 0, 0);
            if r != CUDA_SUCCESS {
                (self.cuda.unregister)(resource);
                return Err(format!("cuGraphicsResourceGetMappedEglFrame failed ({r})"));
            }

            let mut copy = CudaMemcpy2D {
                src_x_in_bytes: 0,
                src_y: 0,
                src_memory_type: 0,
                src_host: std::ptr::null(),
                src_device: 0,
                src_array: std::ptr::null_mut(),
                src_pitch: 0,
                dst_x_in_bytes: 0,
                dst_y: 0,
                dst_memory_type: CU_MEMORYTYPE_DEVICE,
                dst_host: std::ptr::null_mut(),
                dst_device,
                dst_array: std::ptr::null_mut(),
                dst_pitch,
                width_in_bytes: width as usize * 4,
                height: height as usize,
            };
            if egl_frame.frame_type == CU_EGL_FRAME_TYPE_ARRAY {
                copy.src_memory_type = CU_MEMORYTYPE_ARRAY;
                copy.src_array = egl_frame.frame[0];
            } else {
                copy.src_memory_type = CU_MEMORYTYPE_DEVICE;
                copy.src_device = egl_frame.frame[0] as CUdeviceptr;
                copy.src_pitch = egl_frame.pitch as usize;
            }

            let r = (self.cuda.memcpy2d)(&copy);
            if r != CUDA_SUCCESS {
                (self.cuda.unregister)(resource);
                return Err(format!("cuMemcpy2D failed ({r})"));
            }
            // Ensure the copy completes before the source is unregistered and the
            // frame is handed to NVENC.
            let r = (self.cuda.ctx_sync)();
            (self.cuda.unregister)(resource);
            if r != CUDA_SUCCESS {
                return Err(format!("cuCtxSynchronize failed ({r})"));
            }
            Ok(())
        }
    }

    fn run_copy(&self, copy: &CudaMemcpy2D) -> Result<(), String> {
        unsafe {
            self.push_ctx()?;
            let r = (self.cuda.memcpy2d)(copy);
            let s = if r == CUDA_SUCCESS {
                (self.cuda.ctx_sync)()
            } else {
                r
            };
            self.pop_ctx();
            if r != CUDA_SUCCESS {
                return Err(format!("cuMemcpy2D failed ({r})"));
            }
            if s != CUDA_SUCCESS {
                return Err(format!("cuCtxSynchronize failed ({s})"));
            }
        }
        Ok(())
    }

    unsafe fn push_ctx(&self) -> Result<(), String> {
        let r = unsafe { (self.cuda.ctx_push)(self.cuda_ctx) };
        if r != CUDA_SUCCESS {
            return Err(format!("cuCtxPushCurrent failed ({r})"));
        }
        Ok(())
    }

    unsafe fn pop_ctx(&self) {
        let mut dummy: CUcontext = std::ptr::null_mut();
        unsafe {
            let _ = (self.cuda.ctx_pop)(&mut dummy);
        }
    }

    /// Track per-frame success/failure; trip the session latch after a run of
    /// failures so the next encoder rebuild falls back to the CPU path.
    fn track(
        &mut self,
        result: Result<*mut ffi::AVFrame, String>,
    ) -> Result<*mut ffi::AVFrame, String> {
        match &result {
            Ok(_) => self.consecutive_failures = 0,
            Err(_) => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= 30 {
                    mark_disabled("30 consecutive frame-import failures");
                }
            }
        }
        result
    }

    /// Prove the chain works end-to-end on this GPU before `new()` returns `Ok`.
    ///
    /// The **RAM upload** path is required: it's what actually runs on NVIDIA
    /// today (the KMS stabiliser can't export a linear DMA-BUF on NVIDIA and
    /// falls back to `FrameData::Ram`), so a failure here means the CUDA path is
    /// unusable → fall back to the CPU encoder.
    ///
    /// The **DMA-BUF import** path is best-effort: it covers `ST_KMS_COPY=0`
    /// (raw scanout) and a future tiled stabiliser output. We validate it with a
    /// *tiled* renderable BO (gbm rejects `GBM_BO_USE_LINEAR` for renderable
    /// targets on NVIDIA, so a tiled modifier is the only thing gbm will hand
    /// out). If gbm can't allocate the probe buffer at all we skip it rather than
    /// disable the whole path — a real per-frame import failure later is caught
    /// by the consecutive-failure latch.
    fn self_test(&mut self) -> Result<(), String> {
        let w = self.width.clamp(16, 256);
        let h = self.height.clamp(16, 256);

        // Required: RAM upload → CUDA pool frame.
        let data = vec![0u8; (w as usize) * (h as usize) * 4];
        let frame = self.make_frame_from_ram(&data, w, h)?;
        unsafe { ffi::av_frame_free(&mut { frame }) };

        // Best-effort: DMA-BUF import via a tiled renderable BO.
        unsafe {
            let bo = (self.gbm.bo_create)(
                self.gbm_device,
                w,
                h,
                DRM_FORMAT_XRGB8888,
                GBM_BO_USE_RENDERING,
            );
            if bo.is_null() {
                eprintln!(
                    "[nvenc-cuda] self-test: gbm renderable BO unavailable; DMA-BUF import unverified (RAM path validated)"
                );
                return Ok(());
            }
            let fd = (self.gbm.bo_get_fd)(bo);
            if fd < 0 {
                (self.gbm.bo_destroy)(bo);
                eprintln!(
                    "[nvenc-cuda] self-test: gbm_bo_get_fd failed; DMA-BUF import unverified"
                );
                return Ok(());
            }
            let pitch = (self.gbm.bo_get_stride)(bo);
            let offset = (self.gbm.bo_get_offset)(bo, 0);
            let modifier = (self.gbm.bo_get_modifier)(bo);

            let outcome = match create_dmabuf_image(
                &self.egl,
                self.display,
                DRM_FORMAT_XRGB8888,
                w,
                h,
                fd,
                offset,
                pitch,
                modifier,
            ) {
                Ok(image) => {
                    let r = self.copy_egl_image_into_pool_frame(image, w, h);
                    let _ = self.egl.destroy_image(self.display, image);
                    r.map(|frame| ffi::av_frame_free(&mut { frame }))
                }
                Err(e) => Err(e),
            };
            libc::close(fd);
            (self.gbm.bo_destroy)(bo);
            if let Err(e) = outcome {
                // Non-fatal: NVIDIA's CUDA-EGL interop rejects block-linear BOs
                // (the only thing gbm hands out here), and NVIDIA capture feeds
                // NVENC RAM frames anyway. Keep the (validated) RAM path; a real
                // DMA-BUF that fails to import later trips the per-frame latch.
                self.dmabuf_import_ok = false;
                eprintln!(
                    "[nvenc-cuda] self-test: DMA-BUF import unsupported on this GPU ({e}); using RAM→CUDA upload (no CPU swscale). ST_KMS_COPY=0 raw-scanout frames will fall back to the CPU path."
                );
            }
        }
        Ok(())
    }
}

impl Drop for CudaZeroCopy {
    fn drop(&mut self) {
        unsafe {
            if !self.frames_ref.is_null() {
                ffi::av_buffer_unref(&mut self.frames_ref);
            }
            if !self.hw_device_ref.is_null() {
                ffi::av_buffer_unref(&mut self.hw_device_ref);
            }
            // EGL display is owned by libEGL; terminating it could disturb other
            // displays on the same gbm device — just destroy the gbm device + fd.
            (self.gbm.device_destroy)(self.gbm_device);
            libc::close(self.gbm_fd);
        }
    }
}

/// RAII cleanup for the partially-built state inside `CudaZeroCopy::new`, so an
/// early `?` return frees gbm + any allocated FFmpeg refs without leaking.
struct ManualCleanup {
    device_destroy: FnGbmDeviceDestroy,
    gbm_device: *mut c_void,
    gbm_fd: libc::c_int,
    hw_device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    armed: bool,
}

impl Drop for ManualCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        unsafe {
            if !self.frames_ref.is_null() {
                ffi::av_buffer_unref(&mut self.frames_ref);
            }
            if !self.hw_device_ref.is_null() {
                ffi::av_buffer_unref(&mut self.hw_device_ref);
            }
            (self.device_destroy)(self.gbm_device);
            libc::close(self.gbm_fd);
        }
    }
}

// --- free helpers -----------------------------------------------------------

fn setup_egl(
    gbm_device: *mut c_void,
) -> Result<(egl::DynamicInstance<egl::EGL1_5>, egl::Display), String> {
    let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
        .map_err(|e| format!("load libEGL: {e:?}"))?;
    let display =
        unsafe { egl.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE]) }
            .map_err(|e| format!("eglGetPlatformDisplay(GBM): {e:?}"))?;
    egl.initialize(display)
        .map_err(|e| format!("eglInitialize: {e:?}"))?;
    let extensions = egl
        .query_string(Some(display), egl::EXTENSIONS)
        .map_err(|e| format!("eglQueryString(EXTENSIONS): {e:?}"))?
        .to_string_lossy()
        .into_owned();
    if !extensions.contains("EGL_EXT_image_dma_buf_import") {
        return Err("EGL_EXT_image_dma_buf_import not available".into());
    }
    Ok((egl, display))
}

/// Create the FFmpeg CUDA device and return `(device_ref, borrowed CUcontext)`.
/// The `CUcontext` is read as the first field of `AVCUDADeviceContext`
/// (`ffmpeg-sys-next` does not bind that struct), which is stable public ABI.
fn create_cuda_device() -> Result<(*mut ffi::AVBufferRef, CUcontext), String> {
    unsafe {
        // AV_CUDA_USE_PRIMARY_CONTEXT: bind FFmpeg's NVENC to the device's CUDA
        // *primary* context. The KMS stabiliser independently
        // `cuDevicePrimaryCtxRetain`s the same context, so a GPU buffer it fills
        // there (capture thread) is a valid device pointer in NVENC (encode
        // thread) — the basis of the future GL→CUDA readback-elimination path.
        const AV_CUDA_USE_PRIMARY_CONTEXT: i32 = 1;
        let mut device_ref: *mut ffi::AVBufferRef = std::ptr::null_mut();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            std::ptr::null(),
            std::ptr::null_mut(),
            AV_CUDA_USE_PRIMARY_CONTEXT,
        );
        if ret < 0 {
            return Err(format!(
                "av_hwdevice_ctx_create(CUDA): {}",
                crate::encode::ffmpeg_err(ret)
            ));
        }
        let dev_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
        let hwctx = (*dev_ctx).hwctx;
        if hwctx.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            return Err("CUDA hwctx is null".into());
        }
        let cuda_ctx = *(hwctx as *const CUcontext);
        if cuda_ctx.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            return Err("borrowed CUcontext is null".into());
        }
        Ok((device_ref, cuda_ctx))
    }
}

fn create_cuda_frames(
    device_ref: *mut ffi::AVBufferRef,
    width: u32,
    height: u32,
) -> Result<*mut ffi::AVBufferRef, String> {
    unsafe {
        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            return Err("av_hwframe_ctx_alloc failed".into());
        }
        let frames = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*frames).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0;
        (*frames).width = width as i32;
        (*frames).height = height as i32;
        // A handful of in-flight pool frames; NVENC keeps one surface but the
        // pool must outlast a frame held by the encoder while we fetch the next.
        (*frames).initial_pool_size = 8;
        let ret = ffi::av_hwframe_ctx_init(frames_ref);
        if ret < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            return Err(format!(
                "av_hwframe_ctx_init: {}",
                crate::encode::ffmpeg_err(ret)
            ));
        }
        Ok(frames_ref)
    }
}

#[allow(clippy::too_many_arguments)]
fn create_dmabuf_image(
    egl: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    drm_format: u32,
    width: u32,
    height: u32,
    fd: libc::c_int,
    offset: u32,
    pitch: u32,
    modifier: u64,
) -> Result<egl::Image, String> {
    let mut attrs = vec![
        EGL_WIDTH as egl::Attrib,
        width as egl::Attrib,
        EGL_HEIGHT as egl::Attrib,
        height as egl::Attrib,
        EGL_LINUX_DRM_FOURCC_EXT as egl::Attrib,
        drm_format as egl::Attrib,
        EGL_DMA_BUF_PLANE0_FD_EXT as egl::Attrib,
        fd as egl::Attrib,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT as egl::Attrib,
        offset as egl::Attrib,
        EGL_DMA_BUF_PLANE0_PITCH_EXT as egl::Attrib,
        pitch as egl::Attrib,
    ];
    if modifier != DRM_FORMAT_MOD_INVALID && modifier != DRM_FORMAT_MOD_LINEAR {
        attrs.push(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT as egl::Attrib);
        attrs.push((modifier as u32) as egl::Attrib);
        attrs.push(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT as egl::Attrib);
        attrs.push((modifier >> 32) as egl::Attrib);
    }
    attrs.push(egl::ATTRIB_NONE);

    egl.create_image(
        display,
        unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) },
        EGL_LINUX_DMA_BUF_EXT,
        unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) },
        &attrs,
    )
    .map_err(|e| format!("eglCreateImage(dmabuf): {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live GPU validation. Opt-in via `ST_TEST_NVENC_CUDA=1` so headless CI
    /// without an NVIDIA GPU stays green; run here on the RTX 4080.
    fn live_enabled() -> bool {
        std::env::var("ST_TEST_NVENC_CUDA").as_deref() == Ok("1")
    }

    #[test]
    fn cuda_zero_copy_init_and_import() {
        if !live_enabled() {
            eprintln!("skip: set ST_TEST_NVENC_CUDA=1 for live GPU validation");
            return;
        }
        // `new()` runs the end-to-end self-test internally (gbm linear BGRA →
        // EGL import → cuGraphicsEGLRegisterImage → cuMemcpy2D into a pool frame),
        // so a successful return proves the whole interop chain on this driver.
        let mut zc = CudaZeroCopy::new(640, 480).expect("CudaZeroCopy::new on NVIDIA");
        // Exercise the RAM upload path too.
        let data = vec![0x40u8; 640 * 480 * 4];
        let frame = zc.make_frame_from_ram(&data, 640, 480).expect("ram upload");
        assert!(!frame.is_null());
        unsafe {
            ffi::av_frame_free(&mut { frame });
        }
    }

    /// Full encode through NVENC's CUDA frames pool: proves NVENC accepts the
    /// `sw_format = BGR0` CUDA frames and emits a decodable IDR.
    #[test]
    fn nvenc_cuda_encodes_idr() {
        if !live_enabled() {
            return;
        }
        use crate::capture::{CapturedFrame, FrameData};
        use crate::encode::NvencEncoder;
        use crate::encode_config::{Codec, EncoderConfig};

        let config = EncoderConfig::from_env_with_framerate_and_codec(640, 480, 60, Codec::H264);
        let mut enc = NvencEncoder::with_config(&config).expect("nvenc encoder");
        assert!(
            enc.cuda_active(),
            "CUDA zero-copy path should be active on this GPU"
        );

        let frame = CapturedFrame {
            data: FrameData::Ram(vec![0x40u8; 640 * 480 * 4]),
            width: 640,
            height: 480,
            cursor: None,
            force_keyframe: false,
        };
        enc.reset_for_keyframe();
        let mut nals = enc.encode(&frame).expect("encode frame 0");
        if nals.is_empty() {
            // Some setups emit on the second submission; feed one more.
            nals = enc.encode(&frame).expect("encode frame 1");
        }
        assert!(!nals.is_empty(), "NVENC produced no output");
        // Annex-B start code on the first unit confirms real encoded bytes.
        let first = &nals[0].data;
        assert!(
            first.starts_with(&[0, 0, 0, 1]) || first.starts_with(&[0, 0, 1]),
            "expected an Annex-B NAL start code, got {:02x?}",
            &first[..first.len().min(8)]
        );
        assert!(
            nals.iter().any(|u| u.is_recovery),
            "forced keyframe should yield an IDR"
        );
    }

    /// Colour-correctness guard for default-on: a solid-blue BGRA frame pushed
    /// through the CUDA path (NVENC's on-GPU BGR0→NV12) must decode back to a
    /// blue-dominant pixel. Catches a wrong conversion matrix or a byte-order
    /// swap (which would decode as red), the realistic ways the GPU convert
    /// could silently corrupt colour for every NVIDIA user.
    #[test]
    fn nvenc_cuda_color_is_not_swapped() {
        if !live_enabled() {
            return;
        }
        use crate::capture::{CapturedFrame, FrameData};
        use crate::encode::NvencEncoder;
        use crate::encode_config::{Codec, EncoderConfig};

        let (w, h) = (320u32, 240u32);
        // BGRA solid blue: bytes B=255, G=0, R=0, A=255.
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.chunks_exact_mut(4) {
            px[0] = 255;
            px[2] = 0;
            px[3] = 255;
        }

        let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 60, Codec::H264);
        let mut enc = NvencEncoder::with_config(&config).expect("nvenc encoder");
        assert!(enc.cuda_active(), "CUDA path should be active");

        let frame = CapturedFrame {
            data: FrameData::Ram(data),
            width: w,
            height: h,
            cursor: None,
            force_keyframe: true,
        };
        enc.reset_for_keyframe();
        // Feed a few frames so the decoder has a clean IDR to lock onto.
        let mut nals = Vec::new();
        for _ in 0..3 {
            nals.extend(
                enc.encode(&frame)
                    .expect("encode")
                    .into_iter()
                    .map(|u| u.data),
            );
        }
        nals.extend(enc.flush().into_iter().map(|u| u.data));
        let bitstream: Vec<u8> = nals.concat();

        let (b, g, r) = decode_center_bgr(&bitstream, w, h).expect("decode center pixel");
        eprintln!("[test] decoded centre BGR = ({b},{g},{r})");
        assert!(
            b as i32 > r as i32 + 40 && b as i32 > g as i32 + 40,
            "expected blue-dominant pixel, got B={b} G={g} R={r} (matrix or byte-order error?)"
        );
    }

    /// Stage-1 proof for the GL→CUDA readback-elimination path: an external CUDA
    /// device buffer (allocated outside FFmpeg's pool, in the shared *primary*
    /// context — exactly what the stabiliser will produce) encodes through NVENC
    /// and decodes back colour-correct. Validates the primary-context sharing
    /// model and `make_frame_from_cuda_buffer` before the pipeline integration.
    #[test]
    fn nvenc_cuda_encodes_external_device_buffer() {
        if !live_enabled() {
            return;
        }
        use crate::encode::NvencEncoder;
        use crate::encode_config::{Codec, EncoderConfig};

        let (w, h) = (320u32, 240u32);
        let config = EncoderConfig::from_env_with_framerate_and_codec(w, h, 60, Codec::H264);
        let mut enc = NvencEncoder::with_config(&config).expect("nvenc encoder");
        assert!(enc.cuda_active(), "CUDA path should be active");

        let (ptr, pitch) = enc
            .cuda_test_alloc_blue(w, h)
            .expect("alloc blue device buffer in primary context");
        enc.reset_for_keyframe();
        let mut nals = Vec::new();
        for _ in 0..3 {
            nals.extend(
                enc.encode_cuda_buffer(ptr, pitch, w, h)
                    .expect("encode external buffer")
                    .into_iter()
                    .map(|u| u.data),
            );
        }
        nals.extend(enc.flush().into_iter().map(|u| u.data));
        enc.cuda_test_free(ptr);

        let bitstream: Vec<u8> = nals.concat();
        let (b, g, r) = decode_center_bgr(&bitstream, w, h).expect("decode centre pixel");
        eprintln!("[test] external-buffer decoded centre BGR = ({b},{g},{r})");
        assert!(
            b as i32 > r as i32 + 40 && b as i32 > g as i32 + 40,
            "external CUDA buffer should encode blue-dominant, got B={b} G={g} R={r}"
        );
    }

    /// Decode an H.264 Annex-B bitstream and return the centre pixel as (B,G,R).
    fn decode_center_bgr(bitstream: &[u8], w: u32, h: u32) -> Option<(u8, u8, u8)> {
        unsafe {
            let dec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
            if dec.is_null() {
                return None;
            }
            let dctx = ffi::avcodec_alloc_context3(dec);
            if dctx.is_null() {
                return None;
            }
            if ffi::avcodec_open2(dctx, dec, std::ptr::null_mut()) < 0 {
                ffi::avcodec_free_context(&mut { dctx });
                return None;
            }
            let pkt = ffi::av_packet_alloc();
            let frame = ffi::av_frame_alloc();
            (*pkt).data = bitstream.as_ptr() as *mut u8;
            (*pkt).size = bitstream.len() as i32;
            let mut result = None;
            if ffi::avcodec_send_packet(dctx, pkt) >= 0 {
                let _ = ffi::avcodec_send_packet(dctx, std::ptr::null()); // flush
                while ffi::avcodec_receive_frame(dctx, frame) >= 0 {
                    // Convert the decoded frame to BGRA and read the centre pixel.
                    let sws = ffi::sws_getContext(
                        w as i32,
                        h as i32,
                        std::mem::transmute::<i32, ffi::AVPixelFormat>((*frame).format),
                        w as i32,
                        h as i32,
                        ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                        2, // SWS_BILINEAR
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    );
                    if !sws.is_null() {
                        let stride = (w * 4) as i32;
                        let mut buf = vec![0u8; (w * h * 4) as usize];
                        let dst = [
                            buf.as_mut_ptr(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        ];
                        let dst_stride = [stride, 0, 0, 0];
                        ffi::sws_scale(
                            sws,
                            (*frame).data.as_ptr() as *const *const u8,
                            (*frame).linesize.as_ptr(),
                            0,
                            h as i32,
                            dst.as_ptr(),
                            dst_stride.as_ptr(),
                        );
                        ffi::sws_freeContext(sws);
                        let idx = (((h / 2) * w + w / 2) * 4) as usize;
                        result = Some((buf[idx], buf[idx + 1], buf[idx + 2]));
                    }
                }
            }
            ffi::av_frame_free(&mut { frame });
            ffi::av_packet_free(&mut { pkt });
            ffi::avcodec_free_context(&mut { dctx });
            result
        }
    }
}
