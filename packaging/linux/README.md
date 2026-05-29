# st-server Linux packaging (user-session mode)

These files install st-server as a **systemd user service** that starts on
desktop login. The server runs inside the user's own session, so PipeWire,
PulseAudio, xdg-desktop-portal, and the compositor are all reachable
natively — no cross-user permission bridging.

Pre-login streaming (at SDDM/GDM) is intentionally out of scope: it's a
different feature and a different architecture. Start a session first.

## One-liner install (recommended)

Downloads the latest GitHub release and wires it up:

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash
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
