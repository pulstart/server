#![allow(non_snake_case)]

use super::super::{CaptureBackend, CapturedFrame, FrameData};
use super::target_fps;
use crossbeam_channel::{Sender, TrySendError};
use libloading::Library;
use std::cell::Cell;
use std::ffi::{c_char, c_void, CStr};
use std::fs;
use std::mem::{size_of, MaybeUninit};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

const NVFBC_VERSION_MAJOR: u32 = 1;
const NVFBC_VERSION_MINOR: u32 = 8;

const NVFBC_SUCCESS: u32 = 0;
const NVFBC_ERR_API_VERSION: u32 = 1;
const NVFBC_ERR_INTERNAL: u32 = 2;
const NVFBC_ERR_INVALID_PARAM: u32 = 3;
const NVFBC_ERR_INVALID_PTR: u32 = 4;
const NVFBC_ERR_INVALID_HANDLE: u32 = 5;
const NVFBC_ERR_MAX_CLIENTS: u32 = 6;
const NVFBC_ERR_UNSUPPORTED: u32 = 7;
const NVFBC_ERR_OUT_OF_MEMORY: u32 = 8;
const NVFBC_ERR_BAD_REQUEST: u32 = 9;
const NVFBC_ERR_X: u32 = 10;
const NVFBC_ERR_GLX: u32 = 11;
const NVFBC_ERR_GL: u32 = 12;
const NVFBC_ERR_CUDA: u32 = 13;
const NVFBC_ERR_ENCODER: u32 = 14;
const NVFBC_ERR_CONTEXT: u32 = 15;
const NVFBC_ERR_MUST_RECREATE: u32 = 16;
const NVFBC_ERR_VULKAN: u32 = 17;

const NVFBC_FALSE: u32 = 0;
const NVFBC_TRUE: u32 = 1;

const NVFBC_CAPTURE_TO_SYS: u32 = 0;
const NVFBC_TRACKING_DEFAULT: u32 = 0;
const NVFBC_BUFFER_FORMAT_BGRA: u32 = 5;

const NVFBC_TOSYS_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY: u32 = 4;

const CREATE_HANDLE_PARAMS_VERSION: u32 = 2;
const DESTROY_HANDLE_PARAMS_VERSION: u32 = 1;
const GET_STATUS_PARAMS_VERSION: u32 = 2;
const CREATE_CAPTURE_SESSION_PARAMS_VERSION: u32 = 6;
const DESTROY_CAPTURE_SESSION_PARAMS_VERSION: u32 = 1;
const TOSYS_SETUP_PARAMS_VERSION: u32 = 3;
const TOSYS_GRAB_FRAME_PARAMS_VERSION: u32 = 2;

const MAGIC_PRIVATE_DATA: [u32; 4] = [0xAEF57AC5, 0x401D1A39, 0x1B856BBE, 0x9ED0CEBA];

type SessionHandle = u64;
type NvFbcStatus = u32;

type FnGetLastErrorStr = unsafe extern "C" fn(SessionHandle) -> *const c_char;
type FnCreateHandle =
    unsafe extern "C" fn(*mut SessionHandle, *mut NvfbcCreateHandleParams) -> NvFbcStatus;
type FnDestroyHandle =
    unsafe extern "C" fn(SessionHandle, *mut NvfbcDestroyHandleParams) -> NvFbcStatus;
type FnGetStatus =
    unsafe extern "C" fn(SessionHandle, *mut NvfbcGetStatusParams) -> NvFbcStatus;
type FnCreateCaptureSession = unsafe extern "C" fn(
    SessionHandle,
    *mut NvfbcCreateCaptureSessionParams,
) -> NvFbcStatus;
type FnDestroyCaptureSession = unsafe extern "C" fn(
    SessionHandle,
    *mut NvfbcDestroyCaptureSessionParams,
) -> NvFbcStatus;
type FnToSysSetUp =
    unsafe extern "C" fn(SessionHandle, *mut NvfbcToSysSetupParams) -> NvFbcStatus;
type FnToSysGrabFrame =
    unsafe extern "C" fn(SessionHandle, *mut NvfbcToSysGrabFrameParams) -> NvFbcStatus;

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcBox {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcSize {
    w: u32,
    h: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcFrameGrabInfo {
    dwWidth: u32,
    dwHeight: u32,
    dwByteSize: u32,
    dwCurrentFrame: u32,
    bIsNewFrame: u32,
    ulTimestampUs: u64,
    dwMissedFrames: u32,
    bRequiredPostProcessing: u32,
    bDirectCapture: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcCreateHandleParams {
    dwVersion: u32,
    privateData: *const c_void,
    privateDataSize: u32,
    bExternallyManagedContext: u32,
    glxCtx: *mut c_void,
    glxFBConfig: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcDestroyHandleParams {
    dwVersion: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcRandrOutputInfo {
    dwId: u32,
    name: [c_char; 128],
    trackedBox: NvfbcBox,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcGetStatusParams {
    dwVersion: u32,
    bIsCapturePossible: u32,
    bCurrentlyCapturing: u32,
    bCanCreateNow: u32,
    screenSize: NvfbcSize,
    bXRandRAvailable: u32,
    outputs: [NvfbcRandrOutputInfo; 5],
    dwOutputNum: u32,
    dwNvFBCVersion: u32,
    bInModeset: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcCreateCaptureSessionParams {
    dwVersion: u32,
    eCaptureType: u32,
    eTrackingType: u32,
    dwOutputId: u32,
    captureBox: NvfbcBox,
    frameSize: NvfbcSize,
    bWithCursor: u32,
    bDisableAutoModesetRecovery: u32,
    bRoundFrameSize: u32,
    dwSamplingRateMs: u32,
    bPushModel: u32,
    bAllowDirectCapture: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcDestroyCaptureSessionParams {
    dwVersion: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcToSysSetupParams {
    dwVersion: u32,
    eBufferFormat: u32,
    ppBuffer: *mut *mut c_void,
    bWithDiffMap: u32,
    ppDiffMap: *mut *mut c_void,
    dwDiffMapScalingFactor: u32,
    diffMapSize: NvfbcSize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvfbcToSysGrabFrameParams {
    dwVersion: u32,
    dwFlags: u32,
    pFrameGrabInfo: *mut NvfbcFrameGrabInfo,
    dwTimeoutMs: u32,
}

fn nvfbc_struct_version<T>(version: u32) -> u32 {
    let nvfbc_version = NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8);
    size_of::<T>() as u32 | ((version << 16) | (nvfbc_version << 24))
}

fn status_code_message(code: NvFbcStatus) -> &'static str {
    match code {
        NVFBC_ERR_API_VERSION => "The API version between the client and the library is not compatible",
        NVFBC_ERR_INTERNAL => "An internal error occurred",
        NVFBC_ERR_INVALID_PARAM => "One or more of the parameters passed to the API call is invalid",
        NVFBC_ERR_INVALID_PTR => "One or more of the pointers passed to the API call is invalid",
        NVFBC_ERR_INVALID_HANDLE => "The session handle passed to the API call is invalid",
        NVFBC_ERR_MAX_CLIENTS => "The maximum number of threaded clients has been reached",
        NVFBC_ERR_UNSUPPORTED => "The requested feature is not supported by the library",
        NVFBC_ERR_OUT_OF_MEMORY => "Unable to allocate enough memory to perform the requested operation",
        NVFBC_ERR_BAD_REQUEST => "The API call was not expected in the current state",
        NVFBC_ERR_X => "An X error occurred in the NVIDIA driver",
        NVFBC_ERR_GLX => "A GLX error occurred in the NVIDIA driver",
        NVFBC_ERR_GL => "An OpenGL error occurred in the NVIDIA driver",
        NVFBC_ERR_CUDA => "A CUDA error occurred in the NVIDIA driver",
        NVFBC_ERR_ENCODER => "A hardware encoder error occurred in the NVIDIA driver",
        NVFBC_ERR_CONTEXT => "An NvFBC context error occurred",
        NVFBC_ERR_MUST_RECREATE => "The capture session must be recreated",
        NVFBC_ERR_VULKAN => "A Vulkan error occurred in the NVIDIA driver",
        _ => "An unknown NvFBC error occurred",
    }
}

fn common_library_dirs() -> &'static [&'static str] {
    &[
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/nvidia",
        "/usr/lib64/nvidia",
    ]
}

fn find_versioned_library_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "libnvidia-fbc.so"
            || name == "libnvidia-fbc.so.1"
            || name.starts_with("libnvidia-fbc.so.")
        {
            return Some(path);
        }
    }
    None
}

fn candidate_library_paths() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("ST_NVFBC_LIB") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(PathBuf::from("libnvidia-fbc.so.1"));
    candidates.push(PathBuf::from("libnvidia-fbc.so"));
    for dir in common_library_dirs() {
        let dir = Path::new(dir);
        if let Some(path) = find_versioned_library_in_dir(dir) {
            candidates.push(path);
        }
    }
    candidates
}

fn load_library() -> Result<Library, String> {
    let mut tried = Vec::new();
    for candidate in candidate_library_paths() {
        let display = candidate.display().to_string();
        tried.push(display.clone());
        let lib = unsafe { Library::new(&candidate) };
        if let Ok(lib) = lib {
            return Ok(lib);
        }
    }
    Err(format!(
        "libnvidia-fbc was not found on this machine (tried: {})",
        tried.join(", ")
    ))
}

struct NvFbcApi {
    _library: Library,
    get_last_error_str: FnGetLastErrorStr,
    create_handle: FnCreateHandle,
    destroy_handle: FnDestroyHandle,
    get_status: FnGetStatus,
    create_capture_session: FnCreateCaptureSession,
    destroy_capture_session: FnDestroyCaptureSession,
    to_sys_set_up: FnToSysSetUp,
    to_sys_grab_frame: FnToSysGrabFrame,
}

unsafe impl Send for NvFbcApi {}

impl NvFbcApi {
    fn load() -> Result<Self, String> {
        let library = load_library()?;
        unsafe {
            let get_last_error_str = *library
                .get::<FnGetLastErrorStr>(b"NvFBCGetLastErrorStr\0")
                .map_err(|err| format!("failed to load NvFBCGetLastErrorStr: {err}"))?;
            let create_handle = *library
                .get::<FnCreateHandle>(b"NvFBCCreateHandle\0")
                .map_err(|err| format!("failed to load NvFBCCreateHandle: {err}"))?;
            let destroy_handle = *library
                .get::<FnDestroyHandle>(b"NvFBCDestroyHandle\0")
                .map_err(|err| format!("failed to load NvFBCDestroyHandle: {err}"))?;
            let get_status = *library
                .get::<FnGetStatus>(b"NvFBCGetStatus\0")
                .map_err(|err| format!("failed to load NvFBCGetStatus: {err}"))?;
            let create_capture_session = *library
                .get::<FnCreateCaptureSession>(b"NvFBCCreateCaptureSession\0")
                .map_err(|err| format!("failed to load NvFBCCreateCaptureSession: {err}"))?;
            let destroy_capture_session = *library
                .get::<FnDestroyCaptureSession>(b"NvFBCDestroyCaptureSession\0")
                .map_err(|err| format!("failed to load NvFBCDestroyCaptureSession: {err}"))?;
            let to_sys_set_up = *library
                .get::<FnToSysSetUp>(b"NvFBCToSysSetUp\0")
                .map_err(|err| format!("failed to load NvFBCToSysSetUp: {err}"))?;
            let to_sys_grab_frame = *library
                .get::<FnToSysGrabFrame>(b"NvFBCToSysGrabFrame\0")
                .map_err(|err| format!("failed to load NvFBCToSysGrabFrame: {err}"))?;

            Ok(Self {
                _library: library,
                get_last_error_str,
                create_handle,
                destroy_handle,
                get_status,
                create_capture_session,
                destroy_capture_session,
                to_sys_set_up,
                to_sys_grab_frame,
            })
        }
    }

    fn last_error(&self, handle: SessionHandle) -> Option<String> {
        if handle == 0 {
            return None;
        }
        let ptr = unsafe { (self.get_last_error_str)(handle) };
        if ptr.is_null() {
            return None;
        }
        let message = unsafe { CStr::from_ptr(ptr) };
        message.to_str().ok().map(ToOwned::to_owned)
    }

    fn check_ret(&self, handle: SessionHandle, ret: NvFbcStatus) -> Result<(), String> {
        if ret == NVFBC_SUCCESS {
            return Ok(());
        }
        if let Some(message) = self.last_error(handle) {
            if !message.is_empty() {
                return Err(format!("{}: {}", status_code_message(ret), message));
            }
        }
        Err(status_code_message(ret).to_string())
    }
}

enum CaptureMethod {
    Blocking = NVFBC_TOSYS_GRAB_FLAGS_NOWAIT_IF_NEW_FRAME_READY as isize,
}

struct DynamicSystemCapturer {
    api: NvFbcApi,
    handle: SessionHandle,
    buffer: Box<Cell<*mut c_void>>,
}

unsafe impl Send for DynamicSystemCapturer {}

struct CaptureStatus {
    is_capture_possible: bool,
    can_create_now: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct NvfbcProbe {
    pub is_capture_possible: bool,
    pub can_create_now: bool,
}

impl DynamicSystemCapturer {
    fn new() -> Result<Self, String> {
        let api = NvFbcApi::load()?;
        let mut params: NvfbcCreateHandleParams = unsafe { MaybeUninit::zeroed().assume_init() };
        params.dwVersion = nvfbc_struct_version::<NvfbcCreateHandleParams>(CREATE_HANDLE_PARAMS_VERSION);
        params.privateData = MAGIC_PRIVATE_DATA.as_ptr().cast();
        params.privateDataSize = size_of::<[u32; 4]>() as u32;
        params.bExternallyManagedContext = NVFBC_FALSE;

        let mut handle = 0;
        let ret = unsafe { (api.create_handle)(&mut handle, &mut params) };
        if ret != NVFBC_SUCCESS {
            return Err(format!("failed to create NvFBC handle: {}", status_code_message(ret)));
        }

        Ok(Self {
            api,
            handle,
            buffer: Box::new(Cell::new(std::ptr::null_mut())),
        })
    }

    fn status(&self) -> Result<CaptureStatus, String> {
        let mut params: NvfbcGetStatusParams = unsafe { MaybeUninit::zeroed().assume_init() };
        params.dwVersion = nvfbc_struct_version::<NvfbcGetStatusParams>(GET_STATUS_PARAMS_VERSION);
        let ret = unsafe { (self.api.get_status)(self.handle, &mut params) };
        self.api.check_ret(self.handle, ret)?;
        Ok(CaptureStatus {
            is_capture_possible: params.bIsCapturePossible == NVFBC_TRUE,
            can_create_now: params.bCanCreateNow == NVFBC_TRUE,
        })
    }

    fn start(&mut self, fps: u32) -> Result<(), String> {
        let mut session_params: NvfbcCreateCaptureSessionParams =
            unsafe { MaybeUninit::zeroed().assume_init() };
        session_params.dwVersion =
            nvfbc_struct_version::<NvfbcCreateCaptureSessionParams>(
                CREATE_CAPTURE_SESSION_PARAMS_VERSION,
            );
        session_params.eCaptureType = NVFBC_CAPTURE_TO_SYS;
        session_params.eTrackingType = NVFBC_TRACKING_DEFAULT;
        session_params.frameSize = NvfbcSize { w: 0, h: 0 };
        session_params.bWithCursor = NVFBC_TRUE;
        session_params.dwSamplingRateMs = (1000 / fps.max(1)).max(1);
        let ret = unsafe { (self.api.create_capture_session)(self.handle, &mut session_params) };
        self.api.check_ret(self.handle, ret)?;

        let mut setup_params: NvfbcToSysSetupParams =
            unsafe { MaybeUninit::zeroed().assume_init() };
        setup_params.dwVersion =
            nvfbc_struct_version::<NvfbcToSysSetupParams>(TOSYS_SETUP_PARAMS_VERSION);
        setup_params.eBufferFormat = NVFBC_BUFFER_FORMAT_BGRA;
        setup_params.ppBuffer = self.buffer.as_ptr();
        let ret = unsafe { (self.api.to_sys_set_up)(self.handle, &mut setup_params) };
        self.api.check_ret(self.handle, ret)
    }

    fn stop(&self) {
        let mut params: NvfbcDestroyCaptureSessionParams =
            unsafe { MaybeUninit::zeroed().assume_init() };
        params.dwVersion = nvfbc_struct_version::<NvfbcDestroyCaptureSessionParams>(
            DESTROY_CAPTURE_SESSION_PARAMS_VERSION,
        );
        let _ = self
            .api
            .check_ret(self.handle, unsafe { (self.api.destroy_capture_session)(self.handle, &mut params) });
    }

    fn next_frame(
        &mut self,
        capture_method: CaptureMethod,
        timeout: Option<Duration>,
    ) -> Result<SystemFrameInfo<'_>, String> {
        let mut frame_info: NvfbcFrameGrabInfo = unsafe { MaybeUninit::zeroed().assume_init() };
        let mut params: NvfbcToSysGrabFrameParams =
            unsafe { MaybeUninit::zeroed().assume_init() };
        params.dwVersion =
            nvfbc_struct_version::<NvfbcToSysGrabFrameParams>(TOSYS_GRAB_FRAME_PARAMS_VERSION);
        params.dwFlags = capture_method as u32;
        params.pFrameGrabInfo = &mut frame_info;
        if let Some(timeout) = timeout {
            params.dwTimeoutMs = timeout.as_millis().min(u32::MAX as u128) as u32;
        }

        let ret = unsafe { (self.api.to_sys_grab_frame)(self.handle, &mut params) };
        self.api.check_ret(self.handle, ret)?;

        let buffer_ptr = unsafe { self.buffer.as_ptr().read_volatile().cast::<u8>() };
        if buffer_ptr.is_null() {
            return Err("NvFBC returned a null frame buffer".into());
        }
        let buffer =
            unsafe { std::slice::from_raw_parts(buffer_ptr, frame_info.dwByteSize as usize) };

        Ok(SystemFrameInfo {
            buffer,
            width: frame_info.dwWidth,
            height: frame_info.dwHeight,
            current_frame: frame_info.dwCurrentFrame,
            is_new_frame: frame_info.bIsNewFrame != 0,
        })
    }
}

pub(crate) fn probe() -> Result<NvfbcProbe, String> {
    let capturer = DynamicSystemCapturer::new()?;
    let status = capturer.status()?;
    Ok(NvfbcProbe {
        is_capture_possible: status.is_capture_possible,
        can_create_now: status.can_create_now,
    })
}

impl Drop for DynamicSystemCapturer {
    fn drop(&mut self) {
        let mut params: NvfbcDestroyHandleParams = unsafe { MaybeUninit::zeroed().assume_init() };
        params.dwVersion =
            nvfbc_struct_version::<NvfbcDestroyHandleParams>(DESTROY_HANDLE_PARAMS_VERSION);
        let _ = self
            .api
            .check_ret(self.handle, unsafe { (self.api.destroy_handle)(self.handle, &mut params) });
    }
}

struct SystemFrameInfo<'a> {
    buffer: &'a [u8],
    width: u32,
    height: u32,
    current_frame: u32,
    is_new_frame: bool,
}

struct CapturerSendWrapper(DynamicSystemCapturer);
unsafe impl Send for CapturerSendWrapper {}

pub struct NvfbcCapture {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl NvfbcCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            handle: None,
        }
    }
}

impl CaptureBackend for NvfbcCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Capture already running".into());
        }

        let mut capturer = DynamicSystemCapturer::new()?;
        let status = capturer.status()?;
        if !status.can_create_now {
            return Err("Cannot create NVFBC capture session".into());
        }
        capturer.start(target_fps())?;

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let wrapped_capturer = CapturerSendWrapper(capturer);

        let handle = thread::spawn(move || {
            let mut capturer = wrapped_capturer.0;
            let trace = std::env::var_os("ST_TRACE").is_some();
            let mut dropped_frames = 0usize;
            while running.load(Ordering::SeqCst) {
                match capturer.next_frame(
                    CaptureMethod::Blocking,
                    Some(Duration::from_millis(50)),
                ) {
                    Ok(frame_info) => {
                        let _ = frame_info.current_frame;
                        let _ = frame_info.is_new_frame;
                        let frame = CapturedFrame {
                            data: FrameData::Ram(frame_info.buffer.to_vec()),
                            width: frame_info.width,
                            height: frame_info.height,
                            cursor: None,
                        };
                        match tx.try_send(frame) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                if trace && dropped_frames < 8 {
                                    eprintln!(
                                        "[trace][nvfbc] dropped captured frame because capture channel is full"
                                    );
                                }
                                dropped_frames = dropped_frames.saturating_add(1);
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(_) => {
                        // Timeout or another driver error. Continue so stop() can break the loop.
                    }
                }
            }
            capturer.stop();
        });

        self.handle = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
