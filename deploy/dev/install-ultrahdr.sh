#!/usr/bin/env bash
# Codec/CI image build only. Runtime has no network and never downloads libraries.
set -euo pipefail
build_dir="$(mktemp -d)"
trap 'rm -rf -- "$build_dir"' EXIT
curl --fail --silent --show-error --location \
    https://codeload.github.com/google/libultrahdr/tar.gz/refs/tags/v2.0.0 \
    --output "$build_dir/source.tar.gz"
printf '%s  %s\n' 39a64a17c48f2f8a68b653b76663ab14ce357cb5ca5acaacd89e6bc97d2f409f "$build_dir/source.tar.gz" | sha256sum --check --status
tar -xzf "$build_dir/source.tar.gz" -C "$build_dir"
# Native XMP output rejects per-channel coefficients; hdr_jpeg::dual writes
# both ISO and per-channel Adobe metadata after the native ISO-only mux.
cmake -S "$build_dir/libultrahdr-2.0.0" -B "$build_dir/build" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="${1:-/usr/local}" \
    -DBUILD_SHARED_LIBS=ON -DUHDR_BUILD_DEPS=OFF -DUHDR_ENABLE_HEIF=OFF \
    -DUHDR_BUILD_EXAMPLES=OFF -DUHDR_BUILD_TESTS=OFF -DUHDR_ENABLE_GLES=OFF \
    -DUHDR_WRITE_XMP=OFF -DUHDR_WRITE_ISO=ON
cmake --build "$build_dir/build" --parallel 2
cmake --install "$build_dir/build"
ldconfig
