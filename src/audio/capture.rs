/// PulseAudio audio capture, matching Sunshine's `platform/linux/audio.cpp`.
///
/// Uses the PulseAudio Simple API to record float32 samples from a monitor source.
/// Supports stereo (2ch), 5.1 (6ch), and 7.1 (8ch) configurations.
use crate::encode_config::AudioConfig;
use crossbeam_channel::Sender;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

extern crate ffmpeg_sys_next as ffi;

/// Raw libpulse-simple FFI bindings.
/// We use raw FFI to avoid pulling in the libpulse-binding crate dependency chain,
/// matching Sunshine's direct use of pa_simple.
#[allow(non_camel_case_types)]
mod pulse_ffi {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_int};

    pub const PA_SAMPLE_FLOAT32LE: u32 = 5;
    pub const PA_STREAM_RECORD: u32 = 2;

    #[repr(C)]
    pub struct pa_sample_spec {
        pub format: u32,
        pub rate: u32,
        pub channels: u8,
    }

    #[repr(C)]
    pub struct pa_buffer_attr {
        pub maxlength: u32,
        pub tlength: u32,
        pub prebuf: u32,
        pub minreq: u32,
        pub fragsize: u32,
    }

    pub type PaSimpleT = c_void;

    extern "C" {
        pub fn pa_simple_new(
            server: *const c_char,
            name: *const c_char,
            dir: u32,
            dev: *const c_char,
            stream_name: *const c_char,
            ss: *const pa_sample_spec,
            map: *const c_void, // pa_channel_map, null for default
            attr: *const pa_buffer_attr,
            error: *mut c_int,
        ) -> *mut PaSimpleT;

        pub fn pa_simple_free(s: *mut PaSimpleT);

        pub fn pa_simple_read(
            s: *mut PaSimpleT,
            data: *mut c_void,
            bytes: usize,
            error: *mut c_int,
        ) -> c_int;

        pub fn pa_strerror(error: c_int) -> *const c_char;
    }
}

/// RAII wrapper for `pa_simple*`.
struct PaSimple {
    ptr: *mut pulse_ffi::PaSimpleT,
}

impl PaSimple {
    fn new(
        device: Option<&str>,
        channels: u8,
        sample_rate: u32,
        fragment_size: u32,
    ) -> Result<Self, String> {
        let app_name = std::ffi::CString::new("st-server").unwrap();
        let stream_name = std::ffi::CString::new("screen-audio").unwrap();

        let dev_c = device.map(|d| std::ffi::CString::new(d).unwrap());
        let dev_ptr = dev_c
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(std::ptr::null());

        let ss = pulse_ffi::pa_sample_spec {
            format: pulse_ffi::PA_SAMPLE_FLOAT32LE,
            rate: sample_rate,
            channels,
        };

        // Use server-defaults for most fields (u32::MAX means "let PA decide"),
        // but cap maxlength to ~500ms of audio to prevent excessive allocation.
        let max_buffer = fragment_size.saturating_mul(25).max(fragment_size);
        let attr = pulse_ffi::pa_buffer_attr {
            maxlength: max_buffer,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: fragment_size,
        };

        let mut error: std::os::raw::c_int = 0;
        let ptr = unsafe {
            pulse_ffi::pa_simple_new(
                std::ptr::null(), // default server
                app_name.as_ptr(),
                pulse_ffi::PA_STREAM_RECORD,
                dev_ptr,
                stream_name.as_ptr(),
                &ss,
                std::ptr::null(), // default channel map
                &attr,
                &mut error,
            )
        };

        if ptr.is_null() {
            let err_str = unsafe {
                let p = pulse_ffi::pa_strerror(error);
                std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
            };
            return Err(format!("pa_simple_new failed: {err_str}"));
        }

        Ok(Self { ptr })
    }

    fn read_f32(&self, buf: &mut [f32]) -> Result<(), String> {
        let mut error: std::os::raw::c_int = 0;
        let bytes = buf.len() * std::mem::size_of::<f32>();
        let ret = unsafe {
            pulse_ffi::pa_simple_read(
                self.ptr,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                bytes,
                &mut error,
            )
        };
        if ret < 0 {
            let err_str = unsafe {
                let p = pulse_ffi::pa_strerror(error);
                std::ffi::CStr::from_ptr(p).to_string_lossy().to_string()
            };
            return Err(format!("pa_simple_read failed: {err_str}"));
        }
        Ok(())
    }
}

impl Drop for PaSimple {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { pulse_ffi::pa_simple_free(self.ptr) };
        }
    }
}

// SAFETY: pa_simple is internally synchronized.
unsafe impl Send for PaSimple {}

/// Captured audio samples — a single frame of interleaved float32 PCM.
pub struct AudioSamples {
    pub data: Vec<f32>,
    pub channels: u32,
    pub sample_rate: u32,
}

pub struct AudioCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AudioCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }

    /// Start capturing audio. Sends `AudioSamples` frames to `tx`.
    ///
    /// `device`: PulseAudio source name (e.g., monitor of default sink).
    ///           Pass `None` for the default monitor source.
    pub fn start(
        &mut self,
        config: AudioConfig,
        device: Option<String>,
        tx: Sender<AudioSamples>,
    ) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Audio capture already running".into());
        }

        let channels = config.channels as u8;
        let sample_rate = config.sample_rate;
        let samples_per_frame = config.total_samples_per_frame();

        // Fragment size in bytes: one frame of float32 samples
        let fragment_size = (samples_per_frame * std::mem::size_of::<f32>()) as u32;

        // Validate PulseAudio connection before spawning thread
        let pa = PaSimple::new(device.as_deref(), channels, sample_rate, fragment_size)?;

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);

        let device_clone = device.map(|d| d.to_string());
        let handle = thread::spawn(move || {
            println!("[audio] Capture thread started ({channels}ch, {sample_rate}Hz, frame={samples_per_frame} samples)");

            let mut pa = pa;
            let mut buf = vec![0.0f32; samples_per_frame];

            while running.load(Ordering::SeqCst) {
                match pa.read_f32(&mut buf) {
                    Ok(()) => {
                        // Move buffer into AudioSamples — avoids clone memcpy
                        let samples = AudioSamples {
                            data: std::mem::replace(&mut buf, vec![0.0f32; samples_per_frame]),
                            channels: channels as u32,
                            sample_rate,
                        };
                        if tx.send(samples).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("[audio] Capture error: {e}");
                        if !running.load(Ordering::SeqCst) {
                            break;
                        }
                        // Recreate PulseAudio connection instead of retrying broken handle
                        thread::sleep(std::time::Duration::from_secs(5));
                        if !running.load(Ordering::SeqCst) {
                            break;
                        }
                        match PaSimple::new(device_clone.as_deref(), channels, sample_rate, fragment_size) {
                            Ok(new_pa) => {
                                eprintln!("[audio] PulseAudio reconnected");
                                pa = new_pa;
                            }
                            Err(e2) => {
                                eprintln!("[audio] PulseAudio reconnect failed: {e2}");
                            }
                        }
                    }
                }
            }

            println!("[audio] Capture thread exited");
        });

        self.handle = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Detect the PulseAudio monitor source for the default sink.
/// Returns something like "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor".
pub fn detect_monitor_source() -> Option<String> {
    // Try to get the default sink and derive its monitor source name.
    // pactl info | grep "Default Sink" → append ".monitor"
    let output = std::process::Command::new("pactl")
        .arg("info")
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(sink) = line.strip_prefix("Default Sink: ") {
            let monitor = format!("{sink}.monitor");
            println!("[audio] Detected monitor source: {monitor}");
            return Some(monitor);
        }
    }

    None
}
