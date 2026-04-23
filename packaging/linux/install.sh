#!/usr/bin/env bash
# st-server Linux system-service installer.
#
# Downloads the latest published release from https://github.com/pulstart/server
# and wires it up as a systemd system service that starts at boot (so the
# server is reachable from the SDDM/GDM greeter before anyone logs in) and
# keeps streaming after a user logs into their desktop.
#
# One-liner:
#     curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash
#
# Uninstall:
#     curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash -s -- --uninstall
#     curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | sudo bash -s -- --uninstall --purge
#
# --uninstall removes the service, unit file, udev rule, autostart entry,
# binary symlink, and the install prefix. State (tokens, portal tokens) at
# /var/lib/st-server is kept by default so a reinstall is silent. Add
# --purge to also delete the state dir, sysusers entry, and `st` user/group.
#
# Environment knobs:
#     ST_SERVER_VERSION=v0.4.6     Pin a specific release (default: latest).
#     ST_SERVER_PREFIX=/opt/st-server   Where to unpack the tarball.
#     ST_SERVER_TOKEN=hexstring    Pre-seed the trust token.
#     ST_SERVER_NO_ENABLE=1        Install but do not `systemctl enable --now`.

set -euo pipefail

REPO="pulstart/server"
PREFIX="${ST_SERVER_PREFIX:-/opt/st-server}"
STATE_DIR="/var/lib/st-server"
SERVICE_PATH="/etc/systemd/system/st-server.service"
SYSUSERS_PATH="/usr/lib/sysusers.d/st-server.conf"
UDEV_PATH="/etc/udev/rules.d/99-st-server.rules"
BIN_SYMLINK="/usr/local/bin/st-server"
AUTOSTART_PATH="/etc/xdg/autostart/st-server-tray.desktop"

log()  { printf '\033[1;34m[st-install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[st-install]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[st-install]\033[0m %s\n' "$*" >&2; exit 1; }

require_root() {
    if [[ "$(id -u)" -ne 0 ]]; then
        die "This installer must run as root. Re-run with:
    curl -fsSL https://raw.githubusercontent.com/${REPO}/main/packaging/linux/install.sh | sudo bash"
    fi
}

require_cmds() {
    local missing=()
    for cmd in curl tar systemctl udevadm install chmod chown; do
        command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
    done
    if (( ${#missing[@]} > 0 )); then
        die "Missing required commands: ${missing[*]}"
    fi
    if ! pidof systemd >/dev/null 2>&1; then
        die "systemd is not PID 1 on this host. This installer only supports systemd-based distributions."
    fi
}

detect_platform() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64) echo "linux-x64" ;;
        aarch64|arm64)
            die "Architecture $arch is not published in the current release pipeline (only linux-x64 is built). Build from source."
            ;;
        *) die "Unsupported architecture: $arch" ;;
    esac
}

resolve_version() {
    if [[ -n "${ST_SERVER_VERSION:-}" ]]; then
        local v="${ST_SERVER_VERSION}"
        [[ "$v" == v* ]] || v="v$v"
        echo "$v"
        return
    fi
    local api="https://api.github.com/repos/${REPO}/releases/latest"
    local tag
    tag="$(curl -fsSL "$api" | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
    [[ -n "$tag" ]] || die "Could not resolve latest release from $api"
    echo "$tag"
}

download_and_extract() {
    local version="$1" platform="$2" target_dir="$3"
    local asset="st-server-${version}-${platform}.tar.gz"
    local url="https://github.com/${REPO}/releases/download/${version}/${asset}"
    local tmp
    tmp="$(mktemp -d)"

    log "Downloading $url"
    if ! curl -fsSL "$url" -o "$tmp/$asset"; then
        rm -rf "$tmp"
        die "Failed to download $url"
    fi

    log "Extracting into $target_dir"
    rm -rf "$target_dir"
    mkdir -p "$target_dir"
    tar -xzf "$tmp/$asset" -C "$tmp"
    local extracted="$tmp/st-server-${version}-${platform}"
    if [[ ! -d "$extracted" ]]; then
        rm -rf "$tmp"
        die "Unexpected archive layout: $extracted missing"
    fi
    # Flatten: move contents of the versioned dir into $target_dir.
    ( shopt -s dotglob; mv "$extracted"/* "$target_dir"/ )
    rm -rf "$tmp"

    [[ -x "$target_dir/st-server" ]] || die "Launcher $target_dir/st-server is missing or not executable"
}

write_sysusers() {
    log "Writing $SYSUSERS_PATH"
    install -Dm0644 /dev/stdin "$SYSUSERS_PATH" <<EOF
# Created by packaging/linux/install.sh.
# Creates the unprivileged system user that runs st-server.service.
u st - "st-server system user" ${STATE_DIR}
EOF
    systemd-sysusers
}

write_udev() {
    log "Writing $UDEV_PATH"
    # Intentionally minimal: only /dev/uinput needs a custom rule because
    # no distro ships one. Writing MODE/GROUP on DRM or evdev nodes here
    # would reset udev permissions and wipe logind's per-session uaccess
    # ACL — the logged-in user would lose GPU access until re-login, and
    # Mesa would silently fall back to llvmpipe. The `st` service user
    # already gets DRM + evdev access via SupplementaryGroups on the unit.
    install -Dm0644 /dev/stdin "$UDEV_PATH" <<'EOF'
# Created by packaging/linux/install.sh.
# Grants /dev/uinput the group access needed for input injection.
# DRM and evdev nodes are deliberately left to the distro defaults so
# logind's uaccess ACL for the logged-in user isn't reset.

KERNEL=="uinput", MODE="0660", GROUP="input", TAG+="uaccess"
EOF
    udevadm control --reload
    udevadm trigger --subsystem-match=drm --subsystem-match=input || true
    # uinput is a module; make sure the kernel module is loaded so /dev/uinput
    # exists at service start even on systems that otherwise auto-load on use.
    modprobe uinput 2>/dev/null || true
    echo "uinput" > /etc/modules-load.d/st-server.conf
}

write_service() {
    local launcher="$PREFIX/st-server"
    log "Writing $SERVICE_PATH (ExecStart=$launcher)"
    local extra_env=""
    if [[ -n "${ST_SERVER_TOKEN:-}" ]]; then
        extra_env=$'Environment=ST_TOKEN='"${ST_SERVER_TOKEN}"
    fi
    install -Dm0644 /dev/stdin "$SERVICE_PATH" <<EOF
[Unit]
Description=st low-latency game-streaming server (system instance)
Documentation=https://github.com/${REPO}
After=network-online.target systemd-user-sessions.service
Wants=network-online.target

[Service]
Type=simple
User=st
Group=st
SupplementaryGroups=video render input
ExecStart=${launcher}
Restart=on-failure
RestartSec=2

Environment=ST_STATE_DIR=${STATE_DIR}
# KMS capture works before anyone logs in (no portal consent dialog needed)
# and transparently follows the handover from the greeter into a user session.
Environment=ST_CAPTURE=kms
${extra_env}

StateDirectory=st-server
StateDirectoryMode=0750
RuntimeDirectory=st-server
RuntimeDirectoryMode=0750

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=${STATE_DIR}
# Device access is gated by Unix group membership, not a cgroup whitelist.
# SupplementaryGroups=video render input + the udev rules we ship cover
# Intel (i915), AMD (amdgpu), and NVIDIA (char-major 195) transparently.
# Do NOT re-introduce DeviceAllow= here; it flips the cgroup device
# controller to whitelist mode and silently breaks at least one vendor.
RestrictSUIDSGID=true
LockPersonality=true

[Install]
WantedBy=multi-user.target
EOF
}

write_bin_symlink() {
    log "Linking $BIN_SYMLINK -> $PREFIX/st-server"
    ln -sf "$PREFIX/st-server" "$BIN_SYMLINK"
}

ensure_state_dir() {
    install -d -o st -g st -m 0750 "$STATE_DIR"
}

# On first install, carry the invoking user's existing user-mode config
# (token, codec/bitrate/quality, peer id) into the system-service state dir
# so users don't have to re-pair clients or re-configure the server. Only
# runs if the system state file does not already exist — subsequent
# installs leave the system config alone.
maybe_migrate_user_state() {
    local target="${SUDO_USER:-}"
    [[ -n "$target" && "$target" != "root" ]] || return
    local home
    home="$(getent passwd "$target" | cut -d: -f6 || true)"
    [[ -n "$home" && -d "$home" ]] || return

    local src="${home}/.local/state/st/st-server-config.json"
    local dst="${STATE_DIR}/st-server-config.json"

    if [[ -f "$dst" ]]; then
        log "System config $dst already exists — skipping user-mode migration."
        return
    fi
    if [[ ! -f "$src" ]]; then
        log "No prior user-mode config at $src — nothing to migrate."
        return
    fi

    log "Migrating user-mode config $src -> $dst"
    install -o st -g st -m 0640 "$src" "$dst"
    log "Old token, codec/bitrate/quality, and peer id carried over. Clients do not need to re-pair."
}

write_autostart_entry() {
    log "Writing $AUTOSTART_PATH (user-session tray companion)"
    # systemd-cat pipes stdout+stderr into the user journal under the tag
    # "st-server-tray", so `journalctl --user -t st-server-tray` works.
    install -Dm0644 /dev/stdin "$AUTOSTART_PATH" <<EOF
[Desktop Entry]
Type=Application
Name=st Server Tray
Comment=Status tray icon + controls for the st-server system service
Exec=systemd-cat -t st-server-tray ${PREFIX}/st-server --tray
Icon=st-server
Terminal=false
Categories=Network;RemoteAccess;
X-GNOME-Autostart-enabled=true
StartupNotify=false
NoDisplay=true
EOF
}

# Add the invoking user to the `st` group so the tray companion can read
# the config file (mode 0640 st:st). Skipped if the installer is not being
# run via sudo (no SUDO_USER set).
maybe_add_user_to_group() {
    local target="${SUDO_USER:-}"
    if [[ -z "$target" ]] || [[ "$target" == "root" ]]; then
        warn "Not running via sudo (SUDO_USER is empty) — skipping 'usermod -aG st'."
        warn "The tray companion will not be able to read the token until you run:"
        warn "    sudo usermod -aG st <your-user>"
        return
    fi
    if id -nG "$target" | tr ' ' '\n' | grep -qx st; then
        log "User '$target' already in group 'st'."
        return
    fi
    log "Adding user '$target' to group 'st' (log out + back in for it to take effect)."
    usermod -aG st "$target"
}

# Kick the tray companion in the current user's session so the icon
# appears without waiting for a re-login. The new process inherits
# supplementary groups freshly from /etc/group, so it picks up the `st`
# group even though the user's running shell does not.
maybe_launch_tray_now() {
    local target="${SUDO_USER:-}"
    if [[ -z "$target" ]] || [[ "$target" == "root" ]]; then
        log "Skipping tray auto-launch (no SUDO_USER). It will start on next login via the autostart entry."
        return
    fi
    local uid bus
    uid="$(id -u "$target")"
    bus="/run/user/${uid}/bus"
    if [[ ! -S "$bus" ]]; then
        warn "No user D-Bus session at $bus; tray will appear on next desktop login."
        return
    fi

    # Don't stack multiple tray instances.
    pkill -u "$target" -f "${PREFIX}/st-server --tray" 2>/dev/null || true

    log "Launching tray companion for '$target' in the current session"
    # systemd-cat puts stdout/stderr into the user journal under tag
    # "st-server-tray"; view later with `journalctl --user -t st-server-tray -f`.
    runuser -u "$target" -- env \
        XDG_RUNTIME_DIR="/run/user/${uid}" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=${bus}" \
        systemd-cat -t st-server-tray "${PREFIX}/st-server" --tray </dev/null &
    disown || true
}

maybe_enable_service() {
    systemctl daemon-reload
    if [[ -n "${ST_SERVER_NO_ENABLE:-}" ]]; then
        log "ST_SERVER_NO_ENABLE is set — not enabling or starting the service."
        log "Enable manually later with: systemctl enable --now st-server"
        return
    fi
    log "Enabling and starting st-server.service"
    # Stop any prior instance cleanly so we swap the binary, not run two.
    systemctl stop st-server.service 2>/dev/null || true
    systemctl enable --now st-server.service
}

print_token_hint() {
    local cfg="${STATE_DIR}/st-server-config.json"
    cat <<EOF

-------------------------------------------------------------------
st-server is installed and running as a system service.

  Status:      systemctl status st-server
  Server logs: journalctl -u st-server -f
  Tray logs:   journalctl --user -t st-server-tray -f
  Binary:      ${PREFIX}/st-server
  State:       ${STATE_DIR}
  Unit:        ${SERVICE_PATH}

First-connect token (keep it secret — anyone with it can control this
machine):

  sudo cat ${cfg}

Override the token in advance by setting ST_SERVER_TOKEN=<hex> before
running this installer, or by editing the unit with:

  sudo systemctl edit st-server

and adding:

  [Service]
  Environment=ST_TOKEN=<your-token>
-------------------------------------------------------------------
EOF
}

uninstall() {
    local purge="${1:-0}"

    log "Stopping any running tray companions"
    pkill -f "${PREFIX}/st-server --tray" 2>/dev/null || true

    log "Stopping and disabling st-server.service"
    systemctl disable --now st-server.service 2>/dev/null || true

    log "Removing service unit, udev rule, autostart entry, binary symlink"
    rm -f "$SERVICE_PATH" "$UDEV_PATH" "$AUTOSTART_PATH" "$BIN_SYMLINK"
    # /etc/modules-load.d drop-in written during install.
    rm -f /etc/modules-load.d/st-server.conf

    systemctl daemon-reload
    udevadm control --reload 2>/dev/null || true

    if [[ -d "$PREFIX" ]]; then
        log "Removing install prefix $PREFIX"
        rm -rf "$PREFIX"
    fi

    if [[ "$purge" == "1" ]]; then
        log "Purging state dir $STATE_DIR"
        rm -rf "$STATE_DIR"
        log "Removing sysusers entry $SYSUSERS_PATH"
        rm -f "$SYSUSERS_PATH"
        if id st >/dev/null 2>&1; then
            log "Removing 'st' user and group"
            userdel st 2>/dev/null || true
            groupdel st 2>/dev/null || true
        fi
    else
        log "State dir $STATE_DIR kept (token + portal tokens preserved)."
        log "Run with --purge to remove it and the 'st' user/group as well."
    fi

    cat <<EOF

-------------------------------------------------------------------
st-server is uninstalled.

  Reinstall anytime with:
    curl -fsSL https://raw.githubusercontent.com/${REPO}/main/packaging/linux/install.sh | sudo bash
-------------------------------------------------------------------
EOF
}

main() {
    require_root
    require_cmds

    local mode="install"
    local purge="0"
    for arg in "$@"; do
        case "$arg" in
            --uninstall) mode="uninstall" ;;
            --purge)     purge="1" ;;
            -h|--help)
                cat <<EOF
Usage: install.sh [--uninstall [--purge]]

  (no args)        Install the latest published release as a system service.
  --uninstall      Remove the service and binary. State dir is preserved.
  --uninstall --purge
                   Remove everything including /var/lib/st-server and the st user.
EOF
                return 0
                ;;
            *) die "Unknown argument: $arg (try --help)" ;;
        esac
    done

    if [[ "$mode" == "uninstall" ]]; then
        uninstall "$purge"
        return
    fi

    local platform version
    platform="$(detect_platform)"
    version="$(resolve_version)"
    log "Installing st-server ${version} (${platform}) into ${PREFIX}"

    download_and_extract "$version" "$platform" "$PREFIX"
    write_sysusers
    ensure_state_dir
    maybe_migrate_user_state
    write_udev
    write_service
    write_bin_symlink
    write_autostart_entry
    maybe_add_user_to_group
    maybe_enable_service
    maybe_launch_tray_now
    print_token_hint
}

main "$@"
