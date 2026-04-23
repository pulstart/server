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

After install:

```sh
systemctl status st-server        # is it running?
journalctl -u st-server -f        # live logs
sudo cat /var/lib/st-server/st-server-config.json   # first-connect token
```

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
