//! What a client can ask to stream, and where each output sits in the
//! compositor's desktop.
//!
//! Capture backends know connectors and scanout modes; only the compositor
//! knows the desktop arrangement (position, rotation, which output is
//! primary). The directory merges both: every streamed output runs its own
//! pipeline keyed by the id resolved here, and absolute pointer input is
//! mapped through the arrangement, because an absolute input device spans the
//! whole desktop rather than the output a client is looking at.

use crate::capture::CaptureOutput;
use st_protocol::control::{OutputInfo, OUTPUT_DEFAULT, OUTPUT_VIRTUAL};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Pipeline key of a backend that cannot enumerate outputs.
pub const SINGLE_OUTPUT: u32 = OUTPUT_DEFAULT;

/// FNV-1a of the connector name: the id survives restarts, so a client's
/// remembered choice keeps resolving to the same monitor.
pub fn output_id(name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in name.as_bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    match hash {
        OUTPUT_DEFAULT => 1,
        OUTPUT_VIRTUAL => OUTPUT_VIRTUAL - 1,
        hash => hash,
    }
}

/// One output as the compositor arranges it, in logical desktop units.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutOutput {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Clockwise quarter turns that bring the scanout upright.
    pub rotation: u8,
    pub primary: bool,
    pub refresh_millihz: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    info: OutputInfo,
    /// Logical desktop rectangle; `None` when the arrangement is unknown.
    rect: Option<(i32, i32, u32, u32)>,
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    /// Size of the desktop every absolute pointer position is scaled over.
    desktop: Option<(u32, u32)>,
    virtual_supported: bool,
    updated_at: Option<Instant>,
}

#[derive(Default)]
pub struct OutputDirectory {
    inner: Mutex<Inner>,
    generation: AtomicU64,
    refresh_requested: AtomicBool,
}

impl OutputDirectory {
    /// Ask the watcher for a re-enumeration ahead of its next periodic one.
    pub fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::Release);
    }

    pub fn take_refresh_request(&self) -> bool {
        self.refresh_requested.swap(false, Ordering::AcqRel)
    }

    /// Bumped whenever the published list changes; handlers re-send
    /// `AvailableOutputs` when it moves.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether the last enumeration is at most `max_age` old.
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.inner
            .lock()
            .unwrap()
            .updated_at
            .is_some_and(|at| at.elapsed() <= max_age)
    }

    pub fn list(&self) -> (Vec<OutputInfo>, bool) {
        let inner = self.inner.lock().unwrap();
        (
            inner.entries.iter().map(|e| e.info.clone()).collect(),
            inner.virtual_supported,
        )
    }

    pub fn info(&self, id: u32) -> Option<OutputInfo> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .find(|e| e.info.id == id)
            .map(|e| e.info.clone())
    }

    /// The pipeline key for a client's request: the output itself when it
    /// exists, else the primary; [`SINGLE_OUTPUT`] when no monitor is
    /// enumerable. A virtual output is only ever streamed by request.
    pub fn resolve(&self, requested: u32) -> u32 {
        let inner = self.inner.lock().unwrap();
        let entries = &inner.entries;
        let mut physical = entries.iter().filter(|e| !e.info.is_virtual);
        entries
            .iter()
            .find(|e| e.info.id == requested)
            .or_else(|| entries.iter().find(|e| e.info.is_primary))
            .or_else(|| physical.next())
            .map_or(SINGLE_OUTPUT, |e| e.info.id)
    }

    pub fn set_virtual_supported(&self, supported: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.virtual_supported != supported {
            inner.virtual_supported = supported;
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Replace the directory with a fresh enumeration. `virtual_outputs` are
    /// compositor outputs without a connector that some session streams.
    pub fn update(
        &self,
        capture: &[CaptureOutput],
        virtual_outputs: &[String],
        layout: Option<&[LayoutOutput]>,
    ) {
        let entries = merge(capture, virtual_outputs, layout);
        let desktop = layout.and_then(desktop_size);
        let mut inner = self.inner.lock().unwrap();
        let changed = inner.entries != entries || inner.desktop != desktop;
        inner.entries = entries;
        inner.desktop = desktop;
        inner.updated_at = Some(Instant::now());
        if changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Scale a position on one output (0..=65535 across its upright picture)
    /// to the same range across the whole desktop. `None` when the
    /// arrangement is unknown: the caller then passes the position through.
    pub fn map_absolute(&self, output: u32, x: u16, y: u16) -> Option<(u16, u16)> {
        let inner = self.inner.lock().unwrap();
        let (desk_w, desk_h) = inner.desktop?;
        let rect = inner.entries.iter().find(|e| e.info.id == output)?.rect?;
        Some((
            map_axis(x, rect.0, rect.2, desk_w),
            map_axis(y, rect.1, rect.3, desk_h),
        ))
    }
}

/// libinput scales an absolute axis over the desktop as `v * size / 65536`
/// with the desktop origin at 0. Returns the smallest value that scales into
/// the output pixel under `v`, so the pointer never spills onto a neighbour.
fn map_axis(v: u16, origin: i32, len: u32, desktop: u32) -> u16 {
    if desktop == 0 || len == 0 {
        return v;
    }
    let pixel = origin as i64 + (v as i64 * (len as i64 - 1) + 32767) / 65535;
    (pixel.max(0) as u64 * 65536)
        .div_ceil(desktop as u64)
        .min(65535) as u16
}

fn desktop_size(layout: &[LayoutOutput]) -> Option<(u32, u32)> {
    let right = layout.iter().map(|o| o.x + o.width as i32).max()?;
    let bottom = layout.iter().map(|o| o.y + o.height as i32).max()?;
    (right > 0 && bottom > 0).then_some((right as u32, bottom as u32))
}

fn merge(
    capture: &[CaptureOutput],
    virtual_outputs: &[String],
    layout: Option<&[LayoutOutput]>,
) -> Vec<Entry> {
    let placed = |name: &str| layout.and_then(|l| l.iter().find(|o| o.name == name));
    let mut entries: Vec<Entry> = capture
        .iter()
        .map(|out| {
            let place = placed(&out.name);
            let turns = place.map_or(0, |p| p.rotation);
            let (width, height) = if turns & 1 == 1 {
                (out.height, out.width)
            } else {
                (out.width, out.height)
            };
            Entry {
                info: OutputInfo {
                    id: out.id,
                    name: out.name.clone(),
                    width,
                    height,
                    x: place.map_or(0, |p| p.x),
                    y: place.map_or(0, |p| p.y),
                    is_primary: place.is_some_and(|p| p.primary),
                    is_virtual: false,
                    refresh_millihz: out.refresh_millihz,
                    rotation: if out.plane_rotated { 0 } else { turns },
                },
                rect: place.map(|p| (p.x, p.y, p.width, p.height)),
            }
        })
        .collect();
    entries.extend(virtual_outputs.iter().filter_map(|name| {
        let place = placed(name)?;
        Some(Entry {
            info: OutputInfo {
                id: output_id(name),
                name: name.clone(),
                width: place.width,
                height: place.height,
                x: place.x,
                y: place.y,
                is_primary: false,
                is_virtual: true,
                refresh_millihz: place.refresh_millihz,
                rotation: 0,
            },
            rect: Some((place.x, place.y, place.width, place.height)),
        })
    }));
    // The default output is always a monitor: `capture` comes first.
    if !entries.iter().any(|e| e.info.is_primary) {
        if let Some(first) = entries.first_mut().filter(|e| !e.info.is_virtual) {
            first.info.is_primary = true;
        }
    }
    entries.sort_by_key(|e| (!e.info.is_primary, e.info.is_virtual, e.info.x, e.info.y));
    entries
}

/// Parse `kscreen-doctor -j`. `rotation` is RandR's bitmask: 1 none, 2 left
/// (picture turned 90° counter-clockwise), 4 inverted, 8 right.
pub fn parse_kscreen(json: &str) -> Option<Vec<LayoutOutput>> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let outputs = root.get("outputs")?.as_array()?;
    let int = |v: &serde_json::Value, key: &str| v.get(key).and_then(|n| n.as_f64());
    let layout: Vec<LayoutOutput> = outputs
        .iter()
        .filter(|o| o.get("enabled").and_then(|e| e.as_bool()) == Some(true))
        .filter_map(|o| {
            let pos = o.get("pos")?;
            let size = o.get("size")?;
            let mode_id = o.get("currentModeId").and_then(|m| m.as_str());
            let refresh = o
                .get("modes")
                .and_then(|m| m.as_array())
                .and_then(|modes| {
                    modes
                        .iter()
                        .find(|m| m.get("id").and_then(|id| id.as_str()) == mode_id)
                })
                .and_then(|m| int(m, "refreshRate"))
                .unwrap_or(0.0);
            Some(LayoutOutput {
                name: o.get("name")?.as_str()?.to_string(),
                x: int(pos, "x")? as i32,
                y: int(pos, "y")? as i32,
                width: int(size, "width")? as u32,
                height: int(size, "height")? as u32,
                rotation: match int(o, "rotation").unwrap_or(1.0) as u32 {
                    2 => 1,
                    4 => 2,
                    8 => 3,
                    _ => 0,
                },
                primary: int(o, "priority") == Some(1.0),
                refresh_millihz: (refresh * 1000.0).round() as u32,
            })
        })
        .collect();
    (!layout.is_empty()).then_some(layout)
}

/// Ask the compositor for the desktop arrangement. Only KDE answers today;
/// elsewhere the arrangement stays unknown and absolute input passes through.
#[cfg(target_os = "linux")]
pub fn query_layout() -> Option<Vec<LayoutOutput>> {
    let mut command = std::process::Command::new("kscreen-doctor");
    command
        .arg("-j")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if std::env::var_os("ST_SYSTEM_MODE").is_some() {
        crate::session_follow::run_as_active_user(&mut command)?;
    }
    let mut child = command.spawn().ok()?;
    // Read while it runs: a desktop with many monitors prints more than a
    // pipe holds, and the tool would block on a pipe nobody drains.
    let mut stdout = child.stdout.take()?;
    let (json_tx, json_rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let mut json = String::new();
        let read = std::io::Read::read_to_string(&mut stdout, &mut json);
        let _ = json_tx.send(read.map(|_| json));
    });
    let json = json_rx.recv_timeout(Duration::from_secs(3));
    if json.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    parse_kscreen(&json.ok()?.ok()?)
}

#[cfg(not(target_os = "linux"))]
pub fn query_layout() -> Option<Vec<LayoutOutput>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(name: &str, width: u32, height: u32) -> CaptureOutput {
        CaptureOutput {
            id: output_id(name),
            name: name.to_string(),
            width,
            height,
            refresh_millihz: 60_000,
            plane_rotated: false,
        }
    }

    /// The box this was written on: a landscape primary below a portrait
    /// monitor turned "right".
    const KSCREEN: &str = r#"{"outputs":[
        {"connected":true,"currentModeId":"1","enabled":true,"id":1,"name":"DP-2",
         "pos":{"x":0,"y":579},"priority":1,"rotation":1,"scale":1,
         "size":{"height":1440,"width":2560},
         "modes":[{"id":"1","refreshRate":144,"size":{"height":1440,"width":2560}}]},
        {"connected":true,"currentModeId":"27","enabled":true,"id":2,"name":"HDMI-A-1",
         "pos":{"x":2560,"y":0},"priority":2,"rotation":8,"scale":1,
         "size":{"height":2560,"width":1440},
         "modes":[{"id":"26","refreshRate":59.95,"size":{"height":1440,"width":2560}},
                  {"id":"27","refreshRate":74.968,"size":{"height":1440,"width":2560}}]},
        {"connected":true,"enabled":false,"id":3,"name":"DP-1",
         "pos":{"x":0,"y":0},"priority":0,"rotation":1,"size":{"height":0,"width":0}}
    ]}"#;

    fn directory() -> OutputDirectory {
        let directory = OutputDirectory::default();
        let layout = parse_kscreen(KSCREEN).unwrap();
        directory.update(
            &[capture("HDMI-A-1", 2560, 1440), capture("DP-2", 2560, 1440)],
            &[],
            Some(&layout),
        );
        directory
    }

    #[test]
    fn kscreen_layout_parses_position_rotation_and_primary() {
        let layout = parse_kscreen(KSCREEN).unwrap();
        assert_eq!(layout.len(), 2);
        assert_eq!(
            layout[1],
            LayoutOutput {
                name: "HDMI-A-1".into(),
                x: 2560,
                y: 0,
                width: 1440,
                height: 2560,
                rotation: 3,
                primary: false,
                refresh_millihz: 74_968,
            }
        );
        assert!(layout[0].primary);
        assert!(parse_kscreen("{}").is_none());
        assert!(parse_kscreen("not json").is_none());
    }

    #[test]
    fn directory_reports_upright_sizes_and_resolves_requests() {
        let directory = directory();
        let (outputs, _) = directory.list();
        assert_eq!(outputs[0].name, "DP-2");
        assert!(outputs[0].is_primary && !outputs[1].is_primary);
        assert_eq!((outputs[1].width, outputs[1].height), (1440, 2560));
        assert_eq!(outputs[1].rotation, 3);
        assert_eq!((outputs[1].x, outputs[1].y), (2560, 0));

        let primary = output_id("DP-2");
        let second = output_id("HDMI-A-1");
        assert_eq!(directory.resolve(second), second);
        assert_eq!(directory.resolve(OUTPUT_DEFAULT), primary);
        assert_eq!(directory.resolve(0x1234), primary);
        assert_eq!(OutputDirectory::default().resolve(second), SINGLE_OUTPUT);
    }

    #[test]
    fn a_plane_rotated_scanout_is_already_upright() {
        let directory = OutputDirectory::default();
        let layout = parse_kscreen(KSCREEN).unwrap();
        let mut rotated = capture("HDMI-A-1", 2560, 1440);
        rotated.plane_rotated = true;
        directory.update(&[rotated], &[], Some(&layout));
        let info = directory.info(output_id("HDMI-A-1")).unwrap();
        assert_eq!(info.rotation, 0);
        assert_eq!((info.width, info.height), (1440, 2560));
    }

    #[test]
    fn absolute_positions_land_on_the_named_output() {
        let directory = directory();
        // Desktop is 4000x2560; libinput turns v into v * size / 65536.
        let pixel = |v: u16, size: u32| (v as u64 * size as u64 / 65536) as i32;
        let primary = output_id("DP-2");
        let second = output_id("HDMI-A-1");

        let (x, y) = directory.map_absolute(primary, 0, 0).unwrap();
        assert_eq!((pixel(x, 4000), pixel(y, 2560)), (0, 579));
        let (x, y) = directory.map_absolute(primary, 65535, 65535).unwrap();
        assert_eq!((pixel(x, 4000), pixel(y, 2560)), (2559, 2018));
        let (x, y) = directory.map_absolute(second, 0, 65535).unwrap();
        assert_eq!((pixel(x, 4000), pixel(y, 2560)), (2560, 2559));
        let (x, y) = directory.map_absolute(second, 32768, 32768).unwrap();
        assert_eq!((pixel(x, 4000), pixel(y, 2560)), (3280, 1280));

        assert!(directory.map_absolute(0x1234, 0, 0).is_none());
    }

    #[test]
    fn without_a_layout_the_first_output_is_primary_and_input_passes_through() {
        let directory = OutputDirectory::default();
        directory.update(
            &[capture("DP-2", 2560, 1440), capture("HDMI-A-1", 2560, 1440)],
            &[],
            None,
        );
        let (outputs, _) = directory.list();
        assert!(outputs[0].is_primary && !outputs[1].is_primary);
        assert_eq!(outputs[1].rotation, 0);
        assert!(directory.map_absolute(outputs[0].id, 100, 100).is_none());
    }

    #[test]
    fn generation_moves_only_when_the_list_changes() {
        let directory = directory();
        let before = directory.generation();
        let layout = parse_kscreen(KSCREEN).unwrap();
        let outputs = [capture("HDMI-A-1", 2560, 1440), capture("DP-2", 2560, 1440)];
        directory.update(&outputs, &[], Some(&layout));
        assert_eq!(directory.generation(), before);
        directory.update(&outputs[..1], &[], Some(&layout));
        assert_ne!(directory.generation(), before);
    }

    /// Runs once per absolute mouse packet.
    /// `cargo test --release map_absolute_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn map_absolute_cost() {
        let directory = directory();
        let id = output_id("HDMI-A-1");
        const CALLS: u32 = 2_000_000;
        let started = Instant::now();
        let mut sum = 0u32;
        for i in 0..CALLS {
            let (x, y) = directory
                .map_absolute(id, i as u16, (i >> 5) as u16)
                .unwrap();
            sum = sum.wrapping_add(x as u32 + y as u32);
        }
        eprintln!(
            "map_absolute: {:?}/call (checksum {sum})",
            started.elapsed() / CALLS
        );
    }

    #[test]
    fn output_ids_never_collide_with_the_reserved_values() {
        assert_ne!(output_id(""), OUTPUT_DEFAULT);
        assert_ne!(output_id("DP-2"), output_id("HDMI-A-1"));
        assert_ne!(output_id("DP-2"), OUTPUT_VIRTUAL);
    }
}
