//! Linux capture dispatcher.
//!
//! Auto-detects the display server and selects the best capture backend,
//! matching Sunshine's detection priority (misc.cpp:1044-1070):
//!
//!   1. NvFBC (NVIDIA only, X11)
//!   2. XDG Portal / PipeWire (preferred Wayland interactive path)
//!   3. Wayland wlroots screencopy (viewer-oriented wlroots fallback)
//!   4. KMS/DRM (requires root or video group)
//!   5. X11 XShm (always enumerated as fallback for software encoding)
//!
//! Override with env var: `ST_CAPTURE=kms|nvfbc|pipewire|wayland|x11`

use super::{CaptureBackend, CapturedFrame};
use crossbeam_channel::Sender;
use std::time::Duration;

pub mod kms_capture;
mod nvfbc_capture;
mod pipewire_capture;
pub mod wl_capture;
pub mod x11_capture;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayServer {
    X11,
    Wayland,
    Unknown,
}

pub fn target_fps() -> u32 {
    super::target_fps()
}

pub fn target_frame_interval() -> Duration {
    Duration::from_secs_f64(1.0 / target_fps() as f64)
}

fn detect_display_server() -> DisplayServer {
    // XDG_SESSION_TYPE is the most reliable indicator
    if let Ok(session_type) = std::env::var("XDG_SESSION_TYPE") {
        match session_type.to_lowercase().as_str() {
            "wayland" => return DisplayServer::Wayland,
            "x11" => return DisplayServer::X11,
            _ => {}
        }
    }

    // Fallback: check for WAYLAND_DISPLAY
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return DisplayServer::Wayland;
    }

    // Fallback: check for DISPLAY (X11)
    if std::env::var("DISPLAY").is_ok() {
        return DisplayServer::X11;
    }

    DisplayServer::Unknown
}

enum Backend {
    NvFbc(nvfbc_capture::NvfbcCapture),
    Wayland(wl_capture::WaylandCapture),
    Kms(kms_capture::KmsCapture),
    X11(x11_capture::X11Capture),
    PipeWire(pipewire_capture::PipeWireCapture),
}

pub struct PlatformCapture {
    backend: Backend,
    display_server: DisplayServer,
}

impl PlatformCapture {
    pub fn new() -> Self {
        // Check for explicit override
        if let Ok(override_val) = std::env::var("ST_CAPTURE") {
            let backend = match override_val.to_lowercase().as_str() {
                "nvfbc" => {
                    println!("[capture] ST_CAPTURE=nvfbc override: using NvFBC capture");
                    Backend::NvFbc(nvfbc_capture::NvfbcCapture::new())
                }
                "wayland" | "wlr" => {
                    println!("[capture] ST_CAPTURE=wayland override: using Wayland screencopy");
                    Backend::Wayland(wl_capture::WaylandCapture::new())
                }
                "kms" => {
                    println!("[capture] ST_CAPTURE=kms override: using KMS capture");
                    Backend::Kms(kms_capture::KmsCapture::new())
                }
                "x11" => {
                    println!("[capture] ST_CAPTURE=x11 override: using X11 XShm capture");
                    Backend::X11(x11_capture::X11Capture::new())
                }
                "pipewire" | "portal" => {
                    println!("[capture] ST_CAPTURE=pipewire override: using PipeWire capture");
                    Backend::PipeWire(pipewire_capture::PipeWireCapture::new())
                }
                other => {
                    eprintln!("[capture] Unknown ST_CAPTURE value '{other}', ignoring");
                    return Self::auto_select();
                }
            };
            return Self {
                backend,
                display_server: detect_display_server(),
            };
        }

        Self::auto_select()
    }

    /// Sunshine-style auto-selection matching misc.cpp:1044-1070.
    ///
    /// Priority:
    ///   1. NvFBC — best on NVIDIA + X11
    ///   2. Wayland screencopy — best on wlroots compositors
    ///   3. KMS — direct framebuffer, works headless
    ///   4. X11 — XShm, always available on X11
    ///   5. PipeWire portal — universal Wayland fallback
    fn auto_select() -> Self {
        let display_server = detect_display_server();
        println!("[capture] Detected display server: {display_server:?}");

        // Pre-validate available backends
        let x11_ok = x11_capture::verify_x11();
        let wl_ok = wl_capture::verify_wayland();
        println!("[capture] Pre-validation: X11={x11_ok}, Wayland={wl_ok}");

        // Current priority: NvFBC > PipeWire > Wayland > KMS > X11
        // The actual backend used depends on which one successfully starts.
        // Here we just pick the initial candidate — fallback happens in start().

        let backend = match display_server {
            DisplayServer::X11 => {
                // On X11: NvFBC (NVIDIA) > KMS > X11 XShm > PipeWire
                println!("[capture] X11 detected — trying NvFBC first");
                Backend::NvFbc(nvfbc_capture::NvfbcCapture::new())
            }
            DisplayServer::Wayland => {
                // Prefer PipeWire on Wayland because it can expose cursor metadata
                // and DMA-BUF-backed frames without embedding the cursor into video.
                println!("[capture] Wayland detected — trying PipeWire portal first");
                let _ = wl_ok;
                Backend::PipeWire(pipewire_capture::PipeWireCapture::new())
            }
            DisplayServer::Unknown => {
                // Headless/Unknown: KMS > PipeWire
                println!("[capture] Unknown display server — trying KMS first");
                Backend::Kms(kms_capture::KmsCapture::new())
            }
        };

        Self {
            backend,
            display_server,
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match &self.backend {
            Backend::NvFbc(_) => "nvfbc",
            Backend::Wayland(_) => "wayland-screencopy",
            Backend::Kms(_) => "kms",
            Backend::X11(_) => "x11",
            Backend::PipeWire(_) => "pipewire",
        }
    }
}

impl CaptureBackend for PlatformCapture {
    fn start(&mut self, tx: Sender<CapturedFrame>) -> Result<(), String> {
        match &mut self.backend {
            // === NvFBC path (X11) ===
            // Fallback: NvFBC → KMS → X11 → PipeWire
            Backend::NvFbc(b) => match b.start(tx.clone()) {
                Ok(()) => Ok(()),
                Err(nvfbc_err) => {
                    eprintln!("[capture] NvFBC failed ({nvfbc_err}), trying KMS...");
                    let mut kms = kms_capture::KmsCapture::new();
                    match kms.start(tx.clone()) {
                        Ok(()) => {
                            self.backend = Backend::Kms(kms);
                            Ok(())
                        }
                        Err(kms_err) => {
                            eprintln!("[capture] KMS failed ({kms_err}), trying X11 XShm...");
                            let mut x11 = x11_capture::X11Capture::new();
                            match x11.start(tx.clone()) {
                                Ok(()) => {
                                    self.backend = Backend::X11(x11);
                                    Ok(())
                                }
                                Err(x11_err) => {
                                    eprintln!(
                                        "[capture] X11 failed ({x11_err}), trying PipeWire..."
                                    );
                                    let mut pw = pipewire_capture::PipeWireCapture::new();
                                    let result = pw.start(tx);
                                    if result.is_ok() {
                                        self.backend = Backend::PipeWire(pw);
                                    }
                                    result.map_err(|pw_err| {
                                        format!(
                                            "All capture backends failed.\n  NvFBC: {nvfbc_err}\n  KMS: {kms_err}\n  X11: {x11_err}\n  PipeWire: {pw_err}"
                                        )
                                    })
                                }
                            }
                        }
                    }
                }
            },

            // === Wayland screencopy path ===
            // Fallback: Wayland screencopy → KMS → PipeWire
            // Note: X11 (XWayland) is NOT in this chain — XWayland root window
            // doesn't contain Wayland desktop content.
            Backend::Wayland(b) => match b.start(tx.clone()) {
                Ok(()) => Ok(()),
                Err(wl_err) => {
                    eprintln!("[capture] Wayland screencopy failed ({wl_err}), trying KMS...");
                    let mut kms = kms_capture::KmsCapture::new();
                    match kms.start(tx.clone()) {
                        Ok(()) => {
                            self.backend = Backend::Kms(kms);
                            Ok(())
                        }
                        Err(kms_err) => {
                            eprintln!("[capture] KMS failed ({kms_err}), trying PipeWire...");
                            let mut pw = pipewire_capture::PipeWireCapture::new();
                            let result = pw.start(tx);
                            if result.is_ok() {
                                self.backend = Backend::PipeWire(pw);
                            }
                            result.map_err(|pw_err| {
                                format!(
                                    "All capture backends failed.\n  Wayland: {wl_err}\n  KMS: {kms_err}\n  PipeWire: {pw_err}"
                                )
                            })
                        }
                    }
                }
            },

            // === KMS path ===
            // Fallback: KMS → X11 → PipeWire (on native X11)
            //           KMS → PipeWire (on Wayland — X11/XWayland can't capture desktop)
            Backend::Kms(b) => match b.start(tx.clone()) {
                Ok(()) => Ok(()),
                Err(kms_err) => {
                    if self.display_server != DisplayServer::Wayland {
                        eprintln!("[capture] KMS failed ({kms_err}), trying X11...");
                        let mut x11 = x11_capture::X11Capture::new();
                        match x11.start(tx.clone()) {
                            Ok(()) => {
                                self.backend = Backend::X11(x11);
                                return Ok(());
                            }
                            Err(x11_err) => {
                                eprintln!("[capture] X11 failed ({x11_err}), trying PipeWire...");
                            }
                        }
                    } else {
                        eprintln!("[capture] KMS failed ({kms_err}), trying PipeWire...");
                    }
                    let mut pw = pipewire_capture::PipeWireCapture::new();
                    let result = pw.start(tx);
                    if result.is_ok() {
                        self.backend = Backend::PipeWire(pw);
                    }
                    result.map_err(|pw_err| {
                        format!(
                            "All capture backends failed.\n  KMS: {kms_err}\n  PipeWire: {pw_err}"
                        )
                    })
                }
            },

            // === X11 path ===
            // Fallback: X11 → PipeWire
            Backend::X11(b) => match b.start(tx.clone()) {
                Ok(()) => Ok(()),
                Err(x11_err) => {
                    eprintln!("[capture] X11 failed ({x11_err}), trying PipeWire...");
                    let mut pw = pipewire_capture::PipeWireCapture::new();
                    let result = pw.start(tx);
                    if result.is_ok() {
                        self.backend = Backend::PipeWire(pw);
                    }
                    result.map_err(|pw_err| {
                        format!(
                            "All capture backends failed.\n  X11: {x11_err}\n  PipeWire: {pw_err}"
                        )
                    })
                }
            },

            // === PipeWire path ===
            // Fallback: PipeWire → Wayland screencopy → KMS
            Backend::PipeWire(b) => {
                match b.start(tx.clone()) {
                    Ok(()) => Ok(()),
                    Err(pw_err) => {
                        if self.display_server == DisplayServer::Wayland
                            && wl_capture::verify_wayland()
                        {
                            eprintln!("[capture] PipeWire failed ({pw_err}), trying Wayland screencopy...");
                            let mut wl = wl_capture::WaylandCapture::new();
                            match wl.start(tx.clone()) {
                                Ok(()) => {
                                    self.backend = Backend::Wayland(wl);
                                    Ok(())
                                }
                                Err(wl_err) => {
                                    eprintln!("[capture] Wayland screencopy failed ({wl_err}), trying KMS...");
                                    let mut kms = kms_capture::KmsCapture::new();
                                    let result = kms.start(tx);
                                    if result.is_ok() {
                                        self.backend = Backend::Kms(kms);
                                    }
                                    result.map_err(|kms_err| {
                                    format!(
                                        "All capture backends failed.\n  PipeWire: {pw_err}\n  Wayland: {wl_err}\n  KMS: {kms_err}"
                                    )
                                })
                                }
                            }
                        } else {
                            Err(pw_err)
                        }
                    }
                }
            }
        }
    }

    fn stop(&mut self) {
        match &mut self.backend {
            Backend::NvFbc(b) => b.stop(),
            Backend::Wayland(b) => b.stop(),
            Backend::Kms(b) => b.stop(),
            Backend::X11(b) => b.stop(),
            Backend::PipeWire(b) => b.stop(),
        }
    }
}
