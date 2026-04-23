#!/usr/bin/env bash
# st-server Linux installer (user-session mode).
#
# Installs st-server into the invoking user's home as a systemd user service
# that starts on desktop login. Because it runs under the user's session bus,
# it reaches PipeWire, PulseAudio, xdg-desktop-portal, and the compositor
# natively — no cross-user permission juggling.
#
# One-liner:
#     curl -fsSL https://raw.githubusercontent.com/zhey/st/main/packaging/linux/install.sh | bash
#
# The only step that needs root is the /dev/uinput udev rule (input
# injection). The script re-execs itself via sudo for that bit only.
#
# Uninstall:
#     curl -fsSL https://raw.githubusercontent.com/zhey/st/main/packaging/linux/install.sh | bash -s -- --uninstall
#
# Environment knobs:
#     ST_SERVER_VERSION=v0.4.6    Pin a specific release (default: latest).
#     ST_SERVER_PREFIX=$HOME/...  Override the user install prefix.
#     ST_SERVER_NO_ENABLE=1       Install but do not `systemctl --user enable --now`.

set -euo pipefail

REPO="zhey/st"
PREFIX="${ST_SERVER_PREFIX:-${HOME}/.local/share/st-server}"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
DESKTOP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor/256x256/apps"
UDEV_PATH="/etc/udev/rules.d/99-st-server.rules"
MODULES_LOAD_PATH="/etc/modules-load.d/st-server.conf"

log()  { printf '\033[1;34m[st-install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[st-install]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[st-install]\033[0m %s\n' "$*" >&2; exit 1; }

require_user() {
    if [[ "$(id -u)" -eq 0 ]]; then
        die "Do not run this installer as root. Run it as your normal desktop user; \
it will call sudo itself for the single udev rule step that needs it."
    fi
}

require_cmds() {
    local missing=()
    for cmd in curl tar systemctl install chmod; do
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
        aarch64|arm64) die "Architecture $arch is not published (only linux-x64 is built). Build from source." ;;
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
    ( shopt -s dotglob; mv "$extracted"/* "$target_dir"/ )
    rm -rf "$tmp"

    [[ -x "$target_dir/st-server" ]] || die "Launcher $target_dir/st-server is missing or not executable"
}

write_bin_symlink() {
    mkdir -p "$BIN_DIR"
    log "Linking ${BIN_DIR}/st-server -> ${PREFIX}/st-server"
    ln -sf "$PREFIX/st-server" "$BIN_DIR/st-server"
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) warn "$BIN_DIR is not on your PATH. Add it to ~/.profile or ~/.bashrc so 'st-server' resolves in your shell." ;;
    esac
}

write_user_service() {
    mkdir -p "$SYSTEMD_USER_DIR"
    local unit_path="${SYSTEMD_USER_DIR}/st-server.service"
    log "Writing $unit_path"
    install -Dm0644 /dev/stdin "$unit_path" <<EOF
[Unit]
Description=st low-latency game-streaming server
Documentation=https://github.com/${REPO}
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart=${PREFIX}/st-server
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
EOF
}

write_desktop_entry() {
    mkdir -p "$DESKTOP_DIR"
    local dst="${DESKTOP_DIR}/st-server.desktop"
    log "Writing $dst"
    install -Dm0644 /dev/stdin "$dst" <<EOF
[Desktop Entry]
Type=Application
Name=st Server
GenericName=Low-Latency Game Streaming Server
Comment=Streams this desktop to st clients over the network
Exec=${BIN_DIR}/st-server
Icon=st-server
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=st-server
StartupNotify=false
NoDisplay=true
EOF
}

# Install the uinput udev rule. This is the only step that needs root.
# We re-exec ourselves via sudo with a narrow command so the user sees
# exactly what's being elevated.
ensure_udev_rule() {
    if [[ -f "$UDEV_PATH" ]] && grep -q "uinput" "$UDEV_PATH"; then
        log "Udev rule $UDEV_PATH already present — skipping sudo step."
    else
        log "Installing udev rule for /dev/uinput (needs sudo)"
        sudo install -Dm0644 /dev/stdin "$UDEV_PATH" <<'EOF'
# Created by packaging/linux/install.sh.
# Grants /dev/uinput the group access needed for input injection.
# DRM and evdev nodes are deliberately left to the distro defaults so
# logind's uaccess ACL for the logged-in user isn't reset.

KERNEL=="uinput", MODE="0660", GROUP="input", TAG+="uaccess"
EOF
        sudo udevadm control --reload
        sudo udevadm trigger --subsystem-match=input || true
        sudo modprobe uinput 2>/dev/null || true
        echo "uinput" | sudo tee "$MODULES_LOAD_PATH" >/dev/null
    fi

    # Make sure the running user is in the `input` group so the uaccess tag
    # applies. (Desktop logind usually grants uaccess to the local user, but
    # this guarantees it for remote/systemd-run sessions too.)
    if id -nG "$USER" | tr ' ' '\n' | grep -qx input; then
        log "User '$USER' already in group 'input'."
    else
        log "Adding user '$USER' to group 'input' (log out + back in for it to take effect)."
        sudo usermod -aG input "$USER"
    fi
}

maybe_enable_service() {
    systemctl --user daemon-reload
    if [[ -n "${ST_SERVER_NO_ENABLE:-}" ]]; then
        log "ST_SERVER_NO_ENABLE is set — not enabling or starting the service."
        log "Enable manually with: systemctl --user enable --now st-server"
        return
    fi
    log "Enabling and starting st-server user service"
    systemctl --user stop st-server.service 2>/dev/null || true
    systemctl --user enable --now st-server.service
}

print_token_hint() {
    local cfg="${HOME}/.local/state/st/st-server-config.json"
    cat <<EOF

-------------------------------------------------------------------
st-server is installed and running in your user session.

  Status:   systemctl --user status st-server
  Logs:     journalctl --user -u st-server -f
  Binary:   ${PREFIX}/st-server
  State:    ${HOME}/.local/state/st/
  Unit:     ${SYSTEMD_USER_DIR}/st-server.service

First-connect token (keep it secret — anyone with it can control this
machine):

  cat ${cfg}

Or click the tray icon on this machine to copy the token.

Override the token by setting ST_TOKEN=<hex> in the unit:

  systemctl --user edit st-server
  (add: [Service] Environment=ST_TOKEN=<your-token>)

-------------------------------------------------------------------
EOF
}

uninstall() {
    log "Stopping and disabling st-server user service"
    systemctl --user disable --now st-server.service 2>/dev/null || true

    log "Removing user unit, desktop entry, binary symlink"
    rm -f "${SYSTEMD_USER_DIR}/st-server.service"
    rm -f "${DESKTOP_DIR}/st-server.desktop"
    rm -f "${BIN_DIR}/st-server"

    if [[ -d "$PREFIX" ]]; then
        log "Removing install prefix $PREFIX"
        rm -rf "$PREFIX"
    fi

    systemctl --user daemon-reload 2>/dev/null || true

    if [[ -f "$UDEV_PATH" ]] || [[ -f "$MODULES_LOAD_PATH" ]]; then
        log "Removing udev rule and modules-load drop-in (needs sudo)"
        sudo rm -f "$UDEV_PATH" "$MODULES_LOAD_PATH"
        sudo udevadm control --reload 2>/dev/null || true
    fi

    cat <<EOF

-------------------------------------------------------------------
st-server is uninstalled.

State at ${HOME}/.local/state/st/ is kept so tokens and peer id
survive a reinstall. Remove it by hand if you want a clean slate:

  rm -rf ${HOME}/.local/state/st

Reinstall anytime with:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/packaging/linux/install.sh | bash
-------------------------------------------------------------------
EOF
}

main() {
    require_user
    require_cmds

    local mode="install"
    for arg in "$@"; do
        case "$arg" in
            --uninstall) mode="uninstall" ;;
            -h|--help)
                cat <<EOF
Usage: install.sh [--uninstall]

  (no args)     Install the latest release as a user systemd service.
  --uninstall   Remove the service, binary, desktop entry, and udev rule.
                State at ~/.local/state/st/ is preserved.
EOF
                return 0
                ;;
            *) die "Unknown argument: $arg (try --help)" ;;
        esac
    done

    if [[ "$mode" == "uninstall" ]]; then
        uninstall
        return
    fi

    local platform version
    platform="$(detect_platform)"
    version="$(resolve_version)"
    log "Installing st-server ${version} (${platform}) into ${PREFIX}"

    download_and_extract "$version" "$platform" "$PREFIX"
    ensure_udev_rule
    write_bin_symlink
    write_user_service
    write_desktop_entry
    maybe_enable_service
    print_token_hint
}

main "$@"
