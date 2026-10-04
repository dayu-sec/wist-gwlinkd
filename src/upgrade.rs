//! 升级闭环：拉 desired → 幂等 → 驱动执行器（`gops prj upgrade`）→ 回执。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归 `gops`）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。

use std::path::PathBuf;

/// 缺省升级执行器程序名。
pub const DEFAULT_UPGRADER_PROGRAM: &str = "gops";

/// 升级驱动：把一次「升到某版本」交给执行器，并在状态目录记账 + 心跳。
#[derive(Debug, Clone)]
pub struct UpgradeDriver {
    /// 执行器程序（如 `gops`）。
    pub program: String,
    /// 状态目录（写 `crate::state::UPGRADE_RECORD_FILE` 与心跳）。
    pub state_dir: PathBuf,
}

impl UpgradeDriver {
    /// 建驱动。
    pub fn new(program: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            state_dir: state_dir.into(),
        }
    }

    // TODO(④): 调 `{program} prj upgrade --to <version>`；写 upgrade.json 与心跳；
    //          幂等（同 plan_id + gateway_id 不重复执行）；回执 `upgrade-result`。
}
