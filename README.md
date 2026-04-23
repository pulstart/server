# st-server

Low-latency screen + audio + input streaming server. Built in Rust, pairs
with the [st-client](https://github.com/pulstart/client) viewer.

## Install on Linux (systemd)

One-liner — downloads the latest release, installs it as a system service
that starts at boot, and works on the login screen (SDDM/GDM) before any
user is logged in:

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash
```

Useful env overrides (set before the pipe):

- `ST_SERVER_VERSION=v0.4.6` — pin a specific release.
- `ST_SERVER_TOKEN=<hex>` — pre-seed the client trust token.
- `ST_SERVER_PREFIX=/opt/st-server` — change the install prefix.
- `ST_SERVER_NO_ENABLE=1` — install but do not `systemctl enable --now`.

Uninstall:

```sh
# Remove the service + binary. Keep the state dir (/var/lib/st-server) so
# the trust token survives a reinstall.
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash -s -- --uninstall

# Full wipe — also deletes /var/lib/st-server, the `st` user, and the
# sysusers entry. Anyone you shared the old token with loses access.
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash -s -- --uninstall --purge
```

After install:

```sh
systemctl status st-server                      # is it running?
journalctl -u st-server -f                      # service logs
journalctl --user -t st-server-tray -f          # tray companion logs
sudo cat /var/lib/st-server/st-server-config.json   # first-connect token
```

### Tray icon

The installer also drops `/etc/xdg/autostart/st-server-tray.desktop`, which
launches a small user-session companion (`st-server --tray`) on desktop
login. It shows service status, a "Copy token" menu entry, and start/stop/
restart actions (via `pkexec systemctl`). The system service itself runs
as the `st` user and has no access to your D-Bus session bus, which is why
the tray lives in a separate user-side process.

The installer adds the sudo user to the `st` group so the tray can read
the token; **log out and back in** (or `newgrp st` in a fresh shell) for
that to take effect.

### Control socket (for scripting / the tray companion)

When running as a system service the server also opens a local Unix socket
at `/run/st-server/control.sock` (mode `0660`, owner `st:st`). Any member
of the `st` group can speak line-delimited JSON to it. Poke at it with
`nc -U`:

```sh
# Read live state (token, codec, bitrate, connected clients, etc.)
printf '{"op":"get_state"}\n' | nc -U /run/st-server/control.sock

# Switch codec
printf '{"op":"set_codec","codec":"hevc"}\n' | nc -U /run/st-server/control.sock

# Pin a bitrate in kbps (0 = auto/ABR)
printf '{"op":"set_bitrate","kbps":25000}\n' | nc -U /run/st-server/control.sock

# Regenerate the trust token — returns the new one
printf '{"op":"regen_token"}\n' | nc -U /run/st-server/control.sock
```

The full op set: `get_state`, `set_codec`, `set_bitrate`, `set_quality`,
`regen_token`, `set_token`, `disconnect_all`, `shutdown`,
`set_session_context`, `clear_session_context`.

### Audio under the system service

The service can't capture your desktop audio directly — PulseAudio /
PipeWire daemons run per-user, and the `st` system user isn't logged in.
Instead the tray companion (which *does* run in your session) probes
`$XDG_RUNTIME_DIR/pulse/native` plus the PulseAudio cookie on startup and
pushes both to the server via `set_session_context`. The server then
captures against that endpoint — no more, no less.

Consequences:

- **No user logged in (SDDM/GDM greeter)**: no tray, no session context,
  no audio — the client sees a silent video stream.
- **User logs in**: tray fires, pushes context, audio capture starts mid-
  stream. Clients already watching suddenly get sound.
- **User logs out**: tray exits on SIGTERM, sends `clear_session_context`;
  audio capture tears down, video keeps streaming silently until someone
  logs back in.
- **Fast user switch**: the new session's tray pushes its own context;
  server rebinds to the new user's audio daemon.

The mechanism (`server/src/session_bridge.rs`) is deliberately generic:
future user-session resources — D-Bus bus address for notifications,
Wayland display for direct-capture variants, XDG paths — can plug into
the same bridge by adding fields to `SessionContext` and subscribing on
the server side.

Full packaging details (manual install, uninstall, NVIDIA caveats) are in
[`packaging/linux/README.md`](packaging/linux/README.md).

## Build from source

```sh
git clone --recurse-submodules https://github.com/pulstart/server.git
cd server
cargo build --release
```

Runtime deps on Linux: PulseAudio, PipeWire, X11/Wayland, FFmpeg (libav*),
GPU/display drivers. Ubuntu 24.04 exact package list is in the CI workflow
at `.github/workflows/release.yml`.

## Platforms

- Linux x64 — published release, systemd installer supported.
- macOS x64 / arm64 — published `.app` zips, run manually.
- Windows x64 — published zip, run manually.

CI/CD details: [`CI.md`](CI.md).
