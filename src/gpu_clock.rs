//! While streaming, hold an NVIDIA GPU's memory clock at its P3 level. An idle
//! desktop parks the GPU in P8 with the PCIe link at gen1, so each sparse frame
//! (typing, hovering) pays for it: scanout readback 9.5 ms instead of 2.7,
//! encode 4.7 ms instead of 2.6 (p99 7.8 vs 3.7) on an RTX 4080 at 1440p.
//! Clock floors on the graphics clock only reach gen2. Costs ~11 W while a
//! client is connected. Needs root (NVML); `ST_GPU_CLOCK_FLOOR=0` disables.

use libloading::Library;
use std::ffi::{c_char, c_uint, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

type Device = *mut c_void;
type Ret = c_uint;

const MARKER: &str = "gpu-clock-floor";

struct Nvml {
    _lib: Library,
    set_locked: unsafe extern "C" fn(Device, c_uint, c_uint) -> Ret,
    reset_locked: unsafe extern "C" fn(Device) -> Ret,
    supported: unsafe extern "C" fn(Device, *mut c_uint, *mut c_uint) -> Ret,
    utilization: unsafe extern "C" fn(Device, *mut [c_uint; 2]) -> Ret,
    error: unsafe extern "C" fn(Ret) -> *const c_char,
    shutdown: unsafe extern "C" fn() -> Ret,
    device: Device,
}

impl Nvml {
    /// NVML attached to the GPU at PCI address `bus_id` (e.g. `0000:0b:00.0`).
    fn open(bus_id: &str) -> Result<Self, String> {
        unsafe {
            let lib = Library::new("libnvidia-ml.so.1").map_err(|e| e.to_string())?;
            let init = *lib
                .get::<unsafe extern "C" fn() -> Ret>(b"nvmlInit_v2\0")
                .map_err(|e| e.to_string())?;
            let by_bus = *lib
                .get::<unsafe extern "C" fn(*const c_char, *mut Device) -> Ret>(
                    b"nvmlDeviceGetHandleByPciBusId_v2\0",
                )
                .map_err(|e| e.to_string())?;
            let mut nvml = Self {
                set_locked: *lib
                    .get(b"nvmlDeviceSetMemoryLockedClocks\0")
                    .map_err(|e| e.to_string())?,
                reset_locked: *lib
                    .get(b"nvmlDeviceResetMemoryLockedClocks\0")
                    .map_err(|e| e.to_string())?,
                supported: *lib
                    .get(b"nvmlDeviceGetSupportedMemoryClocks\0")
                    .map_err(|e| e.to_string())?,
                utilization: *lib
                    .get(b"nvmlDeviceGetUtilizationRates\0")
                    .map_err(|e| e.to_string())?,
                error: *lib.get(b"nvmlErrorString\0").map_err(|e| e.to_string())?,
                shutdown: *lib.get(b"nvmlShutdown\0").map_err(|e| e.to_string())?,
                device: std::ptr::null_mut(),
                _lib: lib,
            };
            if init() != 0 {
                return Err("nvmlInit failed".into());
            }
            let bus = CString::new(bus_id).map_err(|e| e.to_string())?;
            let ret = by_bus(bus.as_ptr(), &mut nvml.device);
            nvml.check(ret, "device lookup")?;
            Ok(nvml)
        }
    }

    fn check(&self, ret: Ret, what: &str) -> Result<(), String> {
        if ret == 0 {
            return Ok(());
        }
        let msg = unsafe { CStr::from_ptr((self.error)(ret)) };
        Err(format!("{what}: {}", msg.to_string_lossy()))
    }

    /// The lowest supported memory clock at or above a quarter of the maximum:
    /// the P3 level (5001 of 11201 MHz on an RTX 4080; 810 is P5, link gen2).
    fn floor_and_max(&self) -> Result<(c_uint, c_uint), String> {
        let mut clocks = [0 as c_uint; 32];
        let mut count = clocks.len() as c_uint;
        let ret = unsafe { (self.supported)(self.device, &mut count, clocks.as_mut_ptr()) };
        self.check(ret, "supported memory clocks")?;
        let clocks = &clocks[..count as usize];
        let max = clocks.iter().copied().max().ok_or("no memory clocks")?;
        let floor = clocks
            .iter()
            .copied()
            .filter(|&c| c * 4 >= max)
            .min()
            .unwrap_or(max);
        Ok((floor, max))
    }
}

static GAME_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The session reports a focused game: the 3D engine counts as busy even
/// between utilisation samples, so nothing retries it against the game.
pub fn set_game_active(active: bool) {
    GAME_ACTIVE.store(active, Ordering::Relaxed);
}

/// 3D/compute utilisation of an NVIDIA GPU, resampled at most every 250 ms.
pub struct GpuLoad {
    nvml: Nvml,
    busy: bool,
    sampled_at: Option<Instant>,
}

// SAFETY: see `ClockFloor`.
unsafe impl Send for GpuLoad {}

impl GpuLoad {
    /// Something else — a game — keeps the 3D engine at least this busy.
    const BUSY_PERCENT: c_uint = 50;
    const RESAMPLE: Duration = Duration::from_millis(250);

    pub fn open(render_node: &str) -> Option<Self> {
        let nvml = Nvml::open(&nvidia_bus_id(render_node)?).ok()?;
        Some(Self {
            nvml,
            busy: false,
            sampled_at: None,
        })
    }

    pub fn busy(&mut self, now: Instant) -> bool {
        if GAME_ACTIVE.load(Ordering::Relaxed) {
            return true;
        }
        if self
            .sampled_at
            .is_none_or(|at| now.saturating_duration_since(at) >= Self::RESAMPLE)
        {
            let mut rates = [0 as c_uint; 2];
            let ret = unsafe { (self.nvml.utilization)(self.nvml.device, &mut rates) };
            self.busy = ret == 0 && rates[0] >= Self::BUSY_PERCENT;
            self.sampled_at = Some(now);
        }
        self.busy
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe { (self.shutdown)() };
    }
}

/// Held while a stream runs; dropping it releases the floor.
pub struct ClockFloor {
    nvml: Nvml,
    marker: Option<PathBuf>,
}

// SAFETY: the NVML device handle is a process-wide token, valid on any thread.
unsafe impl Send for ClockFloor {}

fn enabled() -> bool {
    !matches!(
        std::env::var("ST_GPU_CLOCK_FLOOR").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// PCI address of an NVIDIA render node's GPU.
fn nvidia_bus_id(render_node: &str) -> Option<String> {
    crate::capture::linux::is_nvidia_render_node(render_node).then_some(())?;
    let name = std::path::Path::new(render_node).file_name()?;
    let device = PathBuf::from("/sys/class/drm").join(name).join("device");
    Some(
        device
            .canonicalize()
            .ok()?
            .file_name()?
            .to_str()?
            .to_owned(),
    )
}

impl ClockFloor {
    /// Raise the memory clock floor of the GPU behind `render_node` if it is
    /// NVIDIA and we may. Logs the first failure only.
    pub fn engage(render_node: Option<&str>) -> Option<Self> {
        static FAILED: AtomicBool = AtomicBool::new(false);
        if !enabled() || FAILED.load(Ordering::Relaxed) {
            return None;
        }
        let bus_id = nvidia_bus_id(render_node?)?;
        let result = (|| {
            let nvml = Nvml::open(&bus_id)?;
            let (floor, max) = nvml.floor_and_max()?;
            let ret = unsafe { (nvml.set_locked)(nvml.device, floor, max) };
            nvml.check(ret, "lock memory clocks")?;
            let marker = crate::server_control::state_file(MARKER);
            if let Some(path) = &marker {
                let _ = std::fs::write(path, &bus_id);
            }
            println!("[gpu] memory clock held at >= {floor} MHz while streaming (ST_GPU_CLOCK_FLOOR=0 disables)");
            Ok::<_, String>(Self { nvml, marker })
        })();
        result
            .map_err(|e| {
                if !FAILED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[gpu] memory clock floor unavailable ({e}); idle-GPU frames run slower"
                    );
                }
            })
            .ok()
    }
}

impl Drop for ClockFloor {
    fn drop(&mut self) {
        let ret = unsafe { (self.nvml.reset_locked)(self.nvml.device) };
        if let Err(e) = self.nvml.check(ret, "reset memory clocks") {
            eprintln!("[gpu] {e}");
        } else if let Some(path) = &self.marker {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A floor left behind by a killed process: release it.
pub fn release_stale() {
    let Some(path) = crate::server_control::state_file(MARKER) else {
        return;
    };
    let Ok(bus_id) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Ok(nvml) = Nvml::open(bus_id.trim()) {
        let ret = unsafe { (nvml.reset_locked)(nvml.device) };
        if nvml.check(ret, "reset").is_ok() {
            println!("[gpu] released a memory clock floor left by a previous run");
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ST_TEST_GPU_FLOOR=1 <test-binary> live_gpu_load --nocapture`
    #[test]
    fn live_gpu_load() {
        if std::env::var_os("ST_TEST_GPU_FLOOR").is_none() {
            return;
        }
        let mut load = GpuLoad::open("/dev/dri/renderD128").expect("nvml");
        let t0 = Instant::now();
        eprintln!("[load] idle busy={}", load.busy(t0));
    }

    /// Engages the floor on the display GPU and checks it holds P3 (root).
    /// `ST_TEST_GPU_FLOOR=1 sudo -E <test-binary> live_clock_floor --nocapture`
    #[test]
    fn live_clock_floor() {
        if std::env::var_os("ST_TEST_GPU_FLOOR").is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ST_STATE_DIR", tmp.path());
        let node = "/dev/dri/renderD128";
        let pstate = || {
            let out = std::process::Command::new("nvidia-smi")
                .args(["--query-gpu=pstate,clocks.mem", "--format=csv,noheader"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let floor = ClockFloor::engage(Some(node)).expect("floor engaged");
        assert!(tmp.path().join(MARKER).exists());
        std::thread::sleep(std::time::Duration::from_secs(2));
        let held = pstate();
        eprintln!("[floor] held: {held}");
        assert!(held.starts_with("P3") || held.starts_with("P2") || held.starts_with("P0"));
        drop(floor);
        assert!(!tmp.path().join(MARKER).exists());
        std::thread::sleep(std::time::Duration::from_secs(3));
        eprintln!("[floor] released: {}", pstate());
    }
}
