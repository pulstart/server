use screencapturekit::prelude::*;

const DISPLAY_ID_ENV: &str = "ST_MACOS_DISPLAY_ID";

pub fn select_capture_display() -> Result<SCDisplay, String> {
    let content =
        SCShareableContent::get().map_err(|err| format!("Failed to enumerate displays: {err:?}"))?;
    let displays = content.displays();
    if displays.is_empty() {
        return Err("No displays found".into());
    }

    if let Some(requested_id) = requested_display_id()? {
        if let Some(display) = displays.iter().find(|display| display.display_id() == requested_id) {
            return Ok(display.clone());
        }
        return Err(format!(
            "Display {} requested by {} was not found. Available displays: {}",
            requested_id,
            DISPLAY_ID_ENV,
            available_displays(&displays)
        ));
    }

    displays
        .iter()
        .max_by_key(|display| (u64::from(display.width()) * u64::from(display.height()), display.display_id()))
        .cloned()
        .ok_or_else(|| "No displays found".to_string())
}

pub fn describe_display(display: &SCDisplay) -> String {
    format!(
        "display {} ({}x{})",
        display.display_id(),
        display.width(),
        display.height()
    )
}

fn requested_display_id() -> Result<Option<u32>, String> {
    let Some(raw) = std::env::var_os(DISPLAY_ID_ENV) else {
        return Ok(None);
    };

    let raw = raw.to_string_lossy();
    let display_id = raw
        .trim()
        .parse::<u32>()
        .map_err(|err| format!("{} must be a numeric display id: {err}", DISPLAY_ID_ENV))?;
    Ok(Some(display_id))
}

fn available_displays(displays: &[SCDisplay]) -> String {
    displays
        .iter()
        .map(|display| describe_display(display))
        .collect::<Vec<_>>()
        .join(", ")
}
