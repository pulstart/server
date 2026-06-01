#!/usr/bin/env bash
# st-server Linux installer.
#
# DEFAULT (no args): SYSTEM-WIDE. Installs a root systemd service that starts at
# the login screen and follows whichever user logs in — KMS video + uinput input
# work at the greeter and across user switches, audio follows the active user via
# logind, and a per-user tray agent (`st-server --tray`) drives the service over a
# control socket. This grants token holders root-level remote control of the
# machine from the login screen onward (see --system section in README).
#
#     curl -fsSL https://raw.githubusercontent.com/pulstart/server/main/packaging/linux/install.sh | bash
#
# --user: per-user mode instead — a systemd *user* service in the invoking user's
# session. No root service; reaches PipeWire/PulseAudio/portal/compositor
# natively. Smallest blast radius. The root steps (uinput udev rule, cap_sys_admin
# on the binary for dialog-free KMS, and a path-unit re-applying the cap after
# self-updates) are bundled into one sudo block.
#
#     curl -fsSL .../install.sh | bash -s -- --user
#
# Uninstall (match the install scope):
#     curl -fsSL .../install.sh | bash -s -- --uninstall          # system-wide
#     curl -fsSL .../install.sh | bash -s -- --user --uninstall   # per-user
#
# Environment knobs:
#     ST_SERVER_VERSION=v0.4.6    Pin a specific release (default: latest).
#     ST_SERVER_PREFIX=$HOME/...  Override the user install prefix.
#     ST_SERVER_NO_ENABLE=1       Install but do not `systemctl --user enable --now`.

set -euo pipefail

REPO="pulstart/server"
PREFIX="${ST_SERVER_PREFIX:-${HOME}/.local/share/st-server}"
BIN_DIR="${HOME}/.local/bin"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
DESKTOP_DIR="${HOME}/.local/share/applications"
ICON_DIR="${HOME}/.local/share/icons/hicolor/256x256/apps"
UDEV_PATH="/etc/udev/rules.d/99-st-server.rules"
MODULES_LOAD_PATH="/etc/modules-load.d/st-server.conf"

# --- System-wide mode paths (install.sh --system) ---
SYSTEM_PREFIX="${ST_SYSTEM_PREFIX:-/opt/st-server}"
SYSTEM_BIN="/usr/local/bin/st-server"
SYSTEM_SERVICE_PATH="/etc/systemd/system/st-server.service"
GLOBAL_TRAY_PATH="/etc/systemd/user/st-server-tray.service"
TMPFILES_PATH="/etc/tmpfiles.d/st-server.conf"
SYSTEM_DESKTOP_PATH="/usr/share/applications/st-server.desktop"
SYSTEM_STATE_DIR="/var/lib/st-server"
SOCKET_DIR="/run/st-server"
SYSTEM_GROUP="st-server"

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
# Never give up on a crash loop: an always-on streaming host should keep
# retrying rather than land in 'failed' after systemd's default 5-starts/10s.
# RestartSec below still throttles to one attempt every 2s.
StartLimitIntervalSec=0

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
    log "Writing $dst (start-menu launcher)"
    # Visible in the application menu. Clicking it starts the systemd *user*
    # service rather than launching the binary directly — launching a second
    # process would fight the already-running unit for the TCP/UDP ports.
    # Starting an already-running unit is a harmless no-op.
    install -Dm0644 /dev/stdin "$dst" <<EOF
[Desktop Entry]
Type=Application
Name=st Server
GenericName=Low-Latency Game Streaming Server
Comment=Start the st-server streaming service in your session
Exec=systemctl --user start st-server.service
Icon=st-server
Terminal=false
Categories=Network;RemoteAccess;
Keywords=stream;remote;game;moonlight;sunshine;
StartupWMClass=st-server
StartupNotify=false
EOF
}

# Root-only setup, bundled into one sudo block (creds cache after the first
# prompt): the uinput udev rule, cap_sys_admin on the binary for dialog-free
# KMS capture, and a path-unit that re-applies the cap after self-updates.
ensure_privileged_setup() {
    local bin="${PREFIX}/st-server"
    local run_user
    run_user="$(id -un)"

    # --- /dev/uinput udev rule (input injection) ---
    if [[ -f "$UDEV_PATH" ]] && grep -q "uinput" "$UDEV_PATH"; then
        log "Udev rule $UDEV_PATH already present."
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
    if id -nG "$run_user" | tr ' ' '\n' | grep -qx input; then
        log "User '$run_user' already in group 'input'."
    else
        log "Adding user '$run_user' to group 'input' (log out + back in for it to take effect)."
        sudo usermod -aG input "$run_user"
    fi

    # --- Dialog-free KMS capture: cap_sys_admin on the binary ---
    # On Wayland the server prefers KMS/DRM capture (no XDG share dialog, native
    # multi-monitor). KWin holds DRM-master, so PRIME-exporting the scanout
    # buffer needs CAP_SYS_ADMIN. This is best-effort: without it the server
    # auto-falls-back to the PipeWire portal (with its dialog).
    local setcap_bin
    setcap_bin="$(command -v setcap || true)"
    if [[ -z "$setcap_bin" ]]; then
        warn "setcap not found (install libcap / libcap2-bin) — KMS dialog-free capture disabled; the server will use the PipeWire portal instead."
        return
    fi

    log "Granting cap_sys_admin to ${bin} (dialog-free KMS capture; needs sudo)"
    sudo "$setcap_bin" cap_sys_admin+ep "$bin"
    log "  $(getcap "$bin" 2>/dev/null || echo 'getcap unavailable')"

    # Self-update (updater.rs) replaces the binary in place, which DROPS the
    # file capability. Install a tiny root path-unit that re-applies the cap
    # whenever the binary changes, so upgrades stay dialog-free with no further
    # prompts. (The just-relaunched post-update process may briefly use the
    # portal until the next service start picks up the re-applied cap.)
    local svc="st-server-setcap-${run_user}"
    log "Installing ${svc}.path so auto-updates keep the capability"
    sudo install -Dm0644 /dev/stdin "/etc/systemd/system/${svc}.service" <<EOF
[Unit]
Description=Re-apply cap_sys_admin to st-server after updates (${run_user})

[Service]
Type=oneshot
ExecStart=${setcap_bin} cap_sys_admin+ep ${bin}
EOF
    sudo install -Dm0644 /dev/stdin "/etc/systemd/system/${svc}.path" <<EOF
[Unit]
Description=Watch st-server and re-apply cap_sys_admin on change (${run_user})

[Path]
PathChanged=${bin}
Unit=${svc}.service

[Install]
WantedBy=paths.target
EOF
    sudo systemctl daemon-reload
    sudo systemctl enable --now "${svc}.path"
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
  Capture:  Wayland uses KMS direct capture (no screen-share dialog) when
            cap_sys_admin is set above; otherwise it falls back to the
            PipeWire portal. Force with ST_CAPTURE=kms|pipewire.

First-connect token (keep it secret — anyone with it can control this
machine):

  cat ${cfg}

Or click the tray icon on this machine to copy the token.

Set the token live from the tray ("Set Token") — it persists and survives
restarts. ST_TOKEN seeds a token only on first run (no token yet); it does
NOT override a tray-set token. To force ST_TOKEN, clear the saved "token"
field in the config first, then add it to the unit:

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

    local run_user setcap_svc
    run_user="$(id -un)"
    setcap_svc="st-server-setcap-${run_user}"
    if [[ -f "/etc/systemd/system/${setcap_svc}.path" ]]; then
        log "Removing cap_sys_admin re-apply units (needs sudo)"
        sudo systemctl disable --now "${setcap_svc}.path" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/${setcap_svc}.path" \
                   "/etc/systemd/system/${setcap_svc}.service"
        sudo systemctl daemon-reload 2>/dev/null || true
    fi

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

# =====================================================================
# System-wide mode (install.sh --system)
#
# Installs st-server as a ROOT system service that starts at the login
# screen and follows whichever user logs in:
#   - video: KMS captures the active scanout (greeter, then any user)
#   - input: uinput injects at the kernel level
#   - audio: a logind watcher repoints PULSE_SERVER at the active user
# The tray is a separate per-user agent (`st-server --tray`) that reaches
# the service over a control socket. This is a meaningful privilege
# escalation over per-user mode: anyone with the token gets root-level
# remote control of this machine from the login screen onward.
# =====================================================================

# Path to the invoking user's per-user st-server config (where their token and
# peer id live). Mirrors server_control.rs config_path(): ST_STATE_DIR is not
# consulted here because the user-mode server never sets it; honor XDG_STATE_HOME
# then fall back to ~/.local/state/st.
user_config_path() {
    local base
    if [[ -n "${XDG_STATE_HOME:-}" ]]; then
        base="${XDG_STATE_HOME}/st"
    else
        base="${HOME}/.local/state/st"
    fi
    echo "${base}/${CONFIG_FILENAME}"
}

CONFIG_FILENAME="st-server-config.json"
SYSTEM_CONFIG_PATH="${SYSTEM_STATE_DIR}/${CONFIG_FILENAME}"

# Seed the system config from the user's existing per-user config so the
# system-wide service reuses the SAME token (and peer id) the user already sees
# in their tray and has entered into clients. Without this, the root service
# mints a fresh token in /var/lib/st-server on first start and every existing
# client silently stops discovering the host. Only seeds when no system config
# exists yet — never clobbers an already-running system token.
seed_system_config_from_user() {
    if sudo test -s "$SYSTEM_CONFIG_PATH"; then
        log "System token already present ($SYSTEM_CONFIG_PATH) — keeping it."
        return
    fi
    local user_cfg
    user_cfg="$(user_config_path)"
    if [[ ! -s "$user_cfg" ]]; then
        log "No per-user token to migrate; the service will generate one (read it from the tray or 'sudo cat $SYSTEM_CONFIG_PATH')."
        return
    fi
    log "Seeding system token from your per-user config ($user_cfg) so existing clients keep working."
    sudo install -Dm0600 -o root -g root "$user_cfg" "$SYSTEM_CONFIG_PATH"
}

ensure_system_group() {
    if getent group "$SYSTEM_GROUP" >/dev/null 2>&1; then
        log "Group '$SYSTEM_GROUP' already exists."
    else
        log "Creating system group '$SYSTEM_GROUP' (needs sudo)"
        sudo groupadd --system "$SYSTEM_GROUP"
    fi
    local run_user
    run_user="$(id -un)"
    if id -nG "$run_user" | tr ' ' '\n' | grep -qx "$SYSTEM_GROUP"; then
        log "User '$run_user' already in group '$SYSTEM_GROUP'."
    else
        log "Adding '$run_user' to '$SYSTEM_GROUP' so the tray can reach the control socket (log out + back in to apply)."
        sudo usermod -aG "$SYSTEM_GROUP" "$run_user"
    fi
}

# /dev/uinput must exist for input injection. The root service can open it
# directly, so (unlike per-user mode) no input-group ACL is needed here.
ensure_uinput_node() {
    if [[ -f "$MODULES_LOAD_PATH" ]]; then
        log "uinput modules-load drop-in already present."
    else
        log "Ensuring uinput kernel module loads at boot (needs sudo)"
        echo "uinput" | sudo tee "$MODULES_LOAD_PATH" >/dev/null
    fi
    sudo modprobe uinput 2>/dev/null || true
}

write_tmpfiles() {
    log "Writing $TMPFILES_PATH (control-socket dir)"
    # setgid (2750) so the socket the root service creates inherits the
    # st-server group, letting tray-agent users connect via the 0660 socket.
    sudo install -Dm0644 /dev/stdin "$TMPFILES_PATH" <<EOF
# Created by packaging/linux/install.sh --system.
d ${SOCKET_DIR} 2750 root ${SYSTEM_GROUP} -
EOF
    sudo systemd-tmpfiles --create "$TMPFILES_PATH" >/dev/null 2>&1 || true
}

write_system_service() {
    log "Writing $SYSTEM_SERVICE_PATH"
    sudo install -Dm0644 /dev/stdin "$SYSTEM_SERVICE_PATH" <<EOF
[Unit]
Description=st low-latency game-streaming server (system-wide)
Documentation=https://github.com/${REPO}
# Start once the graphical login screen is up so there is an active scanout
# (the greeter holds DRM-master) to capture.
After=display-manager.service systemd-logind.service
Wants=graphical.target
# Never give up on a crash loop: an always-on streaming host should keep
# retrying rather than land in 'failed' after systemd's default 5-starts/10s.
# RestartSec below still throttles to one attempt every 2s.
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart=${SYSTEM_PREFIX}/st-server --system
Restart=on-failure
RestartSec=2
User=root
# KMS PRIME-export of a compositor-owned scanout needs CAP_SYS_ADMIN. root
# already holds it; declaring it documents the requirement and survives a
# future switch to a non-root User=.
AmbientCapabilities=CAP_SYS_ADMIN
SupplementaryGroups=video render input
# Force a specific backend only for debugging, e.g.:
#   Environment=ST_CAPTURE=kms
#   Environment=ST_AUDIO_FOLLOW=0

[Install]
WantedBy=graphical.target
EOF
}

write_global_tray_unit() {
    log "Writing $GLOBAL_TRAY_PATH (per-user tray agent, all users)"
    sudo install -Dm0644 /dev/stdin "$GLOBAL_TRAY_PATH" <<EOF
[Unit]
Description=st-server tray agent
After=graphical-session.target
PartOf=graphical-session.target
# Keep retrying instead of landing in 'failed' after a quick restart burst
# (e.g. control socket not up yet at boot). RestartSec throttles attempts.
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart=${SYSTEM_BIN} --tray
Restart=on-failure
RestartSec=3

[Install]
WantedBy=graphical-session.target
EOF
}

# System-wide start-menu launcher, installed for ALL users. The root service
# itself starts at boot and can't be (re)started from an unprivileged click;
# what a user interacts with is their per-user tray agent, so this entry starts
# that. Starting an already-running tray is a harmless no-op.
write_system_desktop_entry() {
    log "Writing $SYSTEM_DESKTOP_PATH (start-menu launcher, all users)"
    sudo install -Dm0644 /dev/stdin "$SYSTEM_DESKTOP_PATH" <<EOF
[Desktop Entry]
Type=Application
Name=st Server Tray
GenericName=Low-Latency Game Streaming Server
Comment=Show the st-server tray for the system-wide streaming service
Exec=systemctl --user start st-server-tray.service
Icon=st-server
Terminal=false
Categories=Network;RemoteAccess;
Keywords=stream;remote;game;moonlight;sunshine;
StartupWMClass=st-server
StartupNotify=false
EOF
}

enable_system_services() {
    sudo systemctl daemon-reload
    if [[ -n "${ST_SERVER_NO_ENABLE:-}" ]]; then
        log "ST_SERVER_NO_ENABLE is set — not enabling the system service."
        log "Enable manually: sudo systemctl enable --now st-server && systemctl --global enable st-server-tray"
        return
    fi
    log "Enabling and starting the system service"
    sudo systemctl enable --now st-server.service
    # --global enables the tray unit for every user's session (applies on
    # their next login; start it now for the current user if a session exists).
    log "Enabling the per-user tray agent for all users"
    sudo systemctl --global enable st-server-tray.service
    systemctl --user start st-server-tray.service 2>/dev/null || true
}

install_system() {
    local platform version tmp
    platform="$(detect_platform)"
    version="$(resolve_version)"
    log "Installing st-server ${version} (${platform}) system-wide into ${SYSTEM_PREFIX}"

    tmp="$(mktemp -d)"
    download_and_extract "$version" "$platform" "$tmp/st-server"

    log "Installing binary + assets into ${SYSTEM_PREFIX} (needs sudo)"
    sudo rm -rf "$SYSTEM_PREFIX"
    sudo mkdir -p "$SYSTEM_PREFIX"
    sudo cp -a "$tmp/st-server/." "$SYSTEM_PREFIX/"
    rm -rf "$tmp"
    sudo ln -sf "$SYSTEM_PREFIX/st-server" "$SYSTEM_BIN"
    sudo install -d -m0700 "$SYSTEM_STATE_DIR"

    # Reuse the user's existing token (the one shown in the tray / already in
    # clients) instead of generating a new one. Must run before the service
    # starts so its first read picks up the seeded token.
    seed_system_config_from_user

    ensure_system_group
    ensure_uinput_node
    write_tmpfiles
    write_system_service
    write_global_tray_unit
    write_system_desktop_entry
    enable_system_services
    print_system_hint
}

print_system_hint() {
    cat <<EOF

-------------------------------------------------------------------
st-server is installed SYSTEM-WIDE and starts at the login screen.

  Status:   systemctl status st-server
  Logs:     journalctl -u st-server -f
  Binary:   ${SYSTEM_PREFIX}/st-server
  State:    ${SYSTEM_STATE_DIR}/  (root-owned)
  Service:  ${SYSTEM_SERVICE_PATH}  (root, --system)
  Tray:     ${GLOBAL_TRAY_PATH}  (per-user 'st-server --tray')
  Socket:   ${SOCKET_DIR}/control.sock  (group ${SYSTEM_GROUP})

Video + input work at the greeter and follow whichever user logs in.
Audio follows the active user (ST_AUDIO_FOLLOW=0 to disable).

The tray icon appears in each user's session and reaches the service
over the control socket. You were added to the '${SYSTEM_GROUP}' group —
log out and back in for the tray to connect.

A "st Server Tray" entry is in your application menu — launch it to bring
the tray back if you closed it ("Quit Tray" closes only your tray; the
system service keeps running). Stop the service itself with:

  sudo systemctl stop st-server

First-connect token (keep it secret — root-level control):

  sudo cat ${SYSTEM_STATE_DIR}/st-server-config.json

Or click the tray icon and pick "Copy Token" / "Set Token". A token set
from the tray persists and survives restarts. ST_TOKEN only seeds a token
on first run (no token yet) and will NOT override a tray-set token.

Uninstall:
  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/packaging/linux/install.sh | bash -s -- --system --uninstall
-------------------------------------------------------------------
EOF
}

uninstall_system() {
    log "Stopping and disabling the system service"
    sudo systemctl disable --now st-server.service 2>/dev/null || true
    sudo systemctl --global disable st-server-tray.service 2>/dev/null || true
    systemctl --user stop st-server-tray.service 2>/dev/null || true

    log "Removing system units, tmpfiles, desktop entry, binary symlink (needs sudo)"
    sudo rm -f "$SYSTEM_SERVICE_PATH" "$GLOBAL_TRAY_PATH" "$TMPFILES_PATH" \
               "$SYSTEM_DESKTOP_PATH" "$SYSTEM_BIN"
    sudo systemctl daemon-reload 2>/dev/null || true

    if [[ -d "$SYSTEM_PREFIX" ]]; then
        log "Removing install prefix $SYSTEM_PREFIX"
        sudo rm -rf "$SYSTEM_PREFIX"
    fi
    sudo rm -rf "$SOCKET_DIR" 2>/dev/null || true

    cat <<EOF

-------------------------------------------------------------------
st-server (system-wide) is uninstalled.

State at ${SYSTEM_STATE_DIR}/ is kept so the token survives a reinstall.
Remove it by hand for a clean slate:

  sudo rm -rf ${SYSTEM_STATE_DIR}

The '${SYSTEM_GROUP}' group and the uinput modules-load drop-in are left in
place (harmless). Remove them manually if you want:

  sudo groupdel ${SYSTEM_GROUP}
  sudo rm -f ${MODULES_LOAD_PATH}
-------------------------------------------------------------------
EOF
}

main() {
    require_user
    require_cmds

    local mode="install" scope="system"
    for arg in "$@"; do
        case "$arg" in
            --uninstall) mode="uninstall" ;;
            --system) scope="system" ;;
            --user) scope="user" ;;
            -h|--help)
                cat <<EOF
Usage: install.sh [--user] [--uninstall]

  (no args)            Install SYSTEM-WIDE (default): a root service that starts
                       at the login screen and follows whichever user logs in
                       (KMS video + uinput input + logind audio-follow), plus a
                       per-user tray agent. Grants token holders root-level
                       remote control from the greeter onward.
  --user               Install as a per-user systemd service that starts on
                       desktop login (no root service, smallest blast radius).
  --uninstall          Remove the system-wide install (state preserved).
  --user --uninstall   Remove the per-user install (state preserved).
EOF
                return 0
                ;;
            *) die "Unknown argument: $arg (try --help)" ;;
        esac
    done

    if [[ "$scope" == "system" ]]; then
        if [[ "$mode" == "uninstall" ]]; then
            uninstall_system
        else
            install_system
        fi
        return
    fi

    if [[ "$mode" == "uninstall" ]]; then
        uninstall
        return
    fi

    local platform version
    platform="$(detect_platform)"
    version="$(resolve_version)"
    log "Installing st-server ${version} (${platform}) into ${PREFIX}"

    download_and_extract "$version" "$platform" "$PREFIX"
    ensure_privileged_setup
    write_bin_symlink
    write_user_service
    write_desktop_entry
    maybe_enable_service
    print_token_hint
}

main "$@"
