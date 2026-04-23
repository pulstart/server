# st-server Linux system-service packaging

These files install st-server as a systemd system service that starts at boot.
The service captures via KMS/DRM and injects input via /dev/uinput, so it
works on the login screen (SDDM/GDM) and transparently keeps working after a
user logs in.

## One-liner install (recommended)

Downloads the latest GitHub release and wires it up:

```sh
curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash
```

Useful env overrides:

- `ST_SERVER_VERSION=v0.4.6` — pin a specific release.
- `ST_SERVER_TOKEN=<hex>` — pre-seed the trust token instead of letting the
  server generate one on first boot.
- `ST_SERVER_PREFIX=/opt/st-server` — change the install prefix.
- `ST_SERVER_NO_ENABLE=1` — install but do not `systemctl enable --now`.

## Manual install (from a local build)

```sh
# 1. binary
sudo install -Dm0755 target/release/st-server /usr/bin/st-server

# 2. systemd-sysusers creates the `st` system user
sudo install -Dm0644 packaging/linux/sysusers.d-st-server.conf \
    /usr/lib/sysusers.d/st-server.conf
sudo systemd-sysusers

# 3. udev rules for DRM + uinput access
sudo install -Dm0644 packaging/linux/99-st-server.rules \
    /etc/udev/rules.d/99-st-server.rules
sudo udevadm control --reload
sudo udevadm trigger

# 4. systemd service
sudo install -Dm0644 packaging/linux/st-server.service \
    /etc/systemd/system/st-server.service
sudo systemctl daemon-reload
sudo systemctl enable --now st-server

# 5. (optional) desktop entry for interactive user-session runs
sudo install -Dm0644 packaging/linux/st-server.desktop \
    /usr/share/applications/st-server.desktop
```

## First connect

The service generates a trust token on first boot and stores it at
`/var/lib/st-server/st-server-config.json` (owner `st:st`, mode 0600).
Read it with:

```sh
sudo cat /var/lib/st-server/st-server-config.json
```

Or pre-seed a token before enabling the service by adding
`Environment=ST_TOKEN=<your-token>` to the unit (via
`systemctl edit st-server`).

## Troubleshooting

- `journalctl -u st-server -f` — live logs.
- KMS capture needs the `st` user in the `video` + `render` groups. The unit
  does this via `SupplementaryGroups=`; no manual step required.
- On NVIDIA proprietary the compositor may hold DRM master exclusively;
  KMS capture is not guaranteed to keep working once the user session starts.
  In that case, switch the unit to `ST_CAPTURE=pipewire` and run as the
  logged-in user instead (a separate user-level service, not covered here).
- The system service cannot use `ST_CAPTURE=pipewire` directly — the portal
  runs inside a user session.
