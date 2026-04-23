//! Tray-side xdg-desktop-portal ScreenCast request.
//!
//! The system service runs as the `st` user and has no session D-Bus to
//! open a portal session on. The tray *does* — so it opens the portal
//! here, gets an authenticated PipeWire fd back, and hands that fd to
//! the service via `SCM_RIGHTS` on the control socket. The service then
//! reads frames from the offered PipeWire node without ever talking to
//! the portal itself.
//!
//! This module is intentionally standalone: it depends only on `zbus` and
//! `tokio`, not on the full server-side PipeWire capture stack, so the
//! tray binary doesn't pull in the entire capture backend.

#![cfg(target_os = "linux")]

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use zvariant::{ObjectPath, OwnedObjectPath, Value};

/// What the portal hands back on a successful ScreenCast `Start`.
pub struct ScreencastOffer {
    pub pw_fd: OwnedFd,
    pub node_id: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    /// The ScreenCast session object path. Callers must keep this
    /// (together with the `zbus::Connection`) alive for as long as they
    /// want the fd to remain valid — dropping the session revokes the
    /// PipeWire stream. [`ScreencastSession`] binds all three lifetimes.
    pub session: ScreencastSession,
}

/// Opaque handle that keeps the portal session alive. Drop closes the
/// session, which tells the portal to tear down the PipeWire stream and
/// revoke the fd.
pub struct ScreencastSession {
    runtime: tokio::runtime::Runtime,
    connection: zbus::Connection,
    session_path: String,
}

impl Drop for ScreencastSession {
    fn drop(&mut self) {
        let result: Result<(), String> = self.runtime.block_on(async {
            let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&self.connection)
                .destination("org.freedesktop.portal.Desktop")
                .map_err(|e| format!("dest: {e}"))?
                .path(self.session_path.as_str())
                .map_err(|e| format!("path: {e}"))?
                .interface("org.freedesktop.portal.Session")
                .map_err(|e| format!("iface: {e}"))?
                .build()
                .await
                .map_err(|e| format!("session proxy: {e}"))?;
            proxy
                .call::<_, _, ()>("Close", &())
                .await
                .map_err(|e| format!("Session.Close: {e}"))?;
            Ok(())
        });
        if let Err(err) = result {
            eprintln!("[tray-portal] failed to close portal session: {err}");
        }
    }
}

fn token_path() -> PathBuf {
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
            PathBuf::from(home).join(".local/state")
        });
    state_dir.join("st").join("portal_token")
}

fn load_restore_token() -> Option<String> {
    std::fs::read_to_string(token_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_restore_token(token: &str) {
    let path = token_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = std::fs::write(&path, token) {
        eprintln!("[tray-portal] save restore token: {err}");
    }
}

/// Run the portal dance. Returns on success with a live fd and the
/// metadata the server needs to connect to the correct stream node.
pub fn request_screencast() -> Result<ScreencastOffer, String> {
    let restore_token = load_restore_token();
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;

    let (conn, session_path, pw_fd, node_id, width, height) = rt.block_on(async {
        use futures_lite::StreamExt;
        use std::collections::HashMap;

        let conn = zbus::Connection::session()
            .await
            .map_err(|e| format!("D-Bus session bus: {e}"))?;

        let unique_name = conn
            .unique_name()
            .ok_or("D-Bus connection has no unique name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");

        let mut token_counter: u32 = 0;
        let mut next_token = || -> String {
            token_counter += 1;
            format!("sttray{token_counter}")
        };

        let portal = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("dest: {e}"))?
            .path("/org/freedesktop/portal/desktop")
            .map_err(|e| format!("path: {e}"))?
            .interface("org.freedesktop.portal.ScreenCast")
            .map_err(|e| format!("iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("portal proxy: {e}"))?;

        // -- CreateSession --
        let session_token = next_token();
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let req_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req dest: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy: {e}"))?;
        let mut response_stream = req_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe CreateSession Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));
        let _: OwnedObjectPath = portal
            .call("CreateSession", &(opts,))
            .await
            .map_err(|e| format!("CreateSession: {e}"))?;
        let signal = response_stream
            .next()
            .await
            .ok_or("CreateSession Response stream ended")?;
        let (code, results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("CreateSession denied (code {code})"));
        }
        drop(response_stream);
        drop(req_proxy);

        let session_path = results
            .get("session_handle")
            .and_then(try_extract_string)
            .unwrap_or_else(|| {
                format!("/org/freedesktop/portal/desktop/session/{unique_name}/{session_token}")
            });
        let session_obj = ObjectPath::try_from(session_path.as_str())
            .map_err(|e| format!("session path: {e}"))?;

        // -- SelectSources --
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let req_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req dest: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy: {e}"))?;
        let mut response_stream = req_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe SelectSources Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        opts.insert("types", Value::U32(1)); // MONITOR
        opts.insert("cursor_mode", Value::U32(4)); // CURSOR_MODE_METADATA
        opts.insert("persist_mode", Value::U32(2)); // PERSIST_UNTIL_REVOKED
        opts.insert("multiple", Value::Bool(false));
        if let Some(ref token) = restore_token {
            opts.insert("restore_token", Value::from(token.as_str()));
        }
        let _: OwnedObjectPath = portal
            .call("SelectSources", &(&session_obj, opts))
            .await
            .map_err(|e| format!("SelectSources: {e}"))?;
        let signal = response_stream
            .next()
            .await
            .ok_or("SelectSources Response stream ended")?;
        let (code, _) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!(
                "SelectSources denied (code {code}). User probably cancelled the dialog."
            ));
        }
        drop(response_stream);
        drop(req_proxy);

        // -- Start --
        let request_token = next_token();
        let request_path =
            format!("/org/freedesktop/portal/desktop/request/{unique_name}/{request_token}");
        let req_proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&conn)
            .destination("org.freedesktop.portal.Desktop")
            .map_err(|e| format!("req dest: {e}"))?
            .path(request_path.as_str())
            .map_err(|e| format!("req path: {e}"))?
            .interface("org.freedesktop.portal.Request")
            .map_err(|e| format!("req iface: {e}"))?
            .build()
            .await
            .map_err(|e| format!("req proxy: {e}"))?;
        let mut response_stream = req_proxy
            .receive_signal("Response")
            .await
            .map_err(|e| format!("subscribe Start Response: {e}"))?;

        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(request_token.as_str()));
        let _: OwnedObjectPath = portal
            .call("Start", &(&session_obj, "", opts))
            .await
            .map_err(|e| format!("Start: {e}"))?;
        let signal = response_stream
            .next()
            .await
            .ok_or("Start Response stream ended")?;
        let (code, start_results) = parse_response(&signal)?;
        if code != 0 {
            return Err(format!("Start denied (code {code})"));
        }
        drop(response_stream);
        drop(req_proxy);

        if let Some(token) = start_results.get("restore_token").and_then(try_extract_string) {
            save_restore_token(&token);
        }

        let (node_id, width, height) = start_results
            .get("streams")
            .ok_or_else(|| "No streams in Start response".to_string())
            .and_then(extract_first_stream)?;

        // -- OpenPipeWireRemote --
        let empty_opts: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = portal
            .call_method("OpenPipeWireRemote", &(&session_obj, empty_opts))
            .await
            .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
        let pw_fd: OwnedFd = reply
            .body()
            .deserialize::<zvariant::OwnedFd>()
            .map_err(|e| format!("OpenPipeWireRemote fd: {e}"))?
            .into();

        Ok::<_, String>((conn, session_path, pw_fd, node_id, width, height))
    })?;

    Ok(ScreencastOffer {
        pw_fd,
        node_id,
        logical_width: width,
        logical_height: height,
        session: ScreencastSession {
            runtime: rt,
            connection: conn,
            session_path,
        },
    })
}

fn parse_response(
    signal: &zbus::message::Message,
) -> Result<(u32, std::collections::HashMap<String, zvariant::OwnedValue>), String> {
    signal
        .body()
        .deserialize()
        .map_err(|e| format!("deserialize Response: {e}"))
}

fn try_extract_string(v: &zvariant::OwnedValue) -> Option<String> {
    if let Ok(s) = <&str>::try_from(v) {
        return Some(s.to_string());
    }
    if let Ok(val) = zvariant::Value::try_from(v) {
        if let zvariant::Value::Str(s) = val {
            return Some(s.to_string());
        }
    }
    None
}

fn extract_first_stream(streams_val: &zvariant::OwnedValue) -> Result<(u32, u32, u32), String> {
    let value = zvariant::Value::try_from(streams_val).map_err(|e| format!("streams: {e}"))?;
    let streams: Vec<(u32, std::collections::HashMap<String, zvariant::OwnedValue>)> =
        value.try_into().map_err(|e| format!("streams: {e}"))?;
    for (node_id, props) in streams {
        let mut w = 0i32;
        let mut h = 0i32;
        if let Some(size) = props.get("size") {
            if let Ok(val) = zvariant::Value::try_from(size) {
                if let Ok((wx, hx)) = <(i32, i32)>::try_from(val) {
                    w = wx;
                    h = hx;
                }
            }
        }
        return Ok((node_id, w.max(1) as u32, h.max(1) as u32));
    }
    Err("empty streams array".into())
}
