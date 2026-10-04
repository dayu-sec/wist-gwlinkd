//! 升级闭环：拉 desired → 幂等 → 驱动执行器（`gops prj upgrade`）→ **回执**。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归 `gops`）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use wist_control::GatewayCredentialBundle;

use crate::center::CenterClient;
use crate::state::{self, UpgradeRecord};

/// 缺省升级执行器程序名。
pub const DEFAULT_UPGRADER_PROGRAM: &str = "gops";

/// 升级期间的心跳刷新周期（远小于 [`state::UPGRADER_DEAD_AFTER`]）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// 升级回执的投递方（中心客户端 + 运行期凭据）。
#[derive(Debug, Clone)]
pub struct UpgradeReporter {
    pub client: CenterClient,
    pub credential: GatewayCredentialBundle,
}

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

    /// 驱动一次升级：写 `running` 记录，起执行器 `{program} prj upgrade --to <version>`，
    /// **升级期间持续刷心跳**；执行器结束后写终态记录，并（若给了 `reporter`）**回执**中心。
    ///
    /// 心跳生产者就是本进程：它持有子进程句柄，知道执行器还活着 —— 这正是「判死判据」需要的信号源
    /// （判据在 [`state::upgrader_is_declared_dead`]，但**必须有生产者**，否则长升级会被判假死）。
    pub async fn start(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        reporter: Option<UpgradeReporter>,
    ) -> Result<(), String> {
        let mut record = UpgradeRecord {
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

        let state_dir = self.state_dir.clone();
        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
            // 「等执行器结束」与「周期刷心跳」并行：任一先到都推进 —— 心跳不新鲜会让升级被判假死。
            let (ok, detail) = loop {
                tokio::select! {
                    status = child.wait() => break outcome_of(status),
                    _ = heartbeat.tick() => {
                        if let Err(err) = state::touch_heartbeat(&state_dir) {
                            eprintln!("event=UpgradeHeartbeatFailed error={err}");
                        }
                    }
                }
            };
            record.status = if ok { "done" } else { "failed" }.to_string();
            record.step = "verify".to_string();
            record.detail = detail;
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                eprintln!("event=UpgradeRecordWriteFailed error={err}");
            }
            if let Some(reporter) = reporter {
                match reporter
                    .client
                    .report_upgrade_result(&reporter.credential, &record)
                    .await
                {
                    Ok(()) => println!("event=UpgradeReported work_id={}", record.work_id),
                    Err(err) => eprintln!("event=UpgradeReportFailed error={err}"),
                }
            }
        });
        Ok(())
    }
}

/// 把执行器退出结果折成 `(成功?, 说明)`。
fn outcome_of(status: std::io::Result<ExitStatus>) -> (bool, String) {
    match status {
        Ok(exit) if exit.success() => (true, format!("executor {exit}")),
        Ok(exit) => (false, format!("executor {exit}")),
        Err(err) => (false, format!("wait failed: {err}")),
    }
}
