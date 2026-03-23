#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: package-macos-app.sh --platform <macos-x64|macos-arm64>

Packages target/release/st-server into a macOS .app bundle and, when signing
credentials are present, signs and notarizes it for direct distribution.
EOF
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
    macos-x64|macos-arm64) ;;
    *)
        echo "Missing or invalid --platform value: '$platform'" >&2
        usage >&2
        exit 1
        ;;
esac

server_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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
app_root="$package_root/st-server.app"
app_executable="$app_root/Contents/MacOS/st-server"
archive_path="$dist_root/${package_name}.zip"

mkdir -p "$staging_root"
rm -rf "$package_root"
mkdir -p "$app_root/Contents/MacOS"

copy_app_bundle() {
    cp "$binary_path" "$app_executable"
    chmod 755 "$app_executable"
    cat > "$app_root/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>st-server</string>
    <key>CFBundleIdentifier</key>
    <string>com.pulstart.st-server</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>st-server</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${version}</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
    cat > "$package_root/install.command" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
app_source="$script_dir/st-server.app"

if [[ ! -d "$app_source" ]]; then
    echo "st-server.app was not found next to this installer." >&2
    exit 1
fi

install_root="/Applications"
if [[ ! -w "$install_root" ]]; then
    install_root="$HOME/Applications"
    mkdir -p "$install_root"
fi

app_target="$install_root/st-server.app"
rm -rf "$app_target"
ditto "$app_source" "$app_target"
xattr -dr com.apple.quarantine "$app_target" || true

echo "Installed st-server to $app_target"
echo "Launching app..."
open "$app_target"
echo
echo "st-server runs as a tray/menu-bar app."
EOF
    chmod 755 "$package_root/install.command"
    cat > "$package_root/README.txt" <<'EOF'
This archive contains the macOS build of st-server packaged as a .app bundle.

The app may be unsigned unless Apple signing credentials were provided during
the packaging step, so macOS may ask the user to allow it manually on first
launch.

Use `install.command` in this folder for the easiest install flow.

Recommended install steps:

    mv st-server.app /Applications/
    xattr -dr com.apple.quarantine /Applications/st-server.app
    open /Applications/st-server.app

The server runs as a tray/menu-bar app.
EOF
}

has_codesign_credentials() {
    [[ -n "${MACOS_CERTIFICATE_P12_BASE64:-}" ]] \
        && [[ -n "${MACOS_CERTIFICATE_PASSWORD:-}" ]] \
        && [[ -n "${MACOS_CODESIGN_IDENTITY:-}" ]]
}

has_notary_api_key_credentials() {
    [[ -n "${MACOS_NOTARY_KEY_ID:-}" ]] \
        && [[ -n "${MACOS_NOTARY_ISSUER:-}" ]] \
        && [[ -n "${MACOS_NOTARY_API_KEY_BASE64:-}" ]]
}

has_notary_apple_id_credentials() {
    [[ -n "${MACOS_NOTARY_APPLE_ID:-}" ]] \
        && [[ -n "${MACOS_NOTARY_APP_PASSWORD:-}" ]] \
        && [[ -n "${MACOS_TEAM_ID:-}" ]]
}

assert_notarization_credentials() {
    if has_notary_api_key_credentials || has_notary_apple_id_credentials; then
        return 0
    fi

    cat >&2 <<'EOF'
macOS signing credentials were provided, but notarization credentials are missing.
Set either:
  - MACOS_NOTARY_KEY_ID, MACOS_NOTARY_ISSUER, MACOS_NOTARY_API_KEY_BASE64
or:
  - MACOS_NOTARY_APPLE_ID, MACOS_NOTARY_APP_PASSWORD, MACOS_TEAM_ID
EOF
    exit 1
}

temp_dir=""
keychain_path=""
keychain_password=""

cleanup_signing_material() {
    if [[ -n "$keychain_path" && -f "$keychain_path" ]]; then
        security delete-keychain "$keychain_path" >/dev/null 2>&1 || true
    fi
    if [[ -n "$temp_dir" && -d "$temp_dir" ]]; then
        rm -rf "$temp_dir"
    fi
}

prepare_codesign_keychain() {
    temp_dir="$(mktemp -d)"
    keychain_path="$temp_dir/codesign.keychain-db"
    keychain_password="$(uuidgen)"
    certificate_path="$temp_dir/certificate.p12"

    printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$certificate_path"

    security create-keychain -p "$keychain_password" "$keychain_path"
    security set-keychain-settings -lut 21600 "$keychain_path"
    security unlock-keychain -p "$keychain_password" "$keychain_path"
    security import "$certificate_path" \
        -k "$keychain_path" \
        -P "$MACOS_CERTIFICATE_PASSWORD" \
        -T /usr/bin/codesign \
        -T /usr/bin/security
    security set-key-partition-list \
        -S apple-tool:,apple:,codesign: \
        -s \
        -k "$keychain_password" \
        "$keychain_path"

    existing_keychains=()
    while IFS= read -r keychain; do
        keychain="${keychain#\"}"
        keychain="${keychain%\"}"
        existing_keychains+=("$keychain")
    done < <(security list-keychains -d user)
    security list-keychains -d user -s "$keychain_path" "${existing_keychains[@]}"
}

sign_app_bundle() {
    codesign \
        --force \
        --sign "$MACOS_CODESIGN_IDENTITY" \
        --timestamp \
        --options runtime \
        "$app_executable"

    codesign \
        --force \
        --sign "$MACOS_CODESIGN_IDENTITY" \
        --timestamp \
        --options runtime \
        "$app_root"

    codesign --verify --deep --strict --verbose=2 "$app_root"
}

notarize_app_bundle() {
    notary_zip="$temp_dir/notary-upload.zip"
    ditto -c -k --sequesterRsrc --keepParent "$app_root" "$notary_zip"

    if has_notary_api_key_credentials; then
        notary_key_path="$temp_dir/AuthKey_${MACOS_NOTARY_KEY_ID}.p8"
        printf '%s' "$MACOS_NOTARY_API_KEY_BASE64" | base64 --decode > "$notary_key_path"
        xcrun notarytool submit "$notary_zip" \
            --key "$notary_key_path" \
            --key-id "$MACOS_NOTARY_KEY_ID" \
            --issuer "$MACOS_NOTARY_ISSUER" \
            --wait
    else
        xcrun notarytool submit "$notary_zip" \
            --apple-id "$MACOS_NOTARY_APPLE_ID" \
            --password "$MACOS_NOTARY_APP_PASSWORD" \
            --team-id "$MACOS_TEAM_ID" \
            --wait
    fi

    xcrun stapler staple "$app_root"
    spctl --assess --type exec --verbose=4 "$app_root"
}

create_final_archive() {
    rm -f "$archive_path"
    ditto -c -k --sequesterRsrc --keepParent "$package_root" "$archive_path"
}

copy_app_bundle

if has_codesign_credentials; then
    assert_notarization_credentials
    trap cleanup_signing_material EXIT
    prepare_codesign_keychain
    sign_app_bundle
    notarize_app_bundle
fi

create_final_archive
echo "Packaged ${platform} artifact at ${archive_path}"
