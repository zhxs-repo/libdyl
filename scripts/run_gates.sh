#!/usr/bin/env bash
# Dyl 一键验证门禁（CI / 本地均可执行）
#
# 顺序执行 6 项检查，任何一步失败立即退出（非零码即门禁失败）：
#   1. 格式检查          2. clippy（含所有 target）
#   3. 全工作区测试（含 C ABI / overlay / header-sync 集成测试）
#   4. smoke_test（FFI 生命周期冒烟）
#   5. e2e_playback（真实播放回归：播放/暂停/seek/停止/退出）
#   6. seek_storm（1000 次拖动式 seek：合并语义与请求记账）
#
# 长播（soak）默认跳过，设置 DYL_SOAK_SECONDS 启用：
#   DYL_SOAK_SECONDS=120 bash scripts/run_gates.sh
#   2 小时验收：bash scripts/make_soak_asset.sh test_video.mp4 7200 long_2h.mp4
#                DYL_SOAK_SECONDS=7200 DYL_SOAK_ASSET=long_2h.mp4 bash scripts/run_gates.sh
#
# select_track 的真实文件回归（dyl-engine/tests/track_switch.rs）在第 3 步里跑，
# 素材不入库，缺了就整体跳过。要覆盖它就先生成：
#   bash scripts/make_multitrack_asset.sh
#
# 资产查找顺序与 crates/*/tests/common/mod.rs 一致：
#   DYL_TEST_ASSET_DIR → 仓库根（CI 在此生成）→ tmp_verify/（本机按 AGENTS.md 放这里）
#
# 用法: bash scripts/run_gates.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# 本机 Windows cargo 增量编译缓存易卡死，默认关闭增量
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

# T-02：MSVC 默认并行会打爆页面文件、报出假 ICE，Windows 上固定 -j 2。
# 其它平台沿用 cargo 自己的并行度；两边都可用 CARGO_BUILD_JOBS 覆盖。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" ;;
esac

mkdir -p tmp_verify
GATE_LOG="tmp_verify/gates.log"

# 链接失败时区分"PDB 损坏"与真编译错误：前者删掉日志点名的那一个 .pdb 即可，
# 整体 cargo clean 会白扔几个 GB 的增量产物（2026-11-06 实测踩过）。
run_cargo() {
  if cargo "$@" 2>&1 | tee "$GATE_LOG"; then
    return 0
  fi
  if grep -q "LNK1285" "$GATE_LOG"; then
    echo "HINT: LNK1285 = PDB 文件已损坏（多为并发 cargo 被中断留下）。相关行：" >&2
    grep -n "LNK1285" "$GATE_LOG" >&2 || true
    echo "      只删上面点名的那个 .pdb 再重跑，例如：" >&2
    echo "        rm target/debug/deps/dyl_subtitle-*.pdb" >&2
    echo "      不要 cargo clean 整个 target。" >&2
  fi
  return 1
}

# 与 Rust 侧同一个查找顺序；找不到返回非零，由调用方决定是 ERROR 还是 note。
# 一律回绝对路径：cargo run 的工作目录不保证是仓库根。MSYS/Git Bash 下用 pwd -W
# 取 Windows 形式（D:/...），否则原生 exe 拿到 /d/... 会打不开文件。
abs_dir() { (cd "$1" && { pwd -W 2>/dev/null || pwd; }); }

resolve_asset() {
  local name="$1" root
  root="$(abs_dir .)"
  local candidates=("$root/$name" "$root/tmp_verify/$name")
  if [[ -n "${DYL_TEST_ASSET_DIR:-}" && -d "$DYL_TEST_ASSET_DIR" ]]; then
    candidates=("$(abs_dir "$DYL_TEST_ASSET_DIR")/$name" "${candidates[@]}")
  fi
  local candidate
  for candidate in "${candidates[@]}"; do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

echo "==> [1/6] cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> [2/6] cargo clippy --workspace --all-targets -- -D warnings"
run_cargo clippy --workspace --all-targets -- -D warnings

echo "==> [3/6] cargo test --workspace"
if ! resolve_asset multitrack.mkv >/dev/null; then
  echo "    note: multitrack.mkv 缺失，track_switch 真实文件回归会跳过（bash scripts/make_multitrack_asset.sh）"
fi
run_cargo test --workspace

echo "==> [4/6] smoke_test (FFI lifecycle)"
run_cargo run --release --example smoke_test -p dyl-ffi

if ! TEST_ASSET="$(resolve_asset test_video.mp4)"; then
  echo "ERROR: test_video.mp4 缺失，无法执行真实播放回归" >&2
  echo "       查找顺序：DYL_TEST_ASSET_DIR → 仓库根 → tmp_verify/" >&2
  echo "       CI/新机器可用 ffmpeg 生成：见 .github/workflows/ci.yml 的 Generate test asset" >&2
  exit 1
fi

echo "==> [5/6] e2e_playback (real playback regression) [$TEST_ASSET]"
run_cargo run --release --example e2e_playback -p dyl-ffi -- "$TEST_ASSET"

echo "==> [6/6] seek_storm (coalescing + request accounting)"
run_cargo run --release --example seek_storm -p dyl-ffi -- "$TEST_ASSET" "${DYL_SEEK_STORM_COUNT:-1000}"

if [[ "${DYL_SOAK_SECONDS:-0}" -gt 0 ]]; then
  if [[ -n "${DYL_SOAK_ASSET:-}" ]]; then
    SOAK_ASSET="$(resolve_asset "$DYL_SOAK_ASSET")" || {
      echo "ERROR: DYL_SOAK_ASSET=$DYL_SOAK_ASSET 找不到（查找顺序同上）" >&2
      exit 1
    }
  else
    SOAK_ASSET="$TEST_ASSET"
  fi
  echo "==> [soak] $SOAK_ASSET for ${DYL_SOAK_SECONDS}s"
  run_cargo run --release --example soak -p dyl-ffi -- "$SOAK_ASSET" "${DYL_SOAK_SECONDS}"
else
  echo "==> [soak] 跳过（设置 DYL_SOAK_SECONDS=120 启用）"
fi

echo "==> ALL GATES PASSED"
