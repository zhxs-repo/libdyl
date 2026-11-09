//! 测试资产解析：`DYL_TEST_ASSET_DIR` → 仓库根 → `tmp_verify/`。
//!
//! 为什么需要它：CI 在仓库根生成资产，本机按 AGENTS.md 的约定放在 `tmp_verify/`，
//! 而各测试原先硬写 `../../<name>`。结果是 CI 绿、本地红，且缺资产时有的测试
//! SKIP、有的直接 panic —— 同一件事两种结论，门禁就没法当判据用。
//!
//! 缺资产统一走 [`asset_or_skip`]：打印可执行的生成命令后返回 `None`，
//! 调用方 `let Some(p) = … else { return };`。资产存在但打不开仍然是失败，
//! 那才是真缺陷。

// 每个测试 target 只用到其中一部分，另一部分在该 target 里就是 dead_code
#![allow(dead_code)]

use std::path::PathBuf;

/// 生成命令提示：各测试传入自己资产的来源，SKIP 信息才有可操作性
pub const HOW_TEST_VIDEO: &str = "CI 见 .github/workflows/ci.yml 的 Generate test asset；本机可用 ffmpeg 生成 testsrc2+sine 的 5s 样本";
pub const HOW_SAR: &str = "需要 320x320 编码、SAR 9:16 的样本（显示尺寸应为 180x320）";
pub const HOW_AV1: &str = "需要 AV1 样本，且本构建的 FFmpeg 带 libdav1d / libaom-av1";

pub fn asset(name: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DYL_TEST_ASSET_DIR") {
        let p = PathBuf::from(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for dir in [root.clone(), root.join("tmp_verify")] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 找到就返回路径字符串（FFmpeg 接口吃 &str），找不到打印 SKIP 并返回 None
pub fn asset_or_skip(name: &str, how: &str) -> Option<String> {
    match asset(name) {
        Some(p) => Some(p.to_string_lossy().into_owned()),
        None => {
            eprintln!("SKIP: 缺少 {name}（{how}）");
            None
        }
    }
}
