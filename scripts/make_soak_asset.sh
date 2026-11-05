#!/usr/bin/env bash
# 生成长播验收素材：把源片段用 -c copy 串接成指定时长（不重编码，几十秒完成）
#
# 用法: bash scripts/make_soak_asset.sh [源文件] [秒数] [输出文件]
#   bash scripts/make_soak_asset.sh test_video.mp4 7200 long_2h.mp4
#
# 产物不入库（见 .gitignore），只用于 soak:
#   cargo run --release --example soak -p dyl-ffi -- long_2h.mp4 7200
set -euo pipefail
cd "$(dirname "$0")/.."

SRC="${1:-test_video.mp4}"
TOTAL_SECONDS="${2:-7200}"
OUT="${3:-long_2h.mp4}"

[[ -f "$SRC" ]] || {
  echo "ERROR: 源文件不存在: $SRC" >&2
  exit 1
}
command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null || {
  echo "ERROR: 需要 ffmpeg / ffprobe 在 PATH 中" >&2
  exit 1
}

SRC_SECONDS=$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$SRC")
COPIES=$(awk -v total="$TOTAL_SECONDS" -v src="$SRC_SECONDS" \
  'BEGIN { n = int((total + src - 1) / src); print n < 1 ? 1 : n }')

# concat demuxer 的路径相对清单文件解析，所以必须写绝对路径；
# ffmpeg 是原生 Windows 程序，Git Bash 的 /d/... 不认，优先转成 Windows 路径
ABS_SRC="$(cd "$(dirname "$SRC")" && pwd)/$(basename "$SRC")"
if command -v cygpath >/dev/null 2>&1; then
  ABS_SRC=$(cygpath -w "$ABS_SRC")
fi
ABS_SRC=${ABS_SRC//\\//}

LIST=$(mktemp)
trap 'rm -f "$LIST"' EXIT
for _ in $(seq 1 "$COPIES"); do
  printf "file '%s'\n" "$ABS_SRC" >>"$LIST"
done

echo "==> 源 ${SRC_SECONDS}s x ${COPIES} = ${TOTAL_SECONDS}s -> ${OUT}"
ffmpeg -hide_banner -loglevel warning -y \
  -f concat -safe 0 -i "$LIST" \
  -t "$TOTAL_SECONDS" -c copy -movflags +faststart \
  "$OUT"

RESULT_SECONDS=$(ffprobe -v error -show_entries format=duration -of default=nw=1:nk=1 "$OUT")
echo "==> 完成: ${OUT} (${RESULT_SECONDS}s, $(du -h "$OUT" | cut -f1))"
