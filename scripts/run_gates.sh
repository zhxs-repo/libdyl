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
# 用法: bash scripts/run_gates.sh
set -euo pipefail
cd "$(dirname "$0")/.."

# 本机 Windows cargo 增量编译缓存易卡死，默认关闭增量
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"

echo "==> [1/6] cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> [2/6] cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> [3/6] cargo test --workspace"
if [[ ! -f multitrack.mkv ]]; then
  echo "    note: multitrack.mkv 缺失，track_switch 真实文件回归会跳过（bash scripts/make_multitrack_asset.sh）"
fi
cargo test --workspace

echo "==> [4/6] smoke_test (FFI lifecycle)"
cargo run --release --example smoke_test -p dyl-ffi

if [[ ! -f test_video.mp4 ]]; then
  echo "ERROR: test_video.mp4 缺失，无法执行真实播放回归" >&2
  echo "       CI/新机器可用 ffmpeg 生成：见 .github/workflows/ci.yml 的 Generate test asset" >&2
  exit 1
fi

echo "==> [5/6] e2e_playback (real playback regression)"
cargo run --release --example e2e_playback -p dyl-ffi -- test_video.mp4

echo "==> [6/6] seek_storm (coalescing + request accounting)"
cargo run --release --example seek_storm -p dyl-ffi -- test_video.mp4 "${DYL_SEEK_STORM_COUNT:-1000}"

if [[ "${DYL_SOAK_SECONDS:-0}" -gt 0 ]]; then
  echo "==> [soak] ${DYL_SOAK_ASSET:-test_video.mp4} for ${DYL_SOAK_SECONDS}s"
  cargo run --release --example soak -p dyl-ffi -- \
    "${DYL_SOAK_ASSET:-test_video.mp4}" "${DYL_SOAK_SECONDS}"
else
  echo "==> [soak] 跳过（设置 DYL_SOAK_SECONDS=120 启用）"
fi

echo "==> ALL GATES PASSED"
