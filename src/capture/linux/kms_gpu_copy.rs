//! GPU stabilizing copy for KMS scanout capture.
//!
//! `kms_capture` PRIME-exports the compositor's *live* scanout framebuffer. The
//! compositor (KWin) double/triple-buffers and page-flips on its own cadence,
//! so handing that DMA-BUF straight to the asynchronous encoder lets the buffer
//! be overwritten mid-encode → **tearing** plus apparent **latest/previous
//! frame jumping** on rapid motion (invisible on static content because
//! consecutive buffers are near-identical). See CLAUDE.md "Wayland regression
//! guards".
//!
//! The fix here decouples the encoder from the compositor's buffer cycle: each
//! captured scanout buffer is imported as an EGLImage and blitted into a
//! private, **linear** DMA-BUF taken from a small ring, with a `glFinish()`
//! barrier so the copy is complete before we return — at which point KWin may
//! freely flip/overwrite the original. The encoder then imports a buffer the
//! compositor can never touch. Default-on (validated live); `ST_KMS_COPY=0`
//! forces the old direct path.
//!
//! Lifetime: a ring slot is only returned to the free pool when the encoder
//! *drops* the `CapturedFrame` (via [`FrameLease`]), so a slot is never reused
//! while still in-flight to the encoder. The GL objects (textures/FBOs/EGL
//! images) live on the capture thread and are never touched from the encoder
//! thread — only the slot index travels back over a channel.
//!
//! Surfaceless EGL/GLES2 on a gbm device; gbm is loaded dynamically (matching
//! `gbm_probe`), EGL via `khronos-egl`, GL via `glow`.

use super::super::{DmaBufPlane, FrameData, FrameLease, FrameLeaseOps};
use crossbeam_channel::{Receiver, Sender};
use glow::HasContext as _;
use khronos_egl as egl;
use libloading::Library;
use std::ffi::c_void;
use std::os::fd::{FromRawFd, OwnedFd};

// --- DRM / gbm constants ---------------------------------------------------

const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_USE_LINEAR: u32 = 1 << 4;

// --- EGL extension constants (not in khronos-egl core) ---------------------

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

type ImageTargetTexture2DOes = extern "system" fn(target: u32, image: *const c_void);

// --- gbm dynamic loader ----------------------------------------------------

type GbmCreateDevice = unsafe extern "C" fn(fd: libc::c_int) -> *mut c_void;
type GbmDeviceDestroy = unsafe extern "C" fn(dev: *mut c_void);
type GbmBoCreate = unsafe extern "C" fn(
    dev: *mut c_void,
    width: u32,
    height: u32,
    format: u32,
    usage: u32,
) -> *mut c_void;
type GbmBoGetFd = unsafe extern "C" fn(bo: *const c_void) -> libc::c_int;
type GbmBoGetStride = unsafe extern "C" fn(bo: *const c_void) -> u32;
type GbmBoGetOffset = unsafe extern "C" fn(bo: *const c_void, plane: libc::c_int) -> u32;
type GbmBoGetModifier = unsafe extern "C" fn(bo: *const c_void) -> u64;
type GbmBoDestroy = unsafe extern "C" fn(bo: *mut c_void);

struct GbmLib {
    _lib: Library,
    create_device: GbmCreateDevice,
    device_destroy: GbmDeviceDestroy,
    bo_create: GbmBoCreate,
    bo_get_fd: GbmBoGetFd,
    bo_get_stride: GbmBoGetStride,
    bo_get_offset: GbmBoGetOffset,
    bo_get_modifier: GbmBoGetModifier,
    bo_destroy: GbmBoDestroy,
}

impl GbmLib {
    fn load() -> Result<Self, String> {
        let lib = ["libgbm.so.1", "libgbm.so"]
            .iter()
            .find_map(|n| unsafe { Library::new(n).ok() })
            .ok_or_else(|| "libgbm.so.1 not found".to_string())?;
        unsafe {
            Ok(Self {
                create_device: *lib
                    .get::<GbmCreateDevice>(b"gbm_create_device\0")
                    .map_err(|e| format!("gbm_create_device: {e}"))?,
                device_destroy: *lib
                    .get::<GbmDeviceDestroy>(b"gbm_device_destroy\0")
                    .map_err(|e| format!("gbm_device_destroy: {e}"))?,
                bo_create: *lib
                    .get::<GbmBoCreate>(b"gbm_bo_create\0")
                    .map_err(|e| format!("gbm_bo_create: {e}"))?,
                bo_get_fd: *lib
                    .get::<GbmBoGetFd>(b"gbm_bo_get_fd\0")
                    .map_err(|e| format!("gbm_bo_get_fd: {e}"))?,
                bo_get_stride: *lib
                    .get::<GbmBoGetStride>(b"gbm_bo_get_stride\0")
                    .map_err(|e| format!("gbm_bo_get_stride: {e}"))?,
                bo_get_offset: *lib
                    .get::<GbmBoGetOffset>(b"gbm_bo_get_offset\0")
                    .map_err(|e| format!("gbm_bo_get_offset: {e}"))?,
                bo_get_modifier: *lib
                    .get::<GbmBoGetModifier>(b"gbm_bo_get_modifier\0")
                    .map_err(|e| format!("gbm_bo_get_modifier: {e}"))?,
                bo_destroy: *lib
                    .get::<GbmBoDestroy>(b"gbm_bo_destroy\0")
                    .map_err(|e| format!("gbm_bo_destroy: {e}"))?,
                _lib: lib,
            })
        }
    }
}

// --- slot pool (unit-testable bookkeeping) ---------------------------------

/// Tracks which ring slots are free vs. in-flight to the encoder. Slots are
/// reclaimed only when their `FrameLease` drops, so the GPU copy in a slot is
/// never overwritten while the encoder may still be reading it.
struct SlotPool {
    in_use: Vec<bool>,
    free_tx: Sender<usize>,
    free_rx: Receiver<usize>,
}

impl SlotPool {
    fn new(count: usize) -> Self {
        let (free_tx, free_rx) = crossbeam_channel::unbounded();
        Self {
            in_use: vec![false; count],
            free_tx,
            free_rx,
        }
    }

    /// Reclaim every slot whose lease has been dropped since the last call.
    fn drain_returns(&mut self) {
        while let Ok(idx) = self.free_rx.try_recv() {
            if let Some(slot) = self.in_use.get_mut(idx) {
                *slot = false;
            }
        }
    }

    /// Reserve a free slot, marking it in-use. Returns `None` when every slot
    /// is still in-flight (caller must drop the frame rather than alias).
    fn acquire(&mut self) -> Option<usize> {
        self.drain_returns();
        let idx = self.in_use.iter().position(|&used| !used)?;
        self.in_use[idx] = true;
        Some(idx)
    }

    /// Build the lease that returns `idx` to the pool when the encoder drops
    /// the frame.
    fn lease(&self, idx: usize) -> FrameLease {
        FrameLease::new(SlotReturn {
            idx,
            free_tx: self.free_tx.clone(),
        })
    }
}

struct SlotReturn {
    idx: usize,
    free_tx: Sender<usize>,
}

impl FrameLeaseOps for SlotReturn {
    fn release(&mut self) {
        // Encoder is done with the buffer; hand the slot back. The capture
        // thread reclaims it on its next `acquire`. No GL calls here — this
        // runs on the encoder thread.
        let _ = self.free_tx.send(self.idx);
    }
}

// --- GPU ring slot ---------------------------------------------------------

struct RingSlot {
    bo: *mut c_void,
    image: egl::Image,
    texture: glow::Texture,
    framebuffer: glow::Framebuffer,
    stride: u32,
    offset: u32,
    modifier: u64,
}

/// Number of private linear target buffers. Sized above the capture channel
/// depth (`CAPTURE_QUEUE_CAPACITY` = 4) plus the frame being encoded, so a slot
/// is essentially always free; transient exhaustion only drops a frame, never
/// aliases. At 4K XRGB8888 each slot is ~33 MB (≈265 MB VRAM for 8).
const RING_SLOTS: usize = 8;

pub struct KmsStabilizer {
    gbm: GbmLib,
    gbm_device: *mut c_void,
    /// fd backing the gbm device. gbm does not take ownership, so we keep it
    /// open for the device's lifetime and close it in `Drop` *after*
    /// `gbm_device_destroy`.
    device_fd: libc::c_int,
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    image_target_texture_2d: ImageTargetTexture2DOes,
    program: glow::Program,
    vbo: glow::Buffer,
    src_texture: glow::Texture,
    pool: SlotPool,
    slots: Vec<RingSlot>,
    width: u32,
    height: u32,
    drm_format: u32,
}

impl KmsStabilizer {
    /// Build a surfaceless EGL/GLES context on `render_node` and probe the
    /// extensions required for zero-copy import/export. Returns `Err` (and the
    /// caller falls back to the direct path) if anything is unavailable.
    pub fn new(render_node: &str) -> Result<Self, String> {
        let gbm = GbmLib::load()?;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
            .map_err(|e| format!("open {render_node}: {e}"))?;
        // gbm takes ownership of the fd for the device's lifetime; leak the
        // File into a raw fd held alongside the device and closed on Drop.
        let fd = {
            use std::os::fd::IntoRawFd;
            file.into_raw_fd()
        };
        let gbm_device = unsafe { (gbm.create_device)(fd) };
        if gbm_device.is_null() {
            unsafe { libc::close(fd) };
            return Err("gbm_create_device failed".into());
        }

        // GL FBO origin is bottom-left while scanout memory is top-down; flip
        // the sampled Y so the copied buffer keeps the source's row order.
        // `ST_KMS_COPY_FLIP=0` disables it if a driver already matches.
        let flip_y = !matches!(std::env::var("ST_KMS_COPY_FLIP").as_deref(), Ok("0"));

        let setup = (|| -> Result<_, String> {
            let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
                .map_err(|e| format!("load libEGL: {e:?}"))?;

            let display = unsafe {
                egl.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE])
            }
            .map_err(|e| format!("eglGetPlatformDisplay(GBM): {e:?}"))?;

            egl.initialize(display)
                .map_err(|e| format!("eglInitialize: {e:?}"))?;

            let extensions = egl
                .query_string(Some(display), egl::EXTENSIONS)
                .map_err(|e| format!("eglQueryString(EXTENSIONS): {e:?}"))?
                .to_string_lossy()
                .into_owned();
            for required in [
                "EGL_EXT_image_dma_buf_import",
                "EGL_KHR_surfaceless_context",
            ] {
                if !extensions.contains(required) {
                    return Err(format!("{required} not available"));
                }
            }

            egl.bind_api(egl::OPENGL_ES_API)
                .map_err(|e| format!("eglBindAPI: {e:?}"))?;

            let config = egl
                .choose_first_config(
                    display,
                    &[
                        egl::SURFACE_TYPE,
                        egl::PBUFFER_BIT,
                        egl::RENDERABLE_TYPE,
                        egl::OPENGL_ES2_BIT,
                        egl::NONE,
                    ],
                )
                .map_err(|e| format!("eglChooseConfig: {e:?}"))?
                .ok_or_else(|| "no suitable EGL config".to_string())?;

            let context = egl
                .create_context(
                    display,
                    config,
                    None,
                    &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
                )
                .map_err(|e| format!("eglCreateContext: {e:?}"))?;

            egl.make_current(display, None, None, Some(context))
                .map_err(|e| format!("eglMakeCurrent(surfaceless): {e:?}"))?;

            let gl = unsafe {
                glow::Context::from_loader_function(|name| match egl.get_proc_address(name) {
                    Some(ptr) => ptr as *const c_void,
                    None => std::ptr::null(),
                })
            };

            if !gl.supported_extensions().contains("GL_OES_EGL_image") {
                return Err("GL_OES_EGL_image not available".into());
            }
            let image_target_texture_2d: ImageTargetTexture2DOes = unsafe {
                std::mem::transmute::<extern "system" fn(), ImageTargetTexture2DOes>(
                    egl.get_proc_address("glEGLImageTargetTexture2DOES")
                        .ok_or_else(|| "glEGLImageTargetTexture2DOES unavailable".to_string())?,
                )
            };

            Ok((egl, display, context, gl, image_target_texture_2d))
        })();

        let (egl, display, context, gl, image_target_texture_2d) = match setup {
            Ok(v) => v,
            Err(e) => {
                unsafe { (gbm.device_destroy)(gbm_device) };
                unsafe { libc::close(fd) };
                return Err(e);
            }
        };

        let (program, vbo, src_texture) = match build_gl_program(&gl, flip_y) {
            Ok(v) => v,
            Err(e) => {
                unsafe { (gbm.device_destroy)(gbm_device) };
                unsafe { libc::close(fd) };
                return Err(e);
            }
        };

        Ok(Self {
            gbm,
            gbm_device,
            device_fd: fd,
            egl,
            display,
            context,
            gl,
            image_target_texture_2d,
            program,
            vbo,
            src_texture,
            pool: SlotPool::new(RING_SLOTS),
            slots: Vec::new(),
            width: 0,
            height: 0,
            drm_format: 0,
        })
    }

    /// Copy one captured scanout buffer into a private linear DMA-BUF and
    /// return a stable `FrameData::DmaBuf` for it. The source planes are the
    /// raw scanout buffer; `drm_format`/`width`/`height` describe it.
    pub fn stabilize(
        &mut self,
        src_planes: &[DmaBufPlane],
        drm_format: u32,
        width: u32,
        height: u32,
    ) -> Result<FrameData, String> {
        if src_planes.len() != 1 {
            return Err(format!(
                "KMS copy supports single-plane scanout only (got {} planes)",
                src_planes.len()
            ));
        }
        self.ensure_targets(width, height, drm_format)?;

        let idx = self
            .pool
            .acquire()
            .ok_or_else(|| "all stabilizer ring slots in-flight".to_string())?;

        let result = self.blit_into(idx, &src_planes[0], drm_format, width, height);
        match result {
            Ok(()) => {
                let slot = &self.slots[idx];
                let raw_fd = unsafe { (self.gbm.bo_get_fd)(slot.bo) };
                if raw_fd < 0 {
                    // Reclaim the slot immediately — nothing left the building.
                    self.pool.in_use[idx] = false;
                    return Err("gbm_bo_get_fd on target failed".into());
                }
                let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
                Ok(FrameData::DmaBuf {
                    planes: vec![DmaBufPlane {
                        fd,
                        offset: slot.offset,
                        pitch: slot.stride,
                        modifier: slot.modifier,
                    }],
                    drm_format,
                    _lease: Some(self.pool.lease(idx)),
                })
            }
            Err(e) => {
                self.pool.in_use[idx] = false;
                Err(e)
            }
        }
    }

    fn blit_into(
        &mut self,
        idx: usize,
        src: &DmaBufPlane,
        drm_format: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let src_fd = {
            use std::os::fd::AsRawFd;
            src.fd.as_raw_fd()
        };
        let src_image = create_dmabuf_image(
            &self.egl,
            self.display,
            drm_format,
            width,
            height,
            src_fd,
            src.offset,
            src.pitch,
            src.modifier,
        )?;

        let gl = &self.gl;
        let fbo = self.slots[idx].framebuffer;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.src_texture));
            (self.image_target_texture_2d)(glow::TEXTURE_2D, src_image.as_ptr() as *const c_void);

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            if gl.check_framebuffer_status(glow::FRAMEBUFFER) != glow::FRAMEBUFFER_COMPLETE {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.bind_texture(glow::TEXTURE_2D, None);
                let _ = self.egl.destroy_image(self.display, src_image);
                return Err("stabilizer FBO incomplete".into());
            }
            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.src_texture));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Barrier: the copy must be finished on the GPU before we return,
            // so the caller can let KWin overwrite the source and so the
            // encoder reads completed pixels.
            gl.finish();

            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.use_program(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        let _ = self.egl.destroy_image(self.display, src_image);
        Ok(())
    }

    fn ensure_targets(&mut self, width: u32, height: u32, drm_format: u32) -> Result<(), String> {
        if self.width == width && self.height == height && self.drm_format == drm_format {
            return Ok(());
        }
        // Resolution/format change: every in-flight slot points at the old
        // geometry. Wait for them to drain is impractical here, so we destroy
        // and rebuild; in-flight leases simply return to a fresh pool whose
        // slots are valid indices (their `in_use=false` is harmless).
        self.destroy_slots();
        self.pool = SlotPool::new(RING_SLOTS);

        for _ in 0..RING_SLOTS {
            let slot = self.create_slot(width, height, drm_format)?;
            self.slots.push(slot);
        }
        self.width = width;
        self.height = height;
        self.drm_format = drm_format;
        Ok(())
    }

    fn create_slot(&self, width: u32, height: u32, drm_format: u32) -> Result<RingSlot, String> {
        let bo = unsafe {
            (self.gbm.bo_create)(
                self.gbm_device,
                width,
                height,
                drm_format,
                GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
            )
        };
        if bo.is_null() {
            return Err("gbm_bo_create(linear target) failed".into());
        }
        let stride = unsafe { (self.gbm.bo_get_stride)(bo) };
        let offset = unsafe { (self.gbm.bo_get_offset)(bo, 0) };
        let mut modifier = unsafe { (self.gbm.bo_get_modifier)(bo) };
        if modifier == DRM_FORMAT_MOD_INVALID {
            modifier = DRM_FORMAT_MOD_LINEAR;
        }
        let import_fd = unsafe { (self.gbm.bo_get_fd)(bo) };
        if import_fd < 0 {
            unsafe { (self.gbm.bo_destroy)(bo) };
            return Err("gbm_bo_get_fd(target) failed".into());
        }
        // EGL dups the fd it imports; close ours once the image is created.
        let image = create_dmabuf_image(
            &self.egl,
            self.display,
            drm_format,
            width,
            height,
            import_fd,
            offset,
            stride,
            modifier,
        );
        unsafe { libc::close(import_fd) };
        let image = match image {
            Ok(img) => img,
            Err(e) => {
                unsafe { (self.gbm.bo_destroy)(bo) };
                return Err(e);
            }
        };

        let gl = &self.gl;
        unsafe {
            let texture = gl
                .create_texture()
                .map_err(|e| format!("create target texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            (self.image_target_texture_2d)(glow::TEXTURE_2D, image.as_ptr() as *const c_void);

            let framebuffer = gl
                .create_framebuffer()
                .map_err(|e| format!("create target FBO: {e}"))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(framebuffer);
                gl.delete_texture(texture);
                let _ = self.egl.destroy_image(self.display, image);
                (self.gbm.bo_destroy)(bo);
                return Err(format!("target FBO incomplete (status {status:#x})"));
            }

            Ok(RingSlot {
                bo,
                image,
                texture,
                framebuffer,
                stride,
                offset,
                modifier,
            })
        }
    }

    fn destroy_slots(&mut self) {
        for slot in self.slots.drain(..) {
            unsafe {
                self.gl.delete_framebuffer(slot.framebuffer);
                self.gl.delete_texture(slot.texture);
                let _ = self.egl.destroy_image(self.display, slot.image);
                (self.gbm.bo_destroy)(slot.bo);
            }
        }
    }
}

impl Drop for KmsStabilizer {
    fn drop(&mut self) {
        self.destroy_slots();
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_texture(self.src_texture);
            let _ = self.egl.make_current(self.display, None, None, None);
            let _ = self.egl.destroy_context(self.display, self.context);
            let _ = self.egl.terminate(self.display);
            (self.gbm.device_destroy)(self.gbm_device);
            // gbm did not own the fd; close it now that the device is gone.
            libc::close(self.device_fd);
        }
    }
}

// SAFETY: a `KmsStabilizer` is created and used entirely on the single KMS
// capture thread; it is never shared across threads. The only cross-thread
// hand-off is the slot index returned via the `SlotReturn` channel, which
// touches no GL/EGL/gbm state.
unsafe impl Send for KmsStabilizer {}

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

fn build_gl_program(
    gl: &glow::Context,
    flip_y: bool,
) -> Result<(glow::Program, glow::Buffer, glow::Texture), String> {
    // Fullscreen-quad passthrough. `flip_y` is baked into the vertex UVs at
    // upload time (see `quad_vertices`). GLES2 / GLSL ES 1.00.
    const VERT: &str = "attribute vec2 a_pos;\n\
attribute vec2 a_uv;\n\
varying vec2 v_uv;\n\
void main() {\n\
    v_uv = a_uv;\n\
    gl_Position = vec4(a_pos, 0.0, 1.0);\n\
}\n";
    const FRAG: &str = "precision mediump float;\n\
varying vec2 v_uv;\n\
uniform sampler2D u_src;\n\
void main() {\n\
    gl_FragColor = vec4(texture2D(u_src, v_uv).rgb, 1.0);\n\
}\n";

    unsafe {
        let vert = gl
            .create_shader(glow::VERTEX_SHADER)
            .map_err(|e| format!("create vertex shader: {e}"))?;
        gl.shader_source(vert, VERT);
        gl.compile_shader(vert);
        if !gl.get_shader_compile_status(vert) {
            let log = gl.get_shader_info_log(vert);
            gl.delete_shader(vert);
            return Err(format!("vertex shader: {log}"));
        }
        let frag = gl
            .create_shader(glow::FRAGMENT_SHADER)
            .map_err(|e| format!("create fragment shader: {e}"))?;
        gl.shader_source(frag, FRAG);
        gl.compile_shader(frag);
        if !gl.get_shader_compile_status(frag) {
            let log = gl.get_shader_info_log(frag);
            gl.delete_shader(vert);
            gl.delete_shader(frag);
            return Err(format!("fragment shader: {log}"));
        }
        let program = gl
            .create_program()
            .map_err(|e| format!("create program: {e}"))?;
        gl.attach_shader(program, vert);
        gl.attach_shader(program, frag);
        gl.bind_attrib_location(program, 0, "a_pos");
        gl.bind_attrib_location(program, 1, "a_uv");
        gl.link_program(program);
        gl.detach_shader(program, vert);
        gl.detach_shader(program, frag);
        gl.delete_shader(vert);
        gl.delete_shader(frag);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(format!("link: {log}"));
        }
        gl.use_program(Some(program));
        if let Some(loc) = gl.get_uniform_location(program, "u_src") {
            gl.uniform_1_i32(Some(&loc), 0);
        }
        gl.use_program(None);

        let vbo = gl.create_buffer().map_err(|e| format!("create vbo: {e}"))?;
        let quad = quad_vertices(flip_y);
        let quad_bytes =
            std::slice::from_raw_parts(quad.as_ptr() as *const u8, std::mem::size_of_val(&quad));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, quad_bytes, glow::STATIC_DRAW);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        let src_texture = gl
            .create_texture()
            .map_err(|e| format!("create src texture: {e}"))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(src_texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);

        Ok((program, vbo, src_texture))
    }
}

/// Interleaved `[x, y, u, v]` for a `TRIANGLE_STRIP` fullscreen quad. When
/// `flip_y` is set the V coordinate is inverted so the FBO (bottom-left origin)
/// writes the source's top-down rows in their original order.
fn quad_vertices(flip_y: bool) -> [f32; 16] {
    let (v0, v1) = if flip_y { (1.0, 0.0) } else { (0.0, 1.0) };
    [
        -1.0, -1.0, 0.0, v0, // bottom-left
        1.0, -1.0, 1.0, v0, // bottom-right
        -1.0, 1.0, 0.0, v1, // top-left
        1.0, 1.0, 1.0, v1, // top-right
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_pool_reuses_only_after_lease_drop() {
        let mut pool = SlotPool::new(2);
        let a = pool.acquire().expect("slot a");
        let b = pool.acquire().expect("slot b");
        assert_ne!(a, b);
        // Both in-flight → no slot available.
        assert!(pool.acquire().is_none());

        // Dropping a lease returns its slot to the pool.
        let lease = pool.lease(a);
        drop(lease);
        let reused = pool.acquire().expect("slot reclaimed after lease drop");
        assert_eq!(reused, a);
        assert!(pool.acquire().is_none());
        let _ = b;
    }

    #[test]
    fn slot_pool_drains_multiple_returns() {
        let mut pool = SlotPool::new(3);
        let ids: Vec<usize> = (0..3).map(|_| pool.acquire().unwrap()).collect();
        assert!(pool.acquire().is_none());
        for id in &ids {
            drop(pool.lease(*id));
        }
        // All three should be reclaimable.
        let mut reclaimed = Vec::new();
        while let Some(id) = pool.acquire() {
            reclaimed.push(id);
        }
        reclaimed.sort_unstable();
        assert_eq!(reclaimed, vec![0, 1, 2]);
    }

    #[test]
    fn quad_flip_inverts_v_only() {
        let normal = quad_vertices(false);
        let flipped = quad_vertices(true);
        // x/y identical, u identical, v inverted.
        for i in 0..4 {
            assert_eq!(normal[i * 4], flipped[i * 4]); // x
            assert_eq!(normal[i * 4 + 1], flipped[i * 4 + 1]); // y
            assert_eq!(normal[i * 4 + 2], flipped[i * 4 + 2]); // u
            assert_eq!(normal[i * 4 + 3], 1.0 - flipped[i * 4 + 3]); // v
        }
    }
}
