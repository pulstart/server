# st-server

Low-latency screen + audio + input streaming server. Built in Rust, pairs
with the [st-client](https://github.com/pulstart/client) viewer.

## Install on Linux (systemd user service)

One-liner — downloads the latest release and installs it as a systemd
**user** service that starts on desktop login. Do **not** run as root:

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash
```

The script re-execs itself with `sudo` only for the single udev-rule step
that needs root (input injection via `/dev/uinput`).

Useful env overrides (set before the pipe):

- `ST_SERVER_VERSION=v0.4.6` — pin a specific release.
- `ST_SERVER_PREFIX=~/.local/share/st-server` — change the install prefix.
- `ST_SERVER_NO_ENABLE=1` — install but do not `systemctl --user enable --now`.

Uninstall:

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash -s -- --uninstall
```

After install:

```sh
systemctl --user status st-server                       # is it running?
journalctl --user -u st-server -f                       # logs
cat ~/.local/state/st/st-server-config.json             # first-connect token
```

### Tray icon

The server runs inside your user session, so the in-process tray icon
(ksni on Linux) is the only tray — click it to copy the token, toggle
accept-new-connections, change codec/bitrate, or shut down.

### Pre-login streaming

Out of scope in this mode. Streaming the SDDM/GDM login screen requires a
separate system-level agent; this installer only covers the user-session
flow, which is what 99% of use cases want.

Full packaging details (manual install, uninstall, troubleshooting) are in
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
