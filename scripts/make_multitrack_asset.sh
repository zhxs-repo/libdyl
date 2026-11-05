#!/usr/bin/env bash
# 生成多轨验收素材：1 视频 + 2 音轨 + 2 文本字幕轨（Matroska）
#
# 产物不入库，只用于 select_track 的真实文件回归：
#   bash scripts/make_multitrack_asset.sh [源文件] [输出文件]
#   cargo test -p dyl-engine --test track_switch
#   DYL_MULTITRACK_ASSET=/path/to/multitrack.mkv cargo test -p dyl-engine --test track_switch
#
# 三条刻意的设计，回归断言就靠它们：
#   - Matroska 容器：音频轨的 timescale 恒为 1/1000，切轨后重开管线才能复现
#     「按 packet 时间基打时间戳」这条不变量（用 1/sample_rate 会把管线拖死）。
#   - 第二条音轨换成 44.1k 单声道 FLAC：切轨必须重新 init 音频设备，
#     参数没跟着换就会听到变速/变调或根本不出声。
#   - 字幕给 ASS + SRT 两条文本轨：位图字幕由 FFmpeg 之外的路径生成，代价太大，
#     而 `resolve_track` 拒绝位图轨的逻辑另有单元测试覆盖。
set -euo pipefail
cd "$(dirname "$0")/.."

SRC="${1:-test_video.mp4}"
OUT="${2:-multitrack.mkv}"

[[ -f "$SRC" ]] || {
  echo "ERROR: 源文件不存在: $SRC" >&2
  exit 1
}
command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null || {
  echo "ERROR: 需要 ffmpeg / ffprobe 在 PATH 中" >&2
  exit 1
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# 两条字幕轨共用同一句台词，但用不同的样式标记，方便肉眼区分切轨是否真的生效
cat >"$WORK/track_ass.ass" <<'ASS'
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,28,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,ASS track one
Dialogue: 0,0:00:02.50,0:00:03.50,Default,,0,0,0,,ASS track two
ASS

cat >"$WORK/track_srt.srt" <<'SRT'
1
00:00:01,000 --> 00:00:02,000
SRT track one

2
00:00:02,500 --> 00:00:03,500
SRT track two
SRT

echo "==> ${SRC} -> ${OUT} (1 video + 2 audio + 2 text subtitle)"
# 第二条音轨先单独转码：`-af` 与 `-c` 的流选择器会同时命中多条音轨，
# 「一条 copy 一条重编码」写在同一条命令里要么互相覆盖，要么把 filter 加到 copy 上
ffmpeg -hide_banner -loglevel warning -y \
  -i "$SRC" -map 0:a:0 \
  -af "aformat=sample_fmts=s16:sample_rates=44100:channel_layouts=mono" \
  -c:a flac "$WORK/second_audio.flac"

ffmpeg -hide_banner -loglevel warning -y \
  -i "$SRC" -i "$WORK/second_audio.flac" \
  -i "$WORK/track_ass.ass" -i "$WORK/track_srt.srt" \
  -map 0:v:0 -map 0:a:0 -map 1:0 -map 2:0 -map 3:0 \
  -c copy \
  -metadata:s:v:0 language=eng -metadata:s:v:0 title="video" \
  -metadata:s:a:0 language=eng -metadata:s:a:0 title="original" \
  -metadata:s:a:1 language=jpn -metadata:s:a:1 title="commentary" \
  -metadata:s:s:0 language=eng -metadata:s:s:0 title="ass" \
  -metadata:s:s:1 language=spa -metadata:s:s:1 title="srt" \
  -disposition:a:0 default -disposition:a:1 0 \
  -disposition:s:0 default -disposition:s:1 0 \
  "$OUT"

echo "==> streams:"
ffprobe -v error -show_entries stream=index,codec_type,codec_name:stream_tags=language,title \
  -of default=nw=1 "$OUT"
echo "==> 完成: ${OUT} ($(du -h "$OUT" | cut -f1))"
