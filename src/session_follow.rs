//! System-wide mode: follow the active seat's user for audio capture.
//!
//! Video and input work at the seat level regardless of which user is logged in
//! (KMS captures the active scanout; uinput injects at the kernel). Audio is the
//! exception: PulseAudio / PipeWire run per-user under `/run/user/<uid>`. A root
//! system service has no audio daemon of its own, so this watcher tracks the
//! active session's uid via logind and, on every change, repoints
//! `PULSE_SERVER` / `XDG_RUNTIME_DIR` at that user and re-attaches the audio
//! pipeline (which re-detects the monitor source against the new daemon).
//!
//! At the login screen the active session is the greeter (usually no audio), so
//! the stream is silent until a real user logs in — then audio follows them.
//!
//! Default-on in system mode; `ST_AUDIO_FOLLOW=0` (also `false`/`no`/`off`)
//! disables it. Needs root to read another user's `/run/user/<uid>`.

#![cfg(target_os = "linux")]

use crate::audio::AudioPipeline;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Spawn the audio-follow watcher if running in system-wide mode and not
/// disabled. No-op otherwise (a per-user service already sits in the user's
/// session and needs no following).
pub fn maybe_spawn(audio: Arc<Mutex<AudioPipeline>>) {
    if std::env::var_os("ST_SYSTEM_MODE").is_none() {
        return;
    }
    if disabled() {
        println!("[session-follow] disabled via ST_AUDIO_FOLLOW");
        return;
    }
    thread::spawn(move || run(audio));
}

fn disabled() -> bool {
    matches!(
        std::env::var("ST_AUDIO_FOLLOW")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn run(audio: Arc<Mutex<AudioPipeline>>) {
    println!("[session-follow] watching seat0 for the active user");
    let mut current: Option<u32> = None;
    loop {
        let active = active_uid();
        if active != current {
            match active {
                Some(uid) => {
                    apply_user_env(uid);
                    println!("[session-follow] active user uid={uid}; re-attaching audio");
                }
                None => println!("[session-follow] no active graphical user; audio idle"),
            }
            current = active;
            if let Ok(mut pipeline) = audio.lock() {
                pipeline.apply_auto_detect();
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Point this process's audio environment at the given user's runtime dir so
/// libpulse and the `pactl` probe both reach that user's daemon. On PipeWire
/// (pipewire-pulse) root is granted access to the socket directly; classic
/// PulseAudio additionally needs the user's auth cookie, which `PULSE_COOKIE`
/// covers when present.
fn apply_user_env(uid: u32) {
    let run_dir = format!("/run/user/{uid}");
    std::env::set_var("XDG_RUNTIME_DIR", &run_dir);
    std::env::set_var("PULSE_SERVER", format!("unix:{run_dir}/pulse/native"));
    let cookie = format!("{run_dir}/pulse/cookie");
    if std::path::Path::new(&cookie).exists() {
        std::env::set_var("PULSE_COOKIE", cookie);
    }
}

/// The uid of the active session on seat0, or `None` if there is none.
fn active_uid() -> Option<u32> {
    active_uid_from_loginctl().or_else(active_uid_from_seat_file)
}

fn active_uid_from_loginctl() -> Option<u32> {
    let session = loginctl_value(&["show-seat", "seat0", "--value", "-p", "ActiveSession"])?;
    if session.is_empty() {
        return None;
    }
    let uid = loginctl_value(&["show-session", &session, "--value", "-p", "User"])?;
    uid.parse().ok()
}

fn loginctl_value(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("loginctl")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Fallback when `loginctl` is unavailable: parse the (private but stable)
/// logind seat state file for `ACTIVE_UID`.
fn active_uid_from_seat_file() -> Option<u32> {
    let content = std::fs::read_to_string("/run/systemd/seats/seat0").ok()?;
    content
        .lines()
        .find_map(|line| line.strip_prefix("ACTIVE_UID="))
        .and_then(|v| v.trim().parse().ok())
}
