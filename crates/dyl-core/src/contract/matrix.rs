use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{CommandOp, CommandStatus, ErrorInfo, PlayerState};

/// `contract/state_matrix.json` 的类型化镜像。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateMatrix {
    pub schema: u32,
    pub states: Vec<PlayerState>,
    pub notes: BTreeMap<String, String>,
    pub reject: RejectSpec,
    pub matrix: BTreeMap<CommandOp, BTreeMap<PlayerState, MatrixCell>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectSpec {
    pub channel: String,
    pub status: CommandStatus,
    pub error: ErrorInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixCell {
    pub status: MatrixStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<TransitionTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancels_inflight_load: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixStatus {
    Accepted,
    Rejected,
}

/// 命令落点：`"same"`、单个状态、或 `load_file` 的候选状态数组。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransitionTarget {
    Same(SameTarget),
    One(PlayerState),
    Many(Vec<PlayerState>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameTarget {
    #[serde(rename = "same")]
    Same,
}

impl StateMatrix {
    pub fn cell(&self, op: CommandOp, state: PlayerState) -> Option<&MatrixCell> {
        self.matrix.get(&op).and_then(|row| row.get(&state))
    }

    pub fn is_accepted(&self, op: CommandOp, state: PlayerState) -> bool {
        self.cell(op, state)
            .is_some_and(|cell| cell.status == MatrixStatus::Accepted)
    }

    /// 返回所有缺失的 (op, state) 组合；用于契约完整性测试。
    pub fn missing_cells(&self) -> Vec<(CommandOp, PlayerState)> {
        let mut missing = Vec::new();
        for op in CommandOp::ALL {
            for state in PlayerState::ALL {
                if self.cell(op, state).is_none() {
                    missing.push((op, state));
                }
            }
        }
        missing
    }
}
