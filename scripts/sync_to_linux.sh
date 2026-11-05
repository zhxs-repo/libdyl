#!/usr/bin/env bash
# 把 cargo workspace 的源码同步到 Linux 机器，用于跨平台验证（方案 9 的 step 0b/4：
# 同一套代码在 Linux 后端与另一个 FFmpeg major 上跑一次探针与硬解 A/B）。
#
# Git Bash 没有 rsync，所以走 tar-over-ssh。只拷 Cargo workspace 真正用到的东西：
# Windows 的 target/ 是几十 GB 的 PE 产物，dart/ 是 Flutter 侧的缓存，Linux 上用不到。
#
#   bash libdyl/scripts/sync_to_linux.sh [user@host] [远程目录]
#
# FFmpeg **不在这里拷**：本机 `FFMPEG_DIR`（vcpkg `x64-windows`）里是 PE 的 dll/diff，
# Linux 侧链不上；而且 step 0b 要的是"另一个 major"，不是把同一份 Windows 库搬过去。
# 远端自备，运行侧显式指过去：
#   FFMPEG_DIR=/usr/local/ffmpeg-8.1 cargo run -p dyl-ffmpeg --release --example hwaccel_probe
#   FFMPEG_DIR=/usr/local/ffmpeg-8.1 cargo test -p dyl-ffmpeg
# 探针会打印链接到的版本与 configure 里的硬件项，拿错版本会当场露出来。
set -euo pipefail

HOST="${1:-spark@192.168.1.9}"
REMOTE_ROOT="${2:-src/dyl-player-sync}"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

# Cargo workspace 的输入：crates 是成员，其余是测试夹具、脚本与链接库头文件
PATHS=(Cargo.toml Cargo.lock crates contract scripts docs .cargo .github dyl.h)
for extra in test_video.mp4 multitrack.mkv multitrack60.mkv hevc10_1080p.mkv; do
  [ -f "$extra" ] && PATHS+=("$extra")
done

echo "==> 同步源码 -> $HOST:~/$REMOTE_ROOT"
ssh "$HOST" "rm -rf ~/$REMOTE_ROOT && mkdir -p ~/$REMOTE_ROOT"
tar -cf - --exclude=target --exclude='*.pdb' "${PATHS[@]}" | ssh "$HOST" "tar -xf - -C ~/$REMOTE_ROOT"

echo "==> 完成。FFmpeg 请按本文件头部在远端自备，先跑 hwaccel_probe 核对版本与硬件项"
