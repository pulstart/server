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
systemctl status st-server        # is it running?
journalctl -u st-server -f        # live logs
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
