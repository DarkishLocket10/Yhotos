#!/usr/bin/env bash
# Extract digest-pinned static video parsers into this worktree, without installing packages.
set -euo pipefail
_video_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
_video_image='mwader/static-ffmpeg:9.0.2-1@sha256:f5d100184884eb3e82b0eb14bb5ebf98da99826ac824989720297fb9ef21c822'
_video_out="$_video_root/target/video-tools"
if [[ -f "$_video_out/image" ]] && [[ "$(cat -- "$_video_out/image")" == "$_video_image" ]] && [[ -x "$_video_out/ffmpeg" && -x "$_video_out/ffprobe" ]]; then
    exit 0
fi
docker pull "$_video_image"
_video_container="$(docker create --network=none "$_video_image")"
trap 'docker rm -f "$_video_container" >/dev/null' EXIT
mkdir -p -- "$_video_out"
docker cp "$_video_container:/ffmpeg" "$_video_out/ffmpeg"
docker cp "$_video_container:/ffprobe" "$_video_out/ffprobe"
printf '%s\n' "$_video_image" > "$_video_out/image"
