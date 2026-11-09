//! 测试资产解析：`DYL_TEST_ASSET_DIR` → 仓库根 → `tmp_verify/`。
//!
//! 与 `dyl-ffmpeg/tests/common` 同一份约定（两个 crate 各留一份，避免为测试辅助
//! 往 `dyl-core` 的公共面上加东西）。CI 在仓库根生成资产，本机按 AGENTS.md 放
//! `tmp_verify/`；缺资产一律 SKIP 并打印生成命令，不 panic —— 资产存在却打不开
//! 才是真缺陷。

// 每个测试 target 只用到其中一部分，另一部分在该 target 里就是 dead_code
#![allow(dead_code)]

use std::path::PathBuf;

pub const HOW_MULTITRACK: &str = "运行 bash scripts/make_multitrack_asset.sh 生成";

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
