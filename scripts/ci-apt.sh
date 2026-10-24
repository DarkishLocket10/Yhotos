#!/usr/bin/env bash
# Installs the native libraries CI builds against. The default Azure Ubuntu mirror has stalled
# for 10+ minutes on some runs, so each attempt is time-boxed and later attempts switch to other
# official mirrors.
set -uo pipefail
pkgs=(libvips-dev libvips-tools liblcms2-dev libclang-dev libjxl-dev
      libheif-plugin-aomenc libheif-plugin-aomdec libheif-plugin-libde265 "$@")
opts=(-o Acquire::Retries=2 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20)
attempt() {
  sudo timeout 300 apt-get "${opts[@]}" -q update &&
    sudo timeout 300 apt-get "${opts[@]}" -q install -y --no-install-recommends "${pkgs[@]}"
}
attempt && exit 0
for mirror in http://archive.ubuntu.com/ubuntu http://mirrors.edge.kernel.org/ubuntu; do
  echo "apt attempt failed; switching to $mirror"
  for f in /etc/apt/sources.list.d/ubuntu.sources /etc/apt/sources.list; do
    [ -f "$f" ] && sudo sed -i -E "s#http://(azure\.archive\.ubuntu\.com|archive\.ubuntu\.com|mirrors\.edge\.kernel\.org)/ubuntu/?#$mirror/#g" "$f"
  done
  attempt && exit 0
done
echo "apt install failed on every mirror" >&2
exit 1
