# st-server Linux packaging

Two install modes:

- **System-wide (default)** — a **root system service** that starts at the login
  screen (SDDM/GDM) and follows whichever user logs in. This is what a no-arg
  install gives you. See [System-wide mode](#system-wide-mode) below.
- **Per-user (`--user`)** — a **systemd user service** that starts on desktop
  login and runs inside the user's own session. PipeWire, PulseAudio,
  xdg-desktop-portal, and the compositor are all reachable natively. No root
  service, smallest blast radius.

## One-liner install (recommended)

Downloads the latest GitHub release and wires it up **system-wide** (root service
at the login screen — see the security note in [System-wide mode](#system-wide-mode)):

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash
```

Per-user instead (no root service):

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash -s -- --user
```

Do **not** run this as root. The script calls `sudo` only for the root
steps: the `/dev/uinput` udev rule, granting `cap_sys_admin` to the binary
(so Wayland KMS capture works **without the screen-share dialog**), and a
small path-unit that re-applies that capability after self-updates.

Useful env overrides:

- `ST_SERVER_VERSION=v0.4.6` — pin a specific release.
- `ST_SERVER_PREFIX=~/.local/share/st-server` — change the install prefix.
- `ST_SERVER_NO_ENABLE=1` — install but do not `systemctl --user enable --now`.

## Manual install (from a local build)

```sh
# 1. binary (user-local, no root)
install -Dm0755 target/release/st-server ~/.local/share/st-server/st-server
ln -sf ~/.local/share/st-server/st-server ~/.local/bin/st-server

# 2. udev rule for /dev/uinput (needs root; one-time)
sudo install -Dm0644 packaging/linux/99-st-server.rules \
    /etc/udev/rules.d/99-st-server.rules
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=input
sudo usermod -aG input "$USER"   # log out/in for this to take effect

# 3. cap_sys_admin for dialog-free Wayland KMS capture (needs root).
#    Without this the server falls back to the PipeWire portal (with its dialog).
BIN=~/.local/share/st-server/st-server
sudo setcap cap_sys_admin+ep "$BIN"
#    Self-update replaces the binary and drops the cap, so re-apply it on
#    change with a root path-unit (one-time install):
sudo tee /etc/systemd/system/st-server-setcap-$USER.service >/dev/null <<EOF
[Unit]
Description=Re-apply cap_sys_admin to st-server after updates ($USER)
[Service]
Type=oneshot
ExecStart=$(command -v setcap) cap_sys_admin+ep $BIN
EOF
sudo tee /etc/systemd/system/st-server-setcap-$USER.path >/dev/null <<EOF
[Unit]
Description=Watch st-server and re-apply cap_sys_admin on change ($USER)
[Path]
PathChanged=$BIN
Unit=st-server-setcap-$USER.service
[Install]
WantedBy=paths.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now st-server-setcap-$USER.path

# 4. systemd user unit
install -Dm0644 packaging/linux/st-server.service \
    ~/.config/systemd/user/st-server.service
# (edit ExecStart to point at your binary path if it's not ~/.local/bin/st-server)
systemctl --user daemon-reload
systemctl --user enable --now st-server

# 5. (optional) desktop entry
sed "s|@BIN@|$HOME/.local/bin/st-server|" packaging/linux/st-server.desktop \
  > ~/.local/share/applications/st-server.desktop
```

## System-wide mode

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash -s -- --system
```

Installs a **root** service that starts at the login screen and follows the
active seat:

- **Video** — KMS captures the active scanout (the greeter first, then any user
  who logs in). No portal dialog, native multi-monitor.
- **Input** — uinput injects at the kernel level (session-independent).
- **Audio** — a logind watcher repoints `PULSE_SERVER`/`XDG_RUNTIME_DIR` at the
  active user and re-attaches the pipeline on every user switch
  (`ST_AUDIO_FOLLOW=0` to disable). No audio at the greeter (none exists there).
- **Tray** — a per-user agent (`st-server --tray`, installed as a global user
  unit) shows the full tray menu in each user's session and drives the service
  over a control socket at `/run/st-server/control.sock` (group `st-server`).

What it lays down: binary in `/opt/st-server`, `/usr/local/bin/st-server`
symlink, `st-server.service` (root, `--system`, `WantedBy=graphical.target`),
`/etc/systemd/user/st-server-tray.service` (enabled `--global`),
`/etc/tmpfiles.d/st-server.conf` (the `2750 root:st-server` socket dir), the
`st-server` group (your user is added to it — log out/in to apply), and a
uinput modules-load drop-in. State lives in `/var/lib/st-server` (root-owned).

> **Security:** this is a real privilege escalation over per-user mode. Anyone
> holding the token gets **root-level remote control of this machine from the
> login screen onward**, before any human logs in. Only enable it where that is
> the intended capability, and treat the token accordingly.

Manage it with the normal system units:

```sh
systemctl status st-server
journalctl -u st-server -f
sudo cat /var/lib/st-server/st-server-config.json   # the token
```

Uninstall: `... install.sh | bash -s -- --system --uninstall` (state preserved).

## First connect

The server generates a trust token on first start and stores it at
`~/.local/state/st/st-server-config.json` (mode 0600). Read it with:

```sh
cat ~/.local/state/st/st-server-config.json
```

Or click the tray icon on this machine and pick "Copy Token".

Pre-seed a token by adding `Environment=ST_TOKEN=<your-token>` via
`systemctl --user edit st-server`.

## Troubleshooting

- `journalctl --user -u st-server -f` — live logs.
- Input injection needs group `input`. The installer adds you; log out and
  back in for group membership to take effect.
- KMS direct capture (no screen-share dialog, multi-monitor selection) needs
  `cap_sys_admin` on the binary — the installer grants it and re-applies it
  after self-updates via the `st-server-setcap-<user>.path` unit. If the cap
  is missing (libcap absent, or the just-relaunched post-update process beat
  the re-apply), the picker falls through to the PipeWire portal, which is the
  safe Wayland fallback. Force a backend with `ST_CAPTURE=kms|pipewire`.
  Check it with: `getcap ~/.local/share/st-server/st-server`.
- If the service fails to start because `graphical-session.target` isn't
  active (remote SSH, no desktop), it's by design — this unit is bound to
  the desktop session.
