#!/usr/bin/env bash
# Installs the native libraries CI builds against.
# GitHub's Ubuntu runners resolve mirrors through /etc/apt/apt-mirrors.txt ("mirror+file:"
# sources). That mirror has served packages at 3-40 s each for whole runs, so:
#   1. packages downloaded by earlier runs are restored from ~/apt-cache (actions/cache),
#   2. each attempt is time-boxed, and
#   3. later attempts point the mirror list (and any plain http sources) at other official mirrors.
set -uo pipefail
pkgs=(libvips-dev libvips-tools liblcms2-dev libclang-dev libjxl-dev
      libheif-plugin-aomenc libheif-plugin-aomdec libheif-plugin-libde265)
opts=(-o Acquire::Retries=2 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20
      -o APT::Keep-Downloaded-Packages=true)
cache="${APT_CACHE_DIR:-$HOME/apt-cache}"
if [[ -n "$cache" && -d "$cache" ]] && compgen -G "$cache/*.deb" > /dev/null; then
  sudo cp "$cache"/*.deb /var/cache/apt/archives/ && echo "restored $(ls "$cache"/*.deb | wc -l) cached packages"
fi
attempt() {
  sudo timeout 240 apt-get "${opts[@]}" -q update &&
    sudo timeout 480 apt-get "${opts[@]}" -q install -y --no-install-recommends "${pkgs[@]}"
}
use_mirror() {
  local mirror="$1" f
  echo "switching apt to $mirror"
  [[ -f /etc/apt/apt-mirrors.txt ]] && echo "$mirror" | sudo tee /etc/apt/apt-mirrors.txt > /dev/null
  for f in /etc/apt/sources.list.d/ubuntu.sources /etc/apt/sources.list; do
    [[ -f "$f" ]] && sudo sed -i -E "s#http://(azure\.archive\.ubuntu\.com|archive\.ubuntu\.com|mirrors\.edge\.kernel\.org)/ubuntu/?#$mirror/#g" "$f"
  done
}
ok=1
attempt && ok=0
if (( ok != 0 )); then
  for mirror in http://archive.ubuntu.com/ubuntu http://mirrors.edge.kernel.org/ubuntu; do
    use_mirror "$mirror"
    attempt && { ok=0; break; }
  done
fi
if (( ok == 0 )) && [[ -n "$cache" ]]; then
  mkdir -p "$cache" && cp /var/cache/apt/archives/*.deb "$cache"/ 2>/dev/null
  echo "cached $(ls "$cache"/*.deb 2>/dev/null | wc -l) packages for later runs"
fi
(( ok == 0 )) || { echo "apt install failed on every mirror" >&2; exit 1; }
