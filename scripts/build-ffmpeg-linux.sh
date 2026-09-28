#!/usr/bin/env bash
# Build the shared FFmpeg bundled with the Linux server.
#
# Distro FFmpeg on the CI image (Ubuntu 24.04: 6.1) has no Vulkan Video
# encoders, which the server needs on NVIDIA to keep encoding while a game
# saturates the GPU. libva/libdrm/libvulkan stay dynamic against the host
# (package-unix.sh never bundles them), so VAAPI keeps using the host driver
# stack.
#
# Usage: scripts/build-ffmpeg-linux.sh <install-prefix>
set -euo pipefail

prefix="${1:?usage: build-ffmpeg-linux.sh <install-prefix>}"
ffmpeg_version="${FFMPEG_SOURCE_VERSION:-8.1}"
vulkan_headers_tag="${VULKAN_HEADERS_TAG:-v1.4.364}"
nv_codec_tag="${NV_CODEC_HEADERS_TAG:-n13.0.19.0}"

mkdir -p "$prefix"
prefix="$(cd "$prefix" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --quiet --depth 1 --branch "$vulkan_headers_tag" \
    https://github.com/KhronosGroup/Vulkan-Headers "$work/vulkan-headers"
cmake -S "$work/vulkan-headers" -B "$work/vulkan-headers/build" \
    -DCMAKE_INSTALL_PREFIX="$prefix" >/dev/null
cmake --install "$work/vulkan-headers/build" >/dev/null

# The distro loader's vulkan.pc reports its older header version; describe the
# fresh headers and link the host loader.
vk_version="${vulkan_headers_tag#v}"
mkdir -p "$prefix/lib/pkgconfig"
cat >"$prefix/lib/pkgconfig/vulkan.pc" <<EOF
prefix=$prefix
includedir=\${prefix}/include

Name: Vulkan-Loader
Description: Vulkan Loader
Version: $vk_version
Libs: -lvulkan
Cflags: -I\${includedir}
EOF

git clone --quiet --depth 1 --branch "$nv_codec_tag" \
    https://github.com/FFmpeg/nv-codec-headers "$work/nv-codec-headers"
make -C "$work/nv-codec-headers" PREFIX="$prefix" install >/dev/null

curl -fsSL "https://ffmpeg.org/releases/ffmpeg-${ffmpeg_version}.tar.xz" \
    | tar -xJ -C "$work"
cd "$work/ffmpeg-${ffmpeg_version}"
PKG_CONFIG_PATH="$prefix/lib/pkgconfig:$prefix/share/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
./configure \
    --prefix="$prefix" \
    --enable-shared \
    --disable-static \
    --enable-gpl \
    --enable-libx264 \
    --enable-libopus \
    --enable-libdrm \
    --enable-vaapi \
    --enable-vulkan \
    --enable-ffnvcodec \
    --enable-nvenc \
    --disable-programs \
    --disable-doc \
    --disable-debug \
    --extra-cflags="-I$prefix/include"
make -j"$(nproc)"
make install

for encoder in h264_vulkan hevc_vulkan hevc_nvenc h264_vaapi libx264 libopus; do
    grep -q "extern const FFCodec ff_${encoder}_encoder" libavcodec/codec_list.c \
        || grep -q "&ff_${encoder}_encoder" libavcodec/codec_list.c \
        || { echo "FFmpeg build is missing ${encoder}" >&2; exit 1; }
done
echo "FFmpeg ${ffmpeg_version} installed to $prefix"
