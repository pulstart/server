#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: package-unix.sh --platform <linux-x64|macos-x64|macos-arm64>

Packages an existing release build from target/release/st-server into server/dist/.
EOF
}

read_ldd_paths() {
    local target="$1"
    ldd "$target" | sed -n \
        -e 's/.*=> \(\/[^ ]*\).*/\1/p' \
        -e 's/^\(\/[^ ]*\).*/\1/p'
}

should_bundle_linux_dep() {
    local dep_path="$1"
    local dep_name
    dep_name="$(basename "$dep_path")"

    case "$dep_name" in
        linux-vdso.so.*|ld-linux*.so*|libc.so.*|libm.so.*|libpthread.so.*|librt.so.*|libdl.so.*|libutil.so.*|libresolv.so.*|libnsl.so.*|libcrypt.so.*|libanl.so.*|libBrokenLocale.so.*)
            return 1
            ;;
        libGL.so.*|libGLX.so.*|libEGL.so.*|libdrm.so.*|libva*.so*|libvdpau.so.*|libOpenCL.so.*|libvulkan.so.*)
            return 1
            ;;
        libX11.so.*|libXext.so.*|libXfixes.so.*|libXtst.so.*|libxcb*.so*|libwayland-*.so*|libpulse*.so*|libpipewire-*.so*|libjack.so.*|libasound.so.*|libdbus-1.so.*|libsystemd.so.*|libudev.so.*)
            return 1
            ;;
    esac

    return 0
}

bundle_linux_runtime_libs() {
    local binary_path="$1"
    local package_root="$2"
    local lib_root="$package_root/lib"
    local -a queue
    local dep dep_name resolved_path

    mkdir -p "$lib_root"
    queue=("$binary_path")

    declare -A seen=()

    while ((${#queue[@]} > 0)); do
        local current="${queue[0]}"
        queue=("${queue[@]:1}")

        while IFS= read -r dep; do
            [[ -n "$dep" ]] || continue
            [[ -f "$dep" ]] || continue
            should_bundle_linux_dep "$dep" || continue

            resolved_path="$(readlink -f "$dep")"
            dep_name="$(basename "$dep")"
            [[ -n "$resolved_path" ]] || resolved_path="$dep"

            if [[ -z "${seen[$dep_name]:-}" ]]; then
                cp -L "$dep" "$lib_root/$dep_name"
                chmod 644 "$lib_root/$dep_name"
                seen["$dep_name"]=1
                queue+=("$resolved_path")
            fi
        done < <(read_ldd_paths "$current")
    done
}

platform=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --platform)
            platform="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

case "$platform" in
    linux-x64|macos-x64|macos-arm64) ;;
    *)
        echo "Missing or invalid --platform value: '$platform'" >&2
        usage >&2
        exit 1
        ;;
esac

server_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "$platform" == macos-x64 || "$platform" == macos-arm64 ]]; then
    exec bash "$server_root/scripts/package-macos-app.sh" --platform "$platform"
fi

binary_path="$server_root/target/release/st-server"
version="$(
    sed -n 's/^version = "\(.*\)"/\1/p' "$server_root/Cargo.toml" \
        | head -n 1
)"

if [[ -z "$version" ]]; then
    echo "Unable to resolve server version from Cargo.toml" >&2
    exit 1
fi

if [[ ! -f "$binary_path" ]]; then
    echo "Release binary not found at $binary_path" >&2
    echo "Build it first with: cargo build --release --locked" >&2
    exit 1
fi

dist_root="$server_root/dist"
staging_root="$dist_root/staging"
package_name="st-server-v${version}-${platform}"
package_root="$staging_root/$package_name"

mkdir -p "$staging_root"
rm -rf "$package_root"
mkdir -p "$package_root"

cp "$binary_path" "$package_root/st-server"
mv "$package_root/st-server" "$package_root/st-server-bin"
chmod 755 "$package_root/st-server-bin"

bundle_linux_runtime_libs "$binary_path" "$package_root"

cat > "$package_root/st-server" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -d "$app_dir/lib" ]]; then
    export LD_LIBRARY_PATH="$app_dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$app_dir/st-server-bin" "$@"
EOF
chmod 755 "$package_root/st-server"

cat > "$package_root/README.txt" <<'EOF'
This archive contains the Linux x64 build of st-server.

Run the server through the included launcher:

  ./st-server

The package bundles the user-space codec/runtime libraries that are practical to ship.
The target machine still needs the normal Linux desktop stack for capture/input/audio:
PulseAudio, PipeWire, X11/Wayland, and GPU/display drivers.

Tray integration is optional. Set ST_SERVER_NO_TRAY=1 to force headless mode.
EOF

archive_path="$dist_root/${package_name}.tar.gz"
rm -f "$archive_path"
tar -C "$staging_root" -czf "$archive_path" "$package_name"

echo "Packaged ${platform} artifact at ${archive_path}"
