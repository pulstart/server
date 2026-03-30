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

#[cfg(target_os = "macos")]
mod platform {
    use super::{AudioConfig, AudioSamples};
    use crate::macos_display::{describe_display, select_capture_display};
    use crossbeam_channel::Sender;
    use screencapturekit::{cm::AudioBufferList, prelude::*};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    const MAX_AUDIO_FORMAT_ERRORS: usize = 12;

    pub struct AudioCapture {
        stream: Option<SCStream>,
    }

    impl AudioCapture {
        pub fn new() -> Self {
            Self { stream: None }
        }

        pub fn start(
            &mut self,
            config: AudioConfig,
            _device: Option<String>,
            tx: Sender<AudioSamples>,
        ) -> Result<(), String> {
            if self.stream.is_some() {
                return Err("Audio capture already running".into());
            }

            let display = select_capture_display()?;
            println!(
                "[audio] macOS ScreenCaptureKit using {}",
                describe_display(&display)
            );

            let filter = SCContentFilter::create()
                .with_display(&display)
                .with_excluding_windows(&[])
                .build();
            let stream_config = SCStreamConfiguration::new()
                .with_width(display.width().max(1))
                .with_height(display.height().max(1))
                .with_captures_audio(true)
                .with_sample_rate(config.sample_rate as i32)
                .with_channel_count(config.channels as i32)
                .with_excludes_current_process_audio(true);
            let frame_samples = config.total_samples_per_frame() as usize;

            let mut stream = SCStream::new_with_delegate(
                &filter,
                &stream_config,
                ErrorHandler::new(|err| eprintln!("[audio] ScreenCaptureKit stream error: {err:?}")),
            );
            stream.add_output_handler(
                AudioOutputHandler {
                    tx,
                    pending: Mutex::new(Vec::with_capacity(frame_samples * 3)),
                    frame_samples,
                    sample_rate: config.sample_rate,
                    channels: config.channels,
                    format_errors: AtomicUsize::new(0),
                },
                SCStreamOutputType::Audio,
            );
            stream
                .start_capture()
                .map_err(|err| format!("Failed to start ScreenCaptureKit audio capture: {err:?}"))?;

            println!(
                "[audio] ScreenCaptureKit audio capture started ({}ch, {}Hz, frame={} samples)",
                config.channels,
                config.sample_rate,
                config.total_samples_per_frame()
            );
            self.stream = Some(stream);
            Ok(())
        }

        pub fn stop(&mut self) {
            if let Some(stream) = self.stream.take() {
                let _ = stream.stop_capture();
            }
        }
    }

    pub fn detect_monitor_source() -> Option<String> {
        Some("default-screencapturekit".to_string())
    }

    struct AudioOutputHandler {
        tx: Sender<AudioSamples>,
        pending: Mutex<Vec<f32>>,
        frame_samples: usize,
        sample_rate: u32,
        channels: u32,
        format_errors: AtomicUsize,
    }

    impl SCStreamOutputTrait for AudioOutputHandler {
        fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }

            let mut decoded = match decode_audio_samples(&sample_buffer, self.sample_rate, self.channels) {
                Ok(decoded) => decoded,
                Err(err) => {
                    let log_idx = self.format_errors.fetch_add(1, Ordering::Relaxed);
                    if log_idx < MAX_AUDIO_FORMAT_ERRORS {
                        eprintln!("[audio] ScreenCaptureKit audio sample dropped: {err}");
                    }
                    return;
                }
            };
            if decoded.is_empty() {
                return;
            }

            let mut pending = self.pending.lock().unwrap();
            pending.append(&mut decoded);
            while pending.len() >= self.frame_samples {
                let frame = pending.drain(..self.frame_samples).collect::<Vec<_>>();
                if self
                    .tx
                    .send(AudioSamples {
                        data: frame,
                        channels: self.channels,
                        sample_rate: self.sample_rate,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    fn decode_audio_samples(
        sample_buffer: &CMSampleBuffer,
        expected_sample_rate: u32,
        expected_channels: u32,
    ) -> Result<Vec<f32>, String> {
        if !sample_buffer.is_valid() {
            return Err("invalid sample buffer".into());
        }

        let format = sample_buffer
            .format_description()
            .ok_or_else(|| "missing audio format description".to_string())?;
        if !format.is_audio() {
            return Err("received non-audio sample".into());
        }
        if !format.is_pcm() {
            return Err(format!(
                "unsupported audio codec {}",
                format.media_subtype_string()
            ));
        }

        let sample_rate = format
            .audio_sample_rate()
            .map(|rate| rate.round().max(1.0) as u32)
            .unwrap_or(expected_sample_rate);
        let channels = format
            .audio_channel_count()
            .unwrap_or(expected_channels)
            .max(1);
        if sample_rate != expected_sample_rate || channels != expected_channels {
            return Err(format!(
                "got {}ch/{}Hz, expected {}ch/{}Hz",
                channels, sample_rate, expected_channels, expected_sample_rate
            ));
        }

        let bits_per_channel = format
            .audio_bits_per_channel()
            .unwrap_or(if format.audio_is_float() { 32 } else { 16 });
        let bytes_per_sample = bytes_per_sample(bits_per_channel, format.audio_is_float())?;
        let buffer_list = sample_buffer
            .audio_buffer_list()
            .ok_or_else(|| "audio sample missing buffer list".to_string())?;
        if buffer_list.num_buffers() == 0 {
            return Ok(Vec::new());
        }

        if buffer_list.num_buffers() == 1 {
            let buffer = buffer_list
                .get(0)
                .ok_or_else(|| "missing interleaved audio buffer".to_string())?;
            let data = buffer.data();
            let interleaved_channels = if buffer.number_channels == 0 {
                channels as usize
            } else {
                buffer.number_channels as usize
            };
            if interleaved_channels != channels as usize {
                return Err(format!(
                    "interleaved audio buffer reports {} channels, expected {}",
                    interleaved_channels, channels
                ));
            }
            decode_interleaved(
                data,
                channels as usize,
                bytes_per_sample,
                bits_per_channel,
                format.audio_is_float(),
                format.audio_is_big_endian(),
            )
        } else {
            if buffer_list.num_buffers() != channels as usize {
                return Err(format!(
                    "planar audio buffer count {} does not match {} channels",
                    buffer_list.num_buffers(),
                    channels
                ));
            }
            decode_planar(
                &buffer_list,
                channels as usize,
                bytes_per_sample,
                bits_per_channel,
                format.audio_is_float(),
                format.audio_is_big_endian(),
            )
        }
    }

    fn bytes_per_sample(bits_per_channel: u32, is_float: bool) -> Result<usize, String> {
        match (is_float, bits_per_channel) {
            (true, 32) => Ok(4),
            (false, 16) => Ok(2),
            (false, 32) => Ok(4),
            _ => Err(format!(
                "unsupported PCM layout: float={} bits={}",
                is_float, bits_per_channel
            )),
        }
    }

    fn decode_interleaved(
        data: &[u8],
        channels: usize,
        bytes_per_sample: usize,
        bits_per_channel: u32,
        is_float: bool,
        big_endian: bool,
    ) -> Result<Vec<f32>, String> {
        let frame_bytes = bytes_per_sample
            .checked_mul(channels)
            .ok_or_else(|| "audio frame size overflow".to_string())?;
        if frame_bytes == 0 || data.len() % frame_bytes != 0 {
            return Err(format!(
                "interleaved audio payload size {} is not aligned to {}-byte frames",
                data.len(),
                frame_bytes
            ));
        }

        let mut samples = Vec::with_capacity(data.len() / bytes_per_sample);
        for sample in data.chunks_exact(bytes_per_sample) {
            samples.push(decode_sample(
                sample,
                bits_per_channel,
                is_float,
                big_endian,
            )?);
        }
        Ok(samples)
    }

    fn decode_planar(
        buffer_list: &AudioBufferList,
        channels: usize,
        bytes_per_sample: usize,
        bits_per_channel: u32,
        is_float: bool,
        big_endian: bool,
    ) -> Result<Vec<f32>, String> {
        let mut channel_data = Vec::with_capacity(channels);
        let mut frames = None;

        for channel_index in 0..channels {
            let buffer = buffer_list
                .get(channel_index)
                .ok_or_else(|| format!("missing channel buffer {channel_index}"))?;
            let data = buffer.data();
            if data.len() % bytes_per_sample != 0 {
                return Err(format!(
                    "planar channel {} payload size {} is not aligned to {}-byte samples",
                    channel_index,
                    data.len(),
                    bytes_per_sample
                ));
            }
            let channel_frames = data.len() / bytes_per_sample;
            match frames {
                Some(existing) if existing != channel_frames => {
                    return Err(format!(
                        "planar channel {} has {} frames, expected {}",
                        channel_index, channel_frames, existing
                    ));
                }
                None => frames = Some(channel_frames),
                _ => {}
            }
            channel_data.push(data);
        }

        let frames = frames.unwrap_or(0);
        let mut samples = Vec::with_capacity(frames * channels);
        for frame_index in 0..frames {
            let start = frame_index * bytes_per_sample;
            let end = start + bytes_per_sample;
            for data in &channel_data {
                samples.push(decode_sample(
                    &data[start..end],
                    bits_per_channel,
                    is_float,
                    big_endian,
                )?);
            }
        }
        Ok(samples)
    }

    fn decode_sample(
        bytes: &[u8],
        bits_per_channel: u32,
        is_float: bool,
        big_endian: bool,
    ) -> Result<f32, String> {
        match (is_float, bits_per_channel) {
            (true, 32) => {
                let bytes: [u8; 4] = bytes
                    .try_into()
                    .map_err(|_| "invalid float32 sample size".to_string())?;
                Ok(if big_endian {
                    f32::from_be_bytes(bytes)
                } else {
                    f32::from_le_bytes(bytes)
                })
            }
            (false, 16) => {
                let bytes: [u8; 2] = bytes
                    .try_into()
                    .map_err(|_| "invalid int16 sample size".to_string())?;
                let value = if big_endian {
                    i16::from_be_bytes(bytes)
                } else {
                    i16::from_le_bytes(bytes)
                };
                Ok(value as f32 / i16::MAX as f32)
            }
            (false, 32) => {
                let bytes: [u8; 4] = bytes
                    .try_into()
                    .map_err(|_| "invalid int32 sample size".to_string())?;
                let value = if big_endian {
                    i32::from_be_bytes(bytes)
                } else {
                    i32::from_le_bytes(bytes)
                };
                Ok(value as f32 / i32::MAX as f32)
            }
            _ => Err(format!(
                "unsupported PCM layout: float={} bits={}",
                is_float, bits_per_channel
            )),
        }
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
