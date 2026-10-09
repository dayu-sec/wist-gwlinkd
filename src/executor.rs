//! 升级**执行器适配层**。
//!
//! 驱动只认两件事：**怎么调**（[`UpgradeExecutor::command`]）与**结局怎么读**
//! （[`UpgradeExecutor::interpret`]）。`gops` 只是其中一个实现（[`GopsExecutor`]）——
//! 换执行器（别的交付器 / 别的 compose 工具 / 站点脚本）不必改驱动，只需另给一个 impl。
//!
//! 边界：本层只负责「构造命令行 + 归一化结局」；**结果可信度**（成功要不要佐证）是驱动的事，
//! 见 `crate::upgrade`。见 CR-003 R4。

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{UpgradeReason, UpgradeResult};

/// 缺省升级执行器程序名。
pub const DEFAULT_UPGRADER_PROGRAM: &str = "gops";
/// 缺省失败处置：**全回滚**（设计稿 §3.3 决策）。
pub const DEFAULT_ON_FAILURE: &str = "rollback-all";

/// 一次调用的输入（来自已批准的升级计划）。
#[derive(Debug, Clone, Copy)]
pub struct ExecutorInvocation<'a> {
    /// 目标版本 / URL / 本机路径（计划里直接填的那个）。
    pub to_version: &'a str,
    /// 计划里的组件名；执行器未固定系统名时用它作 NAME。
    pub component: Option<&'a str>,
}

/// 一次执行的结局（记账 + 回执用）。
///
/// 不变式：`ok == (status == "done")` —— 调用方据 `ok` gate「成功佐证」，两者必须一致。
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub status: String,
    pub step: String,
    pub detail: String,
}

/// 执行器适配器：把「机制」翻成「命令行」，把「结局」归一到 [`Outcome`]。
pub trait UpgradeExecutor: Send + Sync {
    /// 程序路径（诊断 / 日志用）。
    fn program(&self) -> &str;

    /// 发执行器**之前**的前置校验：`Err(可读原因)` = 别发（驱动把它落成可读失败并回执）。
    ///
    /// 缺省无前置；`gops` 需要**工程根**（`ops-prj.yml`），见 [`GopsExecutor`]。
    fn preflight(&self) -> UpgradeResult<()> {
        Ok(())
    }

    /// 构造调用（含参数、cwd、stdout 管道与回收策略）。**stderr 由驱动接管**（落执行器日志）。
    fn command(&self, invocation: &ExecutorInvocation<'_>) -> Command;

    /// 把「退出码 + stdout」折成统一结局。
    fn interpret(&self, exit_ok: bool, stdout: &str, fallback_detail: String) -> Outcome;
}

/// `gops prj upgrade` 适配器。
///
/// 调用契约（gops 2.2.x）：`gops prj upgrade --to <版本|URL|路径> --on-failure <rollback-all|halt>
/// [--health-cmd <cmd> [--health-timeout <s>]] --json [NAME]`。
/// `--on-failure` 现阶段**必填**；`--json` 出机读结局；gops 从 **cwd** 解析工程（`ops-prj.yml`）。
#[derive(Debug, Clone)]
pub struct GopsExecutor {
    program: String,
    on_failure: String,
    /// gops 工程根（含 `ops-prj.yml`）；不设则用本进程 cwd。
    project_dir: Option<PathBuf>,
    /// 只升级该系统（位置参数 NAME；缺省 = 工程里已导入的全部系统）。
    project_name: Option<String>,
    /// 栈外健康检查命令（`--health-cmd`）：**给了才让 gops 做「健康不过即回滚」的判定**。
    health_cmd: Option<String>,
    /// 健康检查超时秒数（`--health-timeout`）。
    health_timeout_seconds: Option<u64>,
}

impl GopsExecutor {
    /// 建 gops 执行器（缺省 `--on-failure rollback-all`）。
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            on_failure: DEFAULT_ON_FAILURE.to_string(),
            project_dir: None,
            project_name: None,
            health_cmd: None,
            health_timeout_seconds: None,
        }
    }

    /// 设 `--on-failure`。
    pub fn with_on_failure(mut self, on_failure: impl Into<String>) -> Self {
        self.on_failure = on_failure.into();
        self
    }

    /// 设工程根与（可选的）固定目标系统名。
    pub fn with_project(mut self, dir: Option<PathBuf>, name: Option<String>) -> Self {
        self.project_dir = dir;
        self.project_name = name;
        self
    }

    /// 设栈外健康检查（gops `--health-cmd` / `--health-timeout`）；给了才让 gops 据健康判定回滚。
    pub fn with_health_check(mut self, cmd: Option<String>, timeout_seconds: Option<u64>) -> Self {
        self.health_cmd = cmd;
        self.health_timeout_seconds = timeout_seconds;
        self
    }
}

impl UpgradeExecutor for GopsExecutor {
    fn program(&self) -> &str {
        &self.program
    }

    fn preflight(&self) -> UpgradeResult<()> {
        // 工程根是 **gops 的要求**，只对 gops 生效（站点脚本执行器不要求）。
        // gops 从 **cwd** 解析工程（`ops-prj.yml`），且 `gops prj upgrade` **没有**指定工程的旗标
        // （见 `gops prj upgrade --help`）—— 所以工程根必须显式配。不配就会跑到进程 cwd，
        // 以「executor exit status: 255」这种不可读的方式失败。这里前置成可读原因。
        if !program_is_gops(&self.program) {
            return Ok(());
        }
        let Some(dir) = self.project_dir.as_deref() else {
            return Err(UpgradeReason::Preflight.err(
                "升级未配置工程根：upgrade_project_dir 未设置（gops 从 cwd 解析 ops-prj.yml，\
                 不配就会以「executor exit status: 255」这种不可读的方式失败）",
            ));
        };
        if !dir.join("ops-prj.yml").is_file() {
            return Err(UpgradeReason::Preflight.err(format!(
                "升级工程根 {} 里没有 ops-prj.yml（gops prj 需要一个运维工程根：`gops prj new` + `gops prj import`）",
                dir.display()
            )));
        }
        Ok(())
    }

    fn command(&self, invocation: &ExecutorInvocation<'_>) -> Command {
        // 固定系统名优先；否则用计划里的组件名作 NAME。
        let name = self.project_name.as_deref().or(invocation.component);
        let mut args = vec![
            "prj".to_string(),
            "upgrade".to_string(),
            "--to".to_string(),
            invocation.to_version.to_string(),
            "--on-failure".to_string(),
            self.on_failure.clone(),
            "--json".to_string(),
        ];
        // 健康检查放在位置参数 NAME 之前。
        if let Some(cmd) = &self.health_cmd {
            args.push("--health-cmd".to_string());
            args.push(cmd.clone());
        }
        if let Some(seconds) = self.health_timeout_seconds {
            args.push("--health-timeout".to_string());
            args.push(seconds.to_string());
        }
        if let Some(name) = name {
            args.push(name.to_string());
        }

        let mut command = Command::new(&self.program);
        command
            .args(&args)
            .stdout(Stdio::piped())
            // 常驻一旦退出（含被 SIGKILL），不把执行器留成孤儿继续动现场。
            .kill_on_drop(true);
        // Linux：父进程死亡即给执行器发 SIGTERM（比 kill_on_drop 的 SIGKILL 温和，给 gops 机会善后）。
        // macOS 无 PDEATHSIG：靠 kill_on_drop（正常退出时）+ systemd cgroup（Linux 宿主）兼容。
        #[cfg(target_os = "linux")]
        unsafe {
            // SAFETY: `pre_exec` 在 fork 后 exec 前跑，回调里只调 async-signal-safe 的 `prctl`。
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if let Some(dir) = &self.project_dir {
            command.current_dir(dir);
        }
        command
    }

    fn interpret(&self, exit_ok: bool, stdout: &str, fallback_detail: String) -> Outcome {
        let parsed = serde_json::from_str::<GopsJson>(stdout.trim())
            .ok()
            .and_then(|json| json.record);
        match parsed {
            Some(rec) => {
                let status = match rec.status.as_str() {
                    "succeeded" => "done".to_string(),
                    "rolled_back" => "rolled_back".to_string(),
                    "failed" => "failed".to_string(),
                    "" if exit_ok => "done".to_string(),
                    "" => "failed".to_string(),
                    // 未知 status：**保留原值**并加前缀（别静默揉成 failed）——契约漂移要看得见。
                    other => format!("unknown:{other}"),
                };
                // `ok` 必须与 `status` 一致（调用方据 `ok` gate 成功佐证）。
                let ok = status == "done";
                let step = if rec.step.is_empty() {
                    "done".to_string()
                } else {
                    rec.step
                };
                let mut detail = rec.detail;
                if let Some(backup) = rec.backup_id {
                    if !detail.is_empty() {
                        detail.push_str("; ");
                    }
                    detail.push_str(&format!("backup={backup}"));
                }
                if detail.is_empty() {
                    detail = fallback_detail;
                }
                Outcome {
                    ok,
                    status,
                    step,
                    detail,
                }
            }
            None => {
                let (ok, detail) = (exit_ok, fallback_detail);
                Outcome {
                    ok,
                    status: if ok { "done" } else { "failed" }.to_string(),
                    step: "verify".to_string(),
                    detail,
                }
            }
        }
    }
}

/// 程序是不是就是 **gops**（按 basename 精确判）。“工程根”这个要求只对 gops 成立；
/// 站点脚本执行器（其它 program）不要求。
pub(crate) fn program_is_gops(program: &str) -> bool {
    std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "gops")
        .unwrap_or(false)
}

/// `gops --json` 单行输出（只取关心的字段）。
#[derive(serde::Deserialize)]
struct GopsJson {
    #[serde(default)]
    record: Option<GopsRecord>,
}

#[derive(serde::Deserialize)]
struct GopsRecord {
    #[serde(default)]
    step: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    backup_id: Option<String>,
    #[serde(default)]
    detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gops() -> GopsExecutor {
        GopsExecutor::new("gops")
    }

    #[test]
    fn program_is_reported_verbatim() {
        assert_eq!(
            GopsExecutor::new("/opt/bin/gops").program(),
            "/opt/bin/gops"
        );
    }

    #[test]
    fn interpret_maps_every_gops_status() {
        // succeeded → done。
        let o = gops().interpret(
            true,
            r#"{"record":{"step":"apply","status":"succeeded","detail":"ok"}}"#,
            "fb".into(),
        );
        assert_eq!(
            (o.ok, o.status.as_str(), o.step.as_str()),
            (true, "done", "apply")
        );
        assert_eq!(o.detail, "ok");

        // rolled_back → rolled_back（回滚 ≠ 成功），backup 追加到 detail。
        let o = gops().interpret(
            true,
            r#"{"record":{"step":"rollback","status":"rolled_back","backup_id":"bk-9","detail":"x"}}"#,
            "fb".into(),
        );
        assert_eq!((o.ok, o.status.as_str()), (false, "rolled_back"));
        assert_eq!(o.detail, "x; backup=bk-9");

        // failed → failed。
        let o = gops().interpret(true, r#"{"record":{"status":"failed"}}"#, "fb".into());
        assert_eq!((o.ok, o.status.as_str()), (false, "failed"));

        // 空 status：按退出码判；step 回落 "done"、detail 回落 fallback。
        let ok = gops().interpret(true, r#"{"record":{"status":""}}"#, "fb".into());
        assert_eq!(
            (ok.ok, ok.status.as_str(), ok.step.as_str()),
            (true, "done", "done")
        );
        assert_eq!(ok.detail, "fb");
        let bad = gops().interpret(false, r#"{"record":{"status":""}}"#, "fb".into());
        assert_eq!((bad.ok, bad.status.as_str()), (false, "failed"));

        // 未知 status：保留原值并加前缀（不静默揉成 failed）。
        let o = gops().interpret(true, r#"{"record":{"status":"weird"}}"#, "fb".into());
        assert_eq!((o.ok, o.status.as_str()), (false, "unknown:weird"));

        // 非 JSON：回落退出码。
        let o = gops().interpret(false, "not json at all", "executor exited 3".into());
        assert_eq!(
            (o.ok, o.status.as_str(), o.detail.as_str()),
            (false, "failed", "executor exited 3")
        );
    }
}
