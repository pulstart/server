//! Minimal gbm probe for DMA-BUF modifier enumeration.
//!
//! PipeWire DMA-BUF negotiation benefits from offering a list of modifiers
//! the importer (our encoder / EGL) and the allocator (the compositor's GPU)
//! both support. This module uses `libgbm.so.1` loaded dynamically via
//! `libloading` to discover the GPU's preferred tiled modifier for a given
//! DRM format, plus LINEAR as a universal fallback.
//!
//! The probe runs once per render node + format and caches its result.
//! It fails silently if `libgbm.so.1` is not available on the system — the
//! caller then falls back to offering LINEAR only (current behavior).

use libloading::Library;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::{IntoRawFd, RawFd};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

type GbmCreateDevice = unsafe extern "C" fn(fd: libc::c_int) -> *mut c_void;
type GbmDeviceDestroy = unsafe extern "C" fn(dev: *mut c_void);
type GbmBoCreate = unsafe extern "C" fn(
    dev: *mut c_void,
    width: u32,
    height: u32,
    format: u32,
    usage: u32,
) -> *mut c_void;
type GbmBoCreateWithModifiers = unsafe extern "C" fn(
    dev: *mut c_void,
    width: u32,
    height: u32,
    format: u32,
    modifiers: *const u64,
    count: libc::c_uint,
) -> *mut c_void;
type GbmBoGetModifier = unsafe extern "C" fn(bo: *const c_void) -> u64;
type GbmBoDestroy = unsafe extern "C" fn(bo: *mut c_void);

const GBM_BO_USE_RENDERING: u32 = 1 << 2;

struct GbmLib {
    _lib: Library,
    create_device: GbmCreateDevice,
    device_destroy: GbmDeviceDestroy,
    bo_create: GbmBoCreate,
    bo_create_with_modifiers: Option<GbmBoCreateWithModifiers>,
    bo_get_modifier: GbmBoGetModifier,
    bo_destroy: GbmBoDestroy,
}

impl GbmLib {
    fn load() -> Option<Self> {
        let candidates = ["libgbm.so.1", "libgbm.so"];
        let lib = candidates
            .iter()
            .find_map(|n| unsafe { Library::new(n).ok() })?;
        unsafe {
            let create_device = *lib.get::<GbmCreateDevice>(b"gbm_create_device\0").ok()?;
            let device_destroy = *lib
                .get::<GbmDeviceDestroy>(b"gbm_device_destroy\0")
                .ok()?;
            let bo_create = *lib.get::<GbmBoCreate>(b"gbm_bo_create\0").ok()?;
            let bo_create_with_modifiers = lib
                .get::<GbmBoCreateWithModifiers>(b"gbm_bo_create_with_modifiers\0")
                .ok()
                .map(|s| *s);
            let bo_get_modifier = *lib
                .get::<GbmBoGetModifier>(b"gbm_bo_get_modifier\0")
                .ok()?;
            let bo_destroy = *lib.get::<GbmBoDestroy>(b"gbm_bo_destroy\0").ok()?;
            Some(Self {
                _lib: lib,
                create_device,
                device_destroy,
                bo_create,
                bo_create_with_modifiers,
                bo_get_modifier,
                bo_destroy,
            })
        }
    }
}

fn lib() -> Option<&'static GbmLib> {
    static LIB: OnceLock<Option<GbmLib>> = OnceLock::new();
    LIB.get_or_init(GbmLib::load).as_ref()
}

fn cache() -> &'static Mutex<HashMap<(String, u32), Vec<u64>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, u32), Vec<u64>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// DRM_FORMAT_MOD_LINEAR sentinel (fourcc_mod_code(NONE, 0)).
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Probe the modifiers supported by `render_node` for `drm_format`.
/// Returns a list containing the GPU's preferred implicit modifier (if any)
/// and LINEAR. The list is empty if probing failed — caller should fall back
/// to offering LINEAR only.
///
/// We avoid depending on the gbm modifier-enumeration APIs that only exist
/// in recent libgbm. Instead we allocate one BO with the default path (gbm
/// picks the driver's preferred modifier) and read it back.
pub fn probe_modifiers(render_node: &str, drm_format: u32) -> Vec<u64> {
    let key = (render_node.to_string(), drm_format);
    if let Some(cached) = cache().lock().unwrap().get(&key).cloned() {
        return cached;
    }

    let mods = probe_modifiers_uncached(render_node, drm_format);
    cache().lock().unwrap().insert(key, mods.clone());
    mods
}

fn probe_modifiers_uncached(render_node: &str, drm_format: u32) -> Vec<u64> {
    let Some(lib) = lib() else {
        return Vec::new();
    };
    if !Path::new(render_node).exists() {
        return Vec::new();
    }

    let fd: RawFd = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)
    {
        Ok(f) => f.into_raw_fd(),
        Err(_) => return Vec::new(),
    };

    unsafe {
        let dev = (lib.create_device)(fd);
        if dev.is_null() {
            libc::close(fd);
            return Vec::new();
        }

        let bo = (lib.bo_create)(dev, 64, 64, drm_format, GBM_BO_USE_RENDERING);
        let mut mods: Vec<u64> = Vec::new();
        if !bo.is_null() {
            let m = (lib.bo_get_modifier)(bo);
            if m != DRM_FORMAT_MOD_INVALID {
                mods.push(m);
            }
            (lib.bo_destroy)(bo);
        }

        // Always include LINEAR as a universal fallback that any importer accepts.
        if !mods.contains(&DRM_FORMAT_MOD_LINEAR) {
            mods.push(DRM_FORMAT_MOD_LINEAR);
        }

        (lib.device_destroy)(dev);
        libc::close(fd);
        mods
    }
}

/// DRM_FORMAT_MOD_INVALID sentinel used by gbm when a BO has no modifier.
pub const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;

/// Optional finer probe: pass a candidate list of modifiers and return only
/// those that `gbm_bo_create_with_modifiers` accepts. Falls back to the
/// default probe when the symbol is unavailable.
#[allow(dead_code)]
pub fn probe_modifier_list(render_node: &str, drm_format: u32, candidates: &[u64]) -> Vec<u64> {
    let Some(lib) = lib() else {
        return Vec::new();
    };
    let Some(with_mods) = lib.bo_create_with_modifiers else {
        return Vec::new();
    };

    let fd: RawFd = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)
    {
        Ok(f) => f.into_raw_fd(),
        Err(_) => return Vec::new(),
    };

    let mut accepted = Vec::new();
    unsafe {
        let dev = (lib.create_device)(fd);
        if dev.is_null() {
            libc::close(fd);
            return Vec::new();
        }
        for &m in candidates {
            let list = [m];
            let bo = (with_mods)(dev, 64, 64, drm_format, list.as_ptr(), 1);
            if !bo.is_null() {
                accepted.push(m);
                (lib.bo_destroy)(bo);
            }
        }
        (lib.device_destroy)(dev);
        libc::close(fd);
    }
    accepted
}
