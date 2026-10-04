//! 升级闭环：拉 desired → 幂等 → 驱动执行器（`gops prj upgrade`）→ 回执。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归 `gops`）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。

use std::path::PathBuf;
use std::process::Stdio;

use crate::state::{self, UpgradeRecord};

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

    /// 驱动一次升级：写 `running` 记录 + 初始心跳，起执行器 `{program} prj upgrade --to <version>`。
    ///
    /// 返回后升级在后台进行；执行器（宿主侧）应周期 [`state::touch_heartbeat`]，超时无心跳即判死
    /// （[`state::upgrader_is_declared_dead`]）。**执行器不被本进程托管**（跨重启）。
    pub async fn start(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
    ) -> Result<(), String> {
        let record = UpgradeRecord {
            work_id: work_id.to_string(),
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            step: "fetch".to_string(),
            status: "running".to_string(),
            detail: String::new(),
        };
        state::write_upgrade_record(&self.state_dir, &record)?;
        state::touch_heartbeat(&self.state_dir)?;

        let mut child = tokio::process::Command::new(&self.program)
            .args(["prj", "upgrade", "--to", to_version])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("启动升级执行器 {} 失败: {err}", self.program))?;

        // 回收子进程，避免僵尸（升级跨越本进程重启由执行器自身保证，不依赖此等待）。
        tokio::spawn(async move {
            let _ = child.wait().await;
            // TODO(④): 等完后写终态到 upgrade.json 并回执 `upgrade-result` 给中心。
        });
        Ok(())
    }
}
