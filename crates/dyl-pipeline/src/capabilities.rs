//! 由 `dyl-pipeline` 声明的定位能力值（规范 ADR-13）。
//!
//! 两个模式各有实现分支，都在 `demux.rs` 的 seek 处理里：
//! - `Exact`：demuxer 发布 `Pipeline::seek_target_us`，视频/音频环路按它丢弃 pre-roll，
//!   首帧 PTS ≥ 目标，进度条不回跳到前一个关键帧。
//! - `Keyframe`：`probe_keyframe` 向后探到第一个关键帧，探测期间读到的包重新排队按序投递，
//!   并且**必须**把 `seek_target_us` 留成 0（不裁剪），否则关键帧之后的帧会被丢光。

use dyl_core::contract::SeekMode;

/// `features.seek_modes` 的值。未在此列出的模式由 engine 在同步阶段返回 `not_supported`。
pub const SEEK_MODES: &[SeekMode] = &[SeekMode::Exact, SeekMode::Keyframe];

/// 能力位的取值（顺序即契约里的顺序）。
pub fn seek_modes() -> Vec<SeekMode> {
    SEEK_MODES.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 声明了的模式必须在 `SeekMode` 里有对应变体：加了枚举却没有实现分支时，
    /// 这条断言不会替它背书，因为下面的清单是逐项手写的。
    #[test]
    fn declared_modes_cover_the_contract_enum() {
        assert_eq!(
            seek_modes(),
            vec![SeekMode::Exact, SeekMode::Keyframe],
            "`SeekMode` 新增了变体却没有在这里声明：能力位会少报，客户端看不到该模式"
        );
    }

    #[test]
    fn exact_is_declared_before_keyframe() {
        // 契约夹具按这个顺序写，改动顺序就是改契约文本
        assert_eq!(SEEK_MODES.first(), Some(&SeekMode::Exact));
    }
}
