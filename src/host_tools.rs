//! Commands for tools of the host system (kscreen-doctor, kdialog, pactl, …).
//!
//! The release launcher puts the bundled libraries first in
//! `LD_LIBRARY_PATH`. A host tool that inherits it loads them instead of the
//! system's: kscreen-doctor died on the bundle's older libstdc++, and the
//! installed service lost the whole monitor arrangement (wrong default
//! output, no rotation, absolute pointer spread over the desktop) while every
//! development run, which has no bundle, worked.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

/// `Command::new` for anything that is not this binary.
pub fn host_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    #[cfg(target_os = "linux")]
    {
        static HOST_PATH: std::sync::OnceLock<LibraryPath> = std::sync::OnceLock::new();
        HOST_PATH
            .get_or_init(|| {
                let bundle = std::env::current_exe()
                    .ok()
                    .and_then(|exe| Some(exe.parent()?.join("lib")));
                match (std::env::var_os("LD_LIBRARY_PATH"), bundle) {
                    (Some(inherited), Some(bundle)) => LibraryPath::without(&inherited, &bundle),
                    _ => LibraryPath::Inherit,
                }
            })
            .apply(&mut command);
    }
    command
}

/// What a host tool gets for `LD_LIBRARY_PATH`.
#[derive(Debug, PartialEq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum LibraryPath {
    Inherit,
    Unset,
    Set(OsString),
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl LibraryPath {
    /// `inherited` minus the bundle directory, however it is spelled (the
    /// launcher does not resolve symlinks, `current_exe` does).
    fn without(inherited: &OsStr, bundle: &Path) -> Self {
        let resolved_bundle = bundle.canonicalize().ok();
        let is_bundle = |entry: &Path| {
            entry == bundle
                || resolved_bundle
                    .as_deref()
                    .is_some_and(|bundle| entry.canonicalize().is_ok_and(|entry| entry == bundle))
        };
        let kept: Vec<_> = std::env::split_paths(inherited)
            .filter(|entry| !is_bundle(entry) && !entry.as_os_str().is_empty())
            .collect();
        match std::env::join_paths(kept) {
            Ok(path) if !path.is_empty() => Self::Set(path),
            _ => Self::Unset,
        }
    }

    fn apply(&self, command: &mut Command) {
        match self {
            Self::Inherit => {}
            Self::Unset => {
                command.env_remove("LD_LIBRARY_PATH");
            }
            Self::Set(path) => {
                command.env("LD_LIBRARY_PATH", path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_is_taken_out_of_the_library_path() {
        let bundle = Path::new("/opt/st-server/lib");
        let without = |path: &str| LibraryPath::without(OsStr::new(path), bundle);
        assert_eq!(without("/opt/st-server/lib"), LibraryPath::Unset);
        assert_eq!(
            without("/opt/st-server/lib:/usr/local/lib"),
            LibraryPath::Set("/usr/local/lib".into())
        );
        assert_eq!(
            without("/a:/opt/st-server/lib:/b"),
            LibraryPath::Set("/a:/b".into())
        );
    }

    /// The failure was only visible in an installed release: a C++ host tool
    /// must start although the inherited library path holds a `libstdc++` it
    /// cannot use.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_host_tool_starts_under_a_poisoned_library_path() {
        let bundle = std::env::temp_dir().join(format!("st-host-tools-{}", std::process::id()));
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("libstdc++.so.6"), b"not a library").unwrap();
        let run = |scrub: bool| {
            let tool = ["kscreen-doctor", "c++filt", "cmake"]
                .into_iter()
                .find_map(|tool| {
                    let mut command = Command::new(tool);
                    command
                        .arg("--version")
                        .env("LD_LIBRARY_PATH", &bundle)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    if scrub {
                        LibraryPath::without(bundle.as_os_str(), &bundle).apply(&mut command);
                    }
                    command.status().ok()
                });
            // 127 is the dynamic loader giving up before the program runs.
            tool.map(|status| status.code() != Some(127))
        };
        let (inherited, scrubbed) = (run(false), run(true));
        let _ = std::fs::remove_dir_all(&bundle);
        match (inherited, scrubbed) {
            (None, _) | (_, None) => eprintln!("no C++ host tool installed; skipping"),
            (Some(true), _) => eprintln!("the host tool does not load libstdc++; skipping"),
            (Some(false), Some(loaded)) => {
                assert!(loaded, "the tool still cannot load without the bundle")
            }
        }
    }
}
