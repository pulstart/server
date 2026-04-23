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
    trap 'rm -rf "$tmp"' RETURN

    log "Downloading $url"
    curl -fsSL "$url" -o "$tmp/$asset" || die "Failed to download $url"

    log "Extracting into $target_dir"
    rm -rf "$target_dir"
    mkdir -p "$target_dir"
    tar -xzf "$tmp/$asset" -C "$tmp"
    local extracted="$tmp/st-server-${version}-${platform}"
    [[ -d "$extracted" ]] || die "Unexpected archive layout: $extracted missing"
    # Flatten: move contents of the versioned dir into $target_dir.
    ( shopt -s dotglob; mv "$extracted"/* "$target_dir"/ )

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
    install -Dm0644 /dev/stdin "$UDEV_PATH" <<'EOF'
# Created by packaging/linux/install.sh.
# Grants the `st` system user the device access needed for KMS capture +
# uinput injection on the login screen.

KERNEL=="uinput", MODE="0660", GROUP="input", TAG+="uaccess"
SUBSYSTEM=="drm", KERNEL=="card[0-9]*", MODE="0660", GROUP="video"
SUBSYSTEM=="drm", KERNEL=="renderD*",   MODE="0660", GROUP="render"
SUBSYSTEM=="input", KERNEL=="event*",   MODE="0660", GROUP="input"
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
DeviceAllow=/dev/uinput rw
DeviceAllow=char-drm rw
DeviceAllow=char-input rw
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

  Status:   systemctl status st-server
  Logs:     journalctl -u st-server -f
  Binary:   ${PREFIX}/st-server
  State:    ${STATE_DIR}
  Unit:     ${SERVICE_PATH}

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

main() {
    require_root
    require_cmds

    local platform version
    platform="$(detect_platform)"
    version="$(resolve_version)"
    log "Installing st-server ${version} (${platform}) into ${PREFIX}"

    download_and_extract "$version" "$platform" "$PREFIX"
    write_sysusers
    ensure_state_dir
    write_udev
    write_service
    write_bin_symlink
    maybe_enable_service
    print_token_hint
}

main "$@"
