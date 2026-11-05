//! 单一状态转换表 — 直接解析 `contract/state_matrix.json`
//!
//! 命令校验与落点共用同一份数据，避免 Rust 侧再抄一张表后与契约漂移。

use std::sync::OnceLock;

use dyl_core::contract::{CommandOp, PlayerState, StateMatrix};

const MATRIX_JSON: &str = include_str!("../../../contract/state_matrix.json");

fn matrix() -> Option<&'static StateMatrix> {
    static MATRIX: OnceLock<Option<StateMatrix>> = OnceLock::new();
    MATRIX
        .get_or_init(|| match serde_json::from_str(MATRIX_JSON) {
            Ok(matrix) => Some(matrix),
            Err(e) => {
                tracing::error!("state matrix parse failed: {e}");
                None
            }
        })
        .as_ref()
}

/// 当前状态下命令是否被接受
pub(crate) fn accepts(op: CommandOp, state: PlayerState) -> bool {
    matrix().is_some_and(|m| m.is_accepted(op, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_parses_and_covers_every_cell() {
        let matrix = matrix().expect("contract state matrix must parse");
        assert!(matrix.missing_cells().is_empty());
    }

    #[test]
    fn seeks_are_rejected_while_loading() {
        assert!(!accepts(CommandOp::Seek, PlayerState::Loading));
        assert!(accepts(CommandOp::Seek, PlayerState::Playing));
        assert!(accepts(CommandOp::Stop, PlayerState::Loading));
    }
}
