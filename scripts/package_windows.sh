#!/usr/bin/env bash
# 打 Windows 发布包：dyl-player.exe + 运行时真正需要的 DLL -> dist/dyl-player-windows-x64/
#
# 用法（在 libdyl 目录或其父目录均可）:
#   bash scripts/package_windows.sh                # cargo build --release 后复制
#   bash scripts/package_windows.sh --no-build     # 直接用现成的 target/release/dyl-player.exe
#   bash scripts/package_windows.sh --zip          # 额外压成 dist/dyl-player-windows-x64.zip
#   DYL_FFMPEG_DIR=/d/Devs/vcpkg/installed/x64-windows bash scripts/package_windows.sh
#
# 为什么要带这些 DLL：dyl-ffmpeg 链的是 FFmpeg 共享构建（FFMPEG_DIR/bin/*.dll），
# exe 里只有导入表引用，DLL 不会内嵌。缺任何一个都是「找不到指定模块」，
# 而且 Rust 侧不会报错——是加载器在进程启动前就拒绝了。
#
# 需要哪几个不写死在脚本里：从 dyl-player.exe 的导入表按 soname（avcodec-62.dll 这种）
# 做传递闭包。av* / sw* 家族以外的系统 DLL（KERNEL32、opengl32、dwmapi…）不复制。
# 闭包里最容易漏的是 swresample-N.dll：没有任何一层直接导入它，只有 avcodec 导入。
# dav1d.dll 是 AV1 软解解码器（FFmpeg 的 libdav1d 依赖它），不带版本号，单独列出。
#
# 闭包之外还补 VCRUNTIME140.dll：FFmpeg DLL import 它，而它不是 Windows 自带的
# （本机的 System32 副本来自 rustup 顺手装的 VC++ 运行库），干净机器上没有。
#
# 最后用 --help 路径自证：清空 PATH 里的 FFmpeg 目录，在 dist 里真跑一次，
# 退出码非 0 就是包没打全。
set -euo pipefail
cd "$(dirname "$0")/.."

BUILD=1
ZIP=0
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    --zip) ZIP=1 ;;
    *) echo "ERROR: 未知参数 $arg（支持 --no-build / --zip）" >&2; exit 2 ;;
  esac
done

REPO=$(pwd)
EXE="$REPO/target/release/dyl-player.exe"
DIST="$REPO/dist/dyl-player-windows-x64"

# ── 1. 构建 ────────────────────────────────────────────────────────────
if [[ "$BUILD" = 1 ]]; then
  export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
  echo "==> cargo build --release -p dyl-player"
  cargo build --release -p dyl-player
fi
[[ -f "$EXE" ]] || {
  echo "ERROR: 找不到 $EXE（先构建，或去掉 --no-build）" >&2
  exit 1
}

# ── 2. 定位链接时用的那套 FFmpeg DLL ───────────────────────────────────
# 必须和构建时 FFMPEG_DIR 指向同一套：soname 号是 ABI 编号，混用不同构建会加载失败。
find_ffmpeg_bin() {
  local dir
  for dir in "${DYL_FFMPEG_DIR:-}" "${FFMPEG_DIR:-}"; do
    [[ -n "$dir" ]] || continue
    if compgen -G "$(cygpath -u "$dir")/bin/*.dll" >/dev/null 2>&1; then
      echo "$(cygpath -u "$dir")/bin"; return 0
    elif compgen -G "$dir/bin/*.dll" >/dev/null 2>&1; then
      echo "$dir/bin"; return 0
    fi
  done
  # 退而求其次：PATH 里能找到的 avformat
  local d hit
  for d in ${PATH//:/ }; do
    hit=$(ls "$d"/avformat-*.dll 2>/dev/null | head -1 || true)
    [[ -n "$hit" ]] && { echo "$d"; return 0; }
  done
  return 1
}

FFMPEG_BIN=$(find_ffmpeg_bin) || {
  echo "ERROR: 找不到 FFmpeg 共享库目录。设 DYL_FFMPEG_DIR（或 FFMPEG_DIR）为构建时那套：" >&2
  echo "       例 DYL_FFMPEG_DIR=/d/Devs/vcpkg/installed/x64-windows" >&2
  exit 1
}
[[ -d "$FFMPEG_BIN" ]] || { echo "ERROR: FFmpeg bin 不存在: $FFMPEG_BIN" >&2; exit 1; }
echo "==> FFmpeg DLL 来源: $FFMPEG_BIN"

# ── 3. 按 soname 求 av*/sw* 的传递闭包 ─────────────────────────────────
# 直接 grep 导入表里的 ASCII 名字即可，够用且不需要 PE 解析器；
# 真正的裁决是第 6 步的加载测试。
sonames() {
  grep -a -o -i -E '(av|sw)[a-z]+-[0-9]+\.dll|dav1d\.dll' "$1" 2>/dev/null | tr 'A-Z' 'a-z' | sort -u
}

QUEUE=()
while read -r name; do QUEUE+=("$name"); done < <(sonames "$EXE")
[[ ${#QUEUE[@]} -gt 0 ]] || {
  echo "ERROR: 没从 $EXE 里读到任何 av*/sw* 导入，二进制不像 MSVC 产物？" >&2
  exit 1
}

NEEDED=()
declare -A SEEN=()
while [[ ${#QUEUE[@]} -gt 0 ]]; do
  name="${QUEUE[0]}"
  QUEUE=("${QUEUE[@]:1}")
  [[ -n "${SEEN[$name]:-}" ]] && continue
  SEEN[$name]=1
  src="$FFMPEG_BIN/$name"
  [[ -f "$src" ]] || {
    echo "ERROR: 依赖 $name 在 $FFMPEG_BIN 里找不到（构建用的 FFmpeg 和这套不一致？）" >&2
    exit 1
  }
  NEEDED+=("$name")
  while read -r dep; do QUEUE+=("$dep"); done < <(sonames "$src")
done
unset SEEN

# ── 4. VCRUNTIME140.dll ────────────────────────────────────────────────
pick_vcruntime() {
  local hit
  # 优先官方 redist 副本，其次 System32（本机那份来自 rustup 装的运行库，同一支 DLL）
  hit=$(find "/c/Program Files (x86)/Microsoft Visual Studio" "/c/Program Files/Microsoft Visual Studio" \
        -ipath '*/VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT/vcruntime140.dll' 2>/dev/null | sort | tail -1 || true)
  [[ -n "$hit" ]] && { echo "$hit"; return 0; }
  hit="${WINDIR:-C:\\Windows}"
  hit=$(cygpath -u "$hit")/System32/vcruntime140.dll
  [[ -f "$hit" ]] && { echo "$hit"; return 0; }
  return 1
}

# ── 5. 复制到 dist ─────────────────────────────────────────────────────
rm -rf "$DIST"
mkdir -p "$DIST"
cp "$EXE" "$DIST/"
for name in "${NEEDED[@]}"; do cp "$FFMPEG_BIN/$name" "$DIST/"; done
echo "==> 已复制: $(basename "$EXE") ${NEEDED[*]}"

VCRUNTIME=$(pick_vcruntime) && {
  cp "$VCRUNTIME" "$DIST/vcruntime140.dll"
  echo "==> 已复制: vcruntime140.dll  (来源 $VCRUNTIME)"
} || {
  echo "WARN: 本机找不到 vcruntime140.dll；FFmpeg DLL 依赖它。" >&2
  echo "      要么装上 VC++ 2015-2022 (x64) 运行库再重跑，要么告知用户自行安装。" >&2
}

# ── 6. 自证：PATH 去掉 FFmpeg 目录，在 dist 里真跑一次 ──────────────────
# 加载器搜索顺序是「exe 同目录 → System32 → PATH」，所以 PATH 里绝不能留着 FFmpeg，
# 否则缺一个 DLL 也能被 PATH 救活，测不出真实发布包的状态。
WIN=$(cygpath -u "${WINDIR:-C:\\Windows}")
CLEAN_PATH="$WIN/System32:$WIN:/usr/bin"
if (cd "$DIST" && PATH="$CLEAN_PATH" ./dyl-player.exe --help >/dev/null 2>&1); then
  echo "==> 加载测试通过：dist 目录内的依赖闭包完整（--help 正常退出）"
else
  echo "ERROR: dist 里的 exe 跑不起来（多半是缺 DLL）。检查上面复制清单是否覆盖导入表。" >&2
  exit 1
fi
# vcruntime 只有 System32 那份在兜底时，测试也会通过，所以要单独说明。
[[ -f "$DIST/vcruntime140.dll" ]] || {
  echo "NOTE: dist 内没有 vcruntime140.dll，上面的测试是靠本机 System32 通过的；" >&2
  echo "      干净机器需要用户自己装 VC++ 运行库。" >&2
}

# ── 7. 汇总 ────────────────────────────────────────────────────────────
echo
echo "==> $DIST"
ls -la "$DIST" | awk 'NR>1 && $NF!="." && $NF!=".." {printf "    %-24s %8.1f KB\n", $NF, $5/1024}'
echo "    合计 $(du -sh "$DIST" | cut -f1)"

if [[ "$ZIP" = 1 ]]; then
  command -v powershell >/dev/null || { echo "ERROR: --zip 需要 powershell" >&2; exit 1; }
  ARC="$REPO/dist/dyl-player-windows-x64.zip"
  rm -f "$ARC"
  powershell -NoProfile -Command "Compress-Archive -Path '$(cygpath -w "$DIST")\*' -DestinationPath '$(cygpath -w "$ARC")' -Force" >/dev/null
  echo "==> $(basename "$ARC") ($(du -h "$ARC" | cut -f1))"
fi

echo
echo "发给用户前还剩三件事（脚本没法替你判断）："
echo "  1. 中文字体：内核只读 %WINDIR%\\Fonts（微软雅黑等），不读程序目录，"
echo "     精简/LTSC 无中文字体的机器上字幕和弹幕会是豆腐块。"
echo "  2. 双击会弹黑框：exe 是控制台子系统，要干净发布就给 dyl-player 加 windows_subsystem。"
echo "  3. FFmpeg 是 LGPL 构建：随包附许可证与源码获取途径。"
