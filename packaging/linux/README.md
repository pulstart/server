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
curl -fsSL https://raw.githubusercontent.com/zhey/st/main/packaging/linux/install.sh | bash
```

Do **not** run this as root. The script re-execs itself with `sudo` only
for the single udev-rule step.

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

# 3. systemd user unit
install -Dm0644 packaging/linux/st-server.service \
    ~/.config/systemd/user/st-server.service
# (edit ExecStart to point at your binary path if it's not ~/.local/bin/st-server)
systemctl --user daemon-reload
systemctl --user enable --now st-server

# 4. (optional) desktop entry
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
- KMS capture is unavailable in this mode (the compositor holds DRM master).
  The auto-picker will fall through to PipeWire/portal, which is the
  expected path on Wayland.
- If the service fails to start because `graphical-session.target` isn't
  active (remote SSH, no desktop), it's by design — this unit is bound to
  the desktop session.
