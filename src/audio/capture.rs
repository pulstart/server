use crate::encode_config::AudioConfig;

/// Captured audio samples — a single frame of interleaved float32 PCM.
pub struct AudioSamples {
    pub data: Vec<f32>,
    pub channels: u32,
    pub sample_rate: u32,
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{AudioConfig, AudioSamples};
    use crossbeam_channel::Sender;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;

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
                map: *const c_void,
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
                    std::ptr::null(),
                    app_name.as_ptr(),
                    pulse_ffi::PA_STREAM_RECORD,
                    dev_ptr,
                    stream_name.as_ptr(),
                    &ss,
                    std::ptr::null(),
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
            let bytes = std::mem::size_of_val(buf);
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

    unsafe impl Send for PaSimple {}

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
            let fragment_size = (samples_per_frame * std::mem::size_of::<f32>()) as u32;
            let pa = PaSimple::new(device.as_deref(), channels, sample_rate, fragment_size)?;

            self.running.store(true, Ordering::SeqCst);
            let running = Arc::clone(&self.running);
            let device_clone = device.map(|d| d.to_string());
            let handle = thread::spawn(move || {
                println!(
                    "[audio] Capture thread started ({channels}ch, {sample_rate}Hz, frame={samples_per_frame} samples)"
                );

                let mut pa = pa;
                let mut buf = vec![0.0f32; samples_per_frame];

                while running.load(Ordering::SeqCst) {
                    match pa.read_f32(&mut buf) {
                        Ok(()) => {
                            let samples = AudioSamples {
                                data: std::mem::replace(
                                    &mut buf,
                                    vec![0.0f32; samples_per_frame],
                                ),
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
                            thread::sleep(std::time::Duration::from_secs(5));
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                            match PaSimple::new(
                                device_clone.as_deref(),
                                channels,
                                sample_rate,
                                fragment_size,
                            ) {
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
    pub fn detect_monitor_source() -> Option<String> {
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
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{AudioConfig, AudioSamples};
    use crossbeam_channel::Sender;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
        WAVEFORMATEXTENSIBLE_0,
    };
    use windows::Win32::Media::KernelStreaming::{
        SPEAKER_BACK_LEFT, SPEAKER_BACK_RIGHT, SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT,
        SPEAKER_FRONT_RIGHT, SPEAKER_LOW_FREQUENCY, SPEAKER_SIDE_LEFT, SPEAKER_SIDE_RIGHT,
        WAVE_FORMAT_EXTENSIBLE,
    };
    use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const INIT_TIMEOUT: Duration = Duration::from_secs(5);
    const PACKET_POLL_INTERVAL: Duration = Duration::from_millis(5);
    const RECOVER_DELAY: Duration = Duration::from_secs(2);

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

        pub fn start(
            &mut self,
            config: AudioConfig,
            _device: Option<String>,
            tx: Sender<AudioSamples>,
        ) -> Result<(), String> {
            if self.running.load(Ordering::SeqCst) {
                return Err("Audio capture already running".into());
            }

            self.running.store(true, Ordering::SeqCst);
            let running = Arc::clone(&self.running);
            let (init_tx, init_rx) = std::sync::mpsc::channel();
            let handle = thread::spawn(move || {
                let mut init_sent = false;
                while running.load(Ordering::SeqCst) {
                    match WasapiLoopbackSession::new(&config) {
                        Ok(mut session) => {
                            if !init_sent {
                                let _ = init_tx.send(Ok(()));
                                init_sent = true;
                            }
                            println!(
                                "[audio] WASAPI loopback started ({}ch, {}Hz, frame={} samples)",
                                config.channels,
                                config.sample_rate,
                                config.total_samples_per_frame()
                            );
                            if let Err(err) = run_session_loop(&mut session, &config, &tx, &running) {
                                eprintln!("[audio] WASAPI capture error: {err}");
                            }
                        }
                        Err(err) => {
                            if !init_sent {
                                let _ = init_tx.send(Err(err));
                                return;
                            }
                            eprintln!("[audio] WASAPI reopen failed: {err}");
                        }
                    }

                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(RECOVER_DELAY);
                }
                println!("[audio] Capture thread exited");
            });

            match init_rx.recv_timeout(INIT_TIMEOUT) {
                Ok(Ok(())) => {
                    self.handle = Some(handle);
                    Ok(())
                }
                Ok(Err(err)) => {
                    self.running.store(false, Ordering::SeqCst);
                    let _ = handle.join();
                    Err(err)
                }
                Err(_) => {
                    self.running.store(false, Ordering::SeqCst);
                    let _ = handle.join();
                    Err("Timed out starting WASAPI loopback capture".into())
                }
            }
        }

        pub fn stop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn detect_monitor_source() -> Option<String> {
        Some("default-wasapi-loopback".to_string())
    }

    fn run_session_loop(
        session: &mut WasapiLoopbackSession,
        config: &AudioConfig,
        tx: &Sender<AudioSamples>,
        running: &Arc<AtomicBool>,
    ) -> Result<(), String> {
        let frame_samples = config.total_samples_per_frame();
        let mut pending = Vec::<f32>::with_capacity(frame_samples * 3);

        while running.load(Ordering::SeqCst) {
            session.read_available(&mut pending)?;
            while pending.len() >= frame_samples {
                let frame = pending.drain(..frame_samples).collect::<Vec<_>>();
                if tx
                    .send(AudioSamples {
                        data: frame,
                        channels: config.channels,
                        sample_rate: config.sample_rate,
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
            thread::sleep(PACKET_POLL_INTERVAL);
        }

        Ok(())
    }

    struct ComGuard;

    impl ComGuard {
        fn init() -> Result<Self, String> {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED)
                    .ok()
                    .map_err(|err| format!("CoInitializeEx failed: {err}"))?;
            }
            Ok(Self)
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }

    struct WasapiLoopbackSession {
        _com: ComGuard,
        audio_client: IAudioClient,
        capture_client: IAudioCaptureClient,
        channels: u32,
    }

    impl WasapiLoopbackSession {
        fn new(config: &AudioConfig) -> Result<Self, String> {
            let com = ComGuard::init()?;
            let enumerator: IMMDeviceEnumerator = unsafe {
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|err| format!("CoCreateInstance(MMDeviceEnumerator) failed: {err}"))?
            };
            let endpoint = unsafe {
                enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|err| format!("GetDefaultAudioEndpoint failed: {err}"))?
            };
            let audio_client: IAudioClient = unsafe {
                endpoint
                    .Activate(CLSCTX_ALL, None)
                    .map_err(|err| format!("IMMDevice::Activate(IAudioClient) failed: {err}"))?
            };

            let mut format = desired_wave_format(config);
            unsafe {
                audio_client
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK
                            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                        20_000_000,
                        0,
                        &mut format.Format as *mut WAVEFORMATEX,
                        None,
                    )
                    .map_err(|err| format!("IAudioClient::Initialize failed: {err}"))?;
            }

            let capture_client = unsafe {
                audio_client
                    .GetService::<IAudioCaptureClient>()
                    .map_err(|err| format!("IAudioClient::GetService failed: {err}"))?
            };
            unsafe {
                audio_client
                    .Start()
                    .map_err(|err| format!("IAudioClient::Start failed: {err}"))?;
            }

            Ok(Self {
                _com: com,
                audio_client,
                capture_client,
                channels: config.channels,
            })
        }

        fn read_available(&mut self, pending: &mut Vec<f32>) -> Result<(), String> {
            loop {
                let packet_frames = unsafe {
                    self.capture_client
                        .GetNextPacketSize()
                        .map_err(|err| format!("IAudioCaptureClient::GetNextPacketSize failed: {err}"))?
                };
                if packet_frames == 0 {
                    break;
                }

                let mut data_ptr = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe {
                    self.capture_client
                        .GetBuffer(
                            &mut data_ptr,
                            &mut frames,
                            &mut flags,
                            None,
                            None,
                        )
                        .map_err(|err| format!("IAudioCaptureClient::GetBuffer failed: {err}"))?;
                }

                let sample_count = frames as usize * self.channels as usize;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data_ptr.is_null() {
                    pending.extend(std::iter::repeat(0.0f32).take(sample_count));
                } else {
                    let src = unsafe { std::slice::from_raw_parts(data_ptr as *const f32, sample_count) };
                    pending.extend_from_slice(src);
                }

                unsafe {
                    self.capture_client
                        .ReleaseBuffer(frames)
                        .map_err(|err| format!("IAudioCaptureClient::ReleaseBuffer failed: {err}"))?;
                }
            }

            Ok(())
        }
    }

    impl Drop for WasapiLoopbackSession {
        fn drop(&mut self) {
            unsafe {
                let _ = self.audio_client.Stop();
            }
        }
    }

    fn desired_wave_format(config: &AudioConfig) -> WAVEFORMATEXTENSIBLE {
        let block_align = (config.channels * std::mem::size_of::<f32>() as u32) as u16;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: config.channels as u16,
                nSamplesPerSec: config.sample_rate,
                nAvgBytesPerSec: config.sample_rate * block_align as u32,
                nBlockAlign: block_align,
                wBitsPerSample: 32,
                cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                    - std::mem::size_of::<WAVEFORMATEX>()) as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: channel_mask(config.channels),
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        }
    }

    fn channel_mask(channels: u32) -> u32 {
        match channels {
            6 => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
            }
            8 => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
                    | SPEAKER_SIDE_LEFT
                    | SPEAKER_SIDE_RIGHT
            }
            _ => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        }
    }
}

pub use platform::{detect_monitor_source, AudioCapture};
