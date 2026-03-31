use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use tar::Archive;
use tempfile::TempDir;
use zip::ZipArchive;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/pulstart/server/releases/latest";
const APPLY_UPDATE_FLAG: &str = "--apply-update";
const RELAUNCH_FLAG: &str = "--relaunch-after-exit";
const ELEVATED_COPY_FLAG: &str = "--elevated-copy";
const PACKAGE_PREFIX: &str = "st-server-v";

#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
}

#[derive(Clone, Debug)]
pub enum CheckOutcome {
    UpToDate { latest_version: String },
    UpdateAvailable(ReleaseInfo),
}

#[derive(Debug)]
struct ApplyUpdateCommand {
    parent_pid: u32,
    staging_root: PathBuf,
    package_root: PathBuf,
    install_root: PathBuf,
    relaunch_executable: PathBuf,
}

#[derive(Debug)]
struct InstallTarget {
    install_root: PathBuf,
    relaunch_executable: PathBuf,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

enum ArchiveKind {
    Zip,
    TarGz,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn supported_target_label() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64"),
        ("macos", "x86_64") => Ok("macos-x64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("windows", "x86_64") => Ok("windows-x64"),
        (os, arch) => Err(format!("Updater is not supported on {os}/{arch}.")),
    }
}

pub fn maybe_run_apply_update_from_args() -> Result<bool, String> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let Some(flag) = args.next() else {
        return Ok(false);
    };
    if flag == ELEVATED_COPY_FLAG {
        let args: Vec<OsString> = args.collect();
        if args.len() != 2 {
            return Err("Elevated copy received an invalid argument set.".to_string());
        }
        sync_package_contents(&PathBuf::from(&args[0]), &PathBuf::from(&args[1]))?;
        return Ok(true);
    }
    if flag == RELAUNCH_FLAG {
        let args: Vec<OsString> = args.collect();
        if args.len() != 3 {
            return Err("Relaunch helper received an invalid argument set.".to_string());
        }
        let parent_pid = args[0]
            .to_string_lossy()
            .parse::<u32>()
            .map_err(|e| format!("Invalid parent pid: {e}"))?;
        let staging_root = PathBuf::from(&args[1]);
        let relaunch_executable = PathBuf::from(&args[2]);
        wait_for_process_exit(parent_pid)?;
        relaunch_updated_app(&relaunch_executable)?;
        cleanup_staging_root(&staging_root);
        return Ok(true);
    }
    if flag != APPLY_UPDATE_FLAG {
        return Ok(false);
    }
    let command = parse_apply_update_command(args.collect())?;
    run_apply_update_command(&command)?;
    Ok(true)
}

pub fn check_latest_release() -> Result<CheckOutcome, String> {
    let asset_suffix = asset_suffix()?;
    let body = http_get_text(GITHUB_RELEASES_API, true)?;
    let release: GitHubRelease = serde_json::from_str(&body)
        .map_err(|err| format!("Invalid GitHub release response: {err}"))?;

    let latest_version = normalize_version(&release.tag_name)?;
    let latest = Version::parse(&latest_version)
        .map_err(|err| format!("Invalid release version '{latest_version}': {err}"))?;
    let current = Version::parse(current_version())
        .map_err(|err| format!("Invalid current version '{}': {err}", current_version()))?;

    if latest <= current {
        return Ok(CheckOutcome::UpToDate { latest_version });
    }

    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name.ends_with(asset_suffix))
        .ok_or_else(|| {
            format!(
                "Latest release does not contain a {} asset.",
                supported_target_label().unwrap_or("supported")
            )
        })?;

    Ok(CheckOutcome::UpdateAvailable(ReleaseInfo {
        version: latest_version,
        asset_name: asset.name,
        download_url: asset.browser_download_url,
    }))
}

pub fn prepare_and_spawn_update(release: &ReleaseInfo) -> Result<(), String> {
    let temp_dir = TempDir::new().map_err(|err| format!("Failed to create temp dir: {err}"))?;
    let archive_path = temp_dir.path().join(&release.asset_name);
    download_to_path(&release.download_url, &archive_path)?;

    extract_archive(&archive_path, temp_dir.path())?;
    let package_root = locate_package_root(temp_dir.path())?;
    let package_root_relative = package_root
        .strip_prefix(temp_dir.path())
        .map_err(|err| format!("Extracted package root is outside the staging dir: {err}"))?
        .to_path_buf();
    let staging_root = temp_dir.keep();
    let package_root = staging_root.join(package_root_relative);
    let install_target = current_install_target()?;

    if cfg!(target_os = "windows") {
        // On Windows the running executable is locked by the OS and cannot be
        // overwritten in place.  Copy the current binary into the staging
        // directory and spawn it from there with --apply-update so it can
        // replace the install contents *after* this process has fully exited.
        let helper_exe = staging_root.join("_st-update-helper.exe");
        fs::copy(&install_target.relaunch_executable, &helper_exe).map_err(|err| {
            format!("Failed to prepare update helper in staging directory: {err}")
        })?;
        let exec_dir = helper_exe
            .parent()
            .ok_or_else(|| "Staging helper does not have a parent directory.".to_string())?;
        Command::new(&helper_exe)
            .arg(APPLY_UPDATE_FLAG)
            .arg(std::process::id().to_string())
            .arg(&staging_root)
            .arg(&package_root)
            .arg(&install_target.install_root)
            .arg(&install_target.relaunch_executable)
            .current_dir(exec_dir)
            .spawn()
            .map_err(|err| format!("Failed to launch update helper: {err}"))?;
    } else {
        // On Unix we can overwrite running executables directly.  Copy files
        // BEFORE shutting down so the user can see the pkexec dialog through
        // the active stream if elevated permissions are needed.
        if let Err(direct_err) =
            sync_package_contents(&package_root, &install_target.install_root)
        {
            eprintln!("[updater] {direct_err}");
            eprintln!("[updater] Requesting elevated permissions...");
            run_elevated_copy(&package_root, &install_target.install_root)?;
        }

        // Spawn a lightweight helper that just waits for this process to exit
        // and then relaunches the updated binary.  No copy needed at this point.
        spawn_relaunch_helper(
            &install_target.relaunch_executable,
            std::process::id(),
            &staging_root,
            &install_target.relaunch_executable,
        )?;
    }
    Ok(())
}

fn parse_apply_update_command(args: Vec<OsString>) -> Result<ApplyUpdateCommand, String> {
    if args.len() != 5 {
        return Err("Updater helper received an invalid argument set.".to_string());
    }

    let parent_pid = args[0]
        .to_string_lossy()
        .parse::<u32>()
        .map_err(|err| format!("Invalid parent pid for updater helper: {err}"))?;

    Ok(ApplyUpdateCommand {
        parent_pid,
        staging_root: PathBuf::from(&args[1]),
        package_root: PathBuf::from(&args[2]),
        install_root: PathBuf::from(&args[3]),
        relaunch_executable: PathBuf::from(&args[4]),
    })
}

fn run_apply_update_command(command: &ApplyUpdateCommand) -> Result<(), String> {
    wait_for_process_exit(command.parent_pid)?;
    if let Err(direct_err) = sync_package_contents(&command.package_root, &command.install_root) {
        eprintln!("[updater] {direct_err}");
        eprintln!("[updater] Requesting elevated permissions...");
        run_elevated_copy(&command.package_root, &command.install_root)?;
    }
    if let Some(parent) = command.install_root.parent() {
        let _ = std::env::set_current_dir(parent);
    } else {
        let _ = std::env::set_current_dir(&command.install_root);
    }
    relaunch_updated_app(&command.relaunch_executable)?;
    cleanup_staging_root(&command.staging_root);
    Ok(())
}

fn normalize_version(tag: &str) -> Result<String, String> {
    let normalized = tag.trim().trim_start_matches('v');
    if normalized.is_empty() {
        return Err("Release tag does not contain a version.".to_string());
    }
    Ok(normalized.to_string())
}

fn asset_suffix() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("-linux-x64.tar.gz"),
        ("macos", "x86_64") => Ok("-macos-x64.zip"),
        ("macos", "aarch64") => Ok("-macos-arm64.zip"),
        ("windows", "x86_64") => Ok("-windows-x64.zip"),
        _ => Err(supported_target_label().unwrap_err()),
    }
}

fn archive_kind() -> Result<ArchiveKind, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok(ArchiveKind::TarGz),
        ("macos", "x86_64") | ("macos", "aarch64") | ("windows", "x86_64") => Ok(ArchiveKind::Zip),
        _ => Err(supported_target_label().unwrap_err()),
    }
}

fn current_install_target() -> Result<InstallTarget, String> {
    let current_executable =
        std::env::current_exe().map_err(|err| format!("Failed to locate current executable: {err}"))?;

    if cfg!(target_os = "macos") {
        let app_root = macos_app_bundle_root(&current_executable).ok_or_else(|| {
            "Full package update on macOS requires the server to run from a .app bundle.".to_string()
        })?;
        return Ok(InstallTarget {
            relaunch_executable: app_root.join("Contents/MacOS/st-server"),
            install_root: app_root,
        });
    }

    let install_root = current_executable
        .parent()
        .ok_or_else(|| "Current executable does not have a parent directory.".to_string())?
        .to_path_buf();
    Ok(InstallTarget {
        install_root,
        relaunch_executable: current_executable,
    })
}

fn macos_app_bundle_root(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("app"))
            .unwrap_or(false)
        {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

fn spawn_relaunch_helper(
    executable: &Path,
    parent_pid: u32,
    staging_root: &Path,
    relaunch_executable: &Path,
) -> Result<(), String> {
    let exec_dir = executable
        .parent()
        .ok_or_else(|| "Relaunch executable does not have a parent directory.".to_string())?;

    Command::new(executable)
        .arg(RELAUNCH_FLAG)
        .arg(parent_pid.to_string())
        .arg(staging_root)
        .arg(relaunch_executable)
        .current_dir(exec_dir)
        .spawn()
        .map_err(|err| format!("Failed to launch relaunch helper: {err}"))?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_elevated_copy(package_root: &Path, install_root: &Path) -> Result<(), String> {
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("Failed to locate current executable: {e}"))?;
    let elevated_helper = linux_elevated_helper_path(&current_exe);
    let status = Command::new("pkexec")
        .arg(&elevated_helper)
        .arg(ELEVATED_COPY_FLAG)
        .arg(package_root)
        .arg(install_root)
        .status()
        .map_err(|e| format!("Failed to request elevated permissions: {e}"))?;
    if !status.success() {
        return Err("Elevated update was cancelled or failed.".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_elevated_helper_path(current_exe: &Path) -> PathBuf {
    let launcher = current_exe.with_file_name("st-server");
    if launcher.is_file() {
        launcher
    } else {
        current_exe.to_path_buf()
    }
}

#[cfg(target_os = "macos")]
fn run_elevated_copy(package_root: &Path, install_root: &Path) -> Result<(), String> {
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("Failed to locate current executable: {e}"))?;
    let shell_cmd = format!(
        "{} {} {} {}",
        shell_escape(&current_exe.to_string_lossy()),
        shell_escape(ELEVATED_COPY_FLAG),
        shell_escape(&package_root.to_string_lossy()),
        shell_escape(&install_root.to_string_lossy()),
    );
    let escaped = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("do shell script \"{escaped}\" with administrator privileges");
    let status = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|e| format!("Failed to request elevated permissions: {e}"))?;
    if !status.success() {
        return Err("Elevated update was cancelled or failed.".to_string());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_elevated_copy(package_root: &Path, install_root: &Path) -> Result<(), String> {
    let current_exe = std::env::current_exe()
        .map_err(|e| format!("Failed to locate current executable: {e}"))?;
    let script = format!(
        "Start-Process -FilePath '{}' -Verb RunAs -Wait -ArgumentList @('{}','{}','{}')",
        powershell_escape(&current_exe.to_string_lossy()),
        powershell_escape(ELEVATED_COPY_FLAG),
        powershell_escape(&package_root.to_string_lossy()),
        powershell_escape(&install_root.to_string_lossy()),
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map_err(|e| format!("Failed to request elevated permissions: {e}"))?;
    if !status.success() {
        return Err("Elevated update was cancelled or failed.".to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(target_os = "windows")]
fn powershell_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn wait_for_process_exit(pid: u32) -> Result<(), String> {
    const MAX_WAIT_STEPS: usize = 600;
    for _ in 0..MAX_WAIT_STEPS {
        if !process_exists(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("Timed out waiting for the running server process to exit.".to_string())
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(target_os = "windows")]
fn process_exists(pid: u32) -> bool {
    unsafe {
        let handle = match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        ) {
            Ok(handle) => handle,
            Err(_) => return false,
        };
        let wait = WaitForSingleObject(handle, 0);
        let _ = CloseHandle(handle);
        wait == WAIT_TIMEOUT
    }
}

fn sync_package_contents(source_root: &Path, install_root: &Path) -> Result<(), String> {
    fs::create_dir_all(install_root)
        .map_err(|err| format!("Failed to create install root '{}': {err}", install_root.display()))?;

    let entries = fs::read_dir(source_root)
        .map_err(|err| format!("Failed to read extracted package '{}': {err}", source_root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read extracted package entry: {err}"))?;
        let source_path = entry.path();
        let destination_path = install_root.join(entry.file_name());
        sync_path(&source_path, &destination_path)?;
    }
    Ok(())
}

fn sync_path(source: &Path, destination: &Path) -> Result<(), String> {
    if source.is_dir() {
        if destination.exists() && destination.is_file() {
            fs::remove_file(destination).map_err(|err| {
                format!(
                    "Failed to remove file blocking directory update '{}': {err}",
                    destination.display()
                )
            })?;
        }
        fs::create_dir_all(destination)
            .map_err(|err| format!("Failed to create directory '{}': {err}", destination.display()))?;
        let entries = fs::read_dir(source)
            .map_err(|err| format!("Failed to read directory '{}': {err}", source.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
            sync_path(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("Failed to create directory '{}': {err}", parent.display()))?;
        }
        if destination.exists() {
            if destination.is_dir() {
                fs::remove_dir_all(destination).map_err(|err| {
                    format!(
                        "Failed to remove directory blocking file update '{}': {err}",
                        destination.display()
                    )
                })?;
            } else {
                fs::remove_file(destination).map_err(|err| {
                    format!(
                        "Failed to remove existing file '{}': {err}",
                        destination.display()
                    )
                })?;
            }
        }
        fs::copy(source, destination).map_err(|err| {
            format!(
                "Failed to copy '{}' to '{}': {err}",
                source.display(),
                destination.display()
            )
        })?;
        copy_permissions(source, destination)?;
    }
    Ok(())
}

fn copy_permissions(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::metadata(source)
        .map_err(|err| format!("Failed to read source permissions '{}': {err}", source.display()))?;
    fs::set_permissions(destination, metadata.permissions()).map_err(|err| {
        format!(
            "Failed to set permissions on '{}': {err}",
            destination.display()
        )
    })?;
    Ok(())
}

fn relaunch_updated_app(path: &Path) -> Result<(), String> {
    let mut command = Command::new(path);
    if let Some(parent) = path.parent() {
        command.current_dir(parent);
    }
    command
        .spawn()
        .map_err(|err| format!("Failed to relaunch updated server '{}': {err}", path.display()))?;
    Ok(())
}

fn cleanup_staging_root(staging_root: &Path) {
    let _ = self_replace::self_delete_outside_path(staging_root);
    let _ = fs::remove_dir_all(staging_root);
}

fn extract_archive(archive_path: &Path, destination_root: &Path) -> Result<(), String> {
    match archive_kind()? {
        ArchiveKind::Zip => extract_zip(archive_path, destination_root),
        ArchiveKind::TarGz => extract_tar_gz(archive_path, destination_root),
    }
}

fn extract_zip(archive_path: &Path, destination_root: &Path) -> Result<(), String> {
    let file =
        File::open(archive_path).map_err(|err| format!("Failed to open downloaded archive: {err}"))?;
    let mut archive =
        ZipArchive::new(file).map_err(|err| format!("Failed to read zip archive: {err}"))?;
    archive
        .extract(destination_root)
        .map_err(|err| format!("Failed to extract zip archive: {err}"))?;
    Ok(())
}

fn extract_tar_gz(archive_path: &Path, destination_root: &Path) -> Result<(), String> {
    let file =
        File::open(archive_path).map_err(|err| format!("Failed to open downloaded archive: {err}"))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(destination_root)
        .map_err(|err| format!("Failed to extract tar archive: {err}"))?;
    Ok(())
}

fn locate_package_root(extract_root: &Path) -> Result<PathBuf, String> {
    if cfg!(target_os = "macos") {
        return locate_macos_package_root(extract_root);
    }

    let entries =
        fs::read_dir(extract_root).map_err(|err| format!("Failed to inspect extracted archive: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read extracted entry: {err}"))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PACKAGE_PREFIX)
        {
            return Ok(entry.path());
        }
    }
    Err("Could not find the extracted package root in the downloaded archive.".to_string())
}

fn locate_macos_package_root(extract_root: &Path) -> Result<PathBuf, String> {
    let entries =
        fs::read_dir(extract_root).map_err(|err| format!("Failed to inspect extracted archive: {err}"))?;

    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read extracted entry: {err}"))?;
        let path = entry.path();
        if entry.file_name() == "st-server.app" && path.is_dir() {
            return Ok(path);
        }
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PACKAGE_PREFIX)
        {
            let bundle_root = path.join("st-server.app");
            if bundle_root.is_dir() {
                return Ok(bundle_root);
            }
        }
    }

    Err("Could not find the extracted macOS app bundle in the downloaded archive.".to_string())
}

fn download_to_path(url: &str, path: &Path) -> Result<(), String> {
    let mut response = http_get_response(url, false)?;
    let mut output =
        File::create(path).map_err(|err| format!("Failed to create download target: {err}"))?;
    io::copy(&mut response, &mut output)
        .map_err(|err| format!("Failed to write downloaded update: {err}"))?;
    Ok(())
}

fn http_get_text(url: &str, json: bool) -> Result<String, String> {
    let response = http_get_response(url, json)?;
    let mut body = String::new();
    let mut reader = response;
    use std::io::Read;
    reader
        .read_to_string(&mut body)
        .map_err(|err| format!("Failed to read HTTP response body: {err}"))?;
    Ok(body)
}

fn http_get_response(url: &str, json: bool) -> Result<Box<dyn io::Read>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let request = agent
        .get(url)
        .set("User-Agent", &format!("st-server/{}", current_version()));
    let request = if json {
        request.set("Accept", "application/vnd.github+json")
    } else {
        request
    };
    let response = request.call().map_err(format_http_error)?;
    Ok(Box::new(response.into_reader()))
}

fn format_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let status_text = response.status_text().to_string();
            format!("HTTP {code} while contacting GitHub: {status_text}")
        }
        ureq::Error::Transport(err) => format!("Network error while contacting GitHub: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_version;

    #[test]
    fn strips_v_prefix() {
        assert_eq!(normalize_version("v1.2.3").unwrap(), "1.2.3");
        assert_eq!(normalize_version("1.2.3").unwrap(), "1.2.3");
    }
}
