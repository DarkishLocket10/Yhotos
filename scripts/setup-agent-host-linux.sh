#!/usr/bin/env bash
# One-time setup for a Linux (Ubuntu 24.04) agent host. Run it yourself; it asks for sudo.
# Installs the same native libraries the CI codec and test jobs use, plus the C toolchain.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y \
  build-essential pkg-config clang libclang-dev libssl-dev \
  libvips-dev libvips-tools liblcms2-dev libjxl-dev \
  libheif-plugin-aomenc libheif-plugin-aomdec libheif-plugin-libde265
pkg-config --modversion vips-cpp openssl
echo "agent host ready"
