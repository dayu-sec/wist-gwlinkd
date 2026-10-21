//! 升级**执行器适配层**。
//!
//! 驱动只认两件事：**怎么调**（[`UpgradeExecutor::command`]）与**结局怎么读**
//! （[`UpgradeExecutor::interpret`]）。`gops` 只是其中一个实现（[`GopsExecutor`]）——
//! 换执行器（别的交付器 / 别的 compose 工具 / 站点脚本）不必改驱动，只需另给一个 impl。
//!
//! 边界：本层只负责「构造命令行 + 归一化结局」；**结果可信度**（成功要不要佐证）是驱动的事，
//! 见 `crate::upgrade`。见 CR-003 R4。

use std::path::{Path, PathBuf};
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
    /// 执行器的**目标系统名**（gops 的位置参数 `NAME`）；`None` = 不点名（工程内全部系统）。
    ///
    /// 注意：它不是计划里的**组件名**。组件名是发布口径（`galaxy-ops` / `wist-gateway-stack` …），
    /// 跟工程里的系统不是一回事 —— 拿它当 `NAME` 会得到「未知系统或无效版本 spec」。
    /// 解析见 [`UpgradeExecutor::resolve_system_name`]。
    pub system_name: Option<&'a str>,
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

    /// 把计划里的**组件名**翻成执行器的**目标系统名**。
    ///
    /// 默认：不支持点名（`None`）—— 只有按系统分派的执行器（gops）才需要。
    /// 返回 `Err` = 组件既不是本机声明的组件、也不是工程里的系统：**拒发**（别把组件名
    /// 当系统名塞进命令行，那会以「未知系统」失败，或更糟——改错升级目标）。
    fn resolve_system_name(&self, _component: Option<&str>) -> Result<Option<String>, String> {
        Ok(None)
    }

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

    /// 解析 `gops prj upgrade` 的位置参数 `NAME`。
    ///
    /// 优先级：显式配置的 `upgrade_project_name` → 计划组件名（仅当它确实对应工程里的系统）
    /// → 工程里唯一系统 → 不点名（`None`，gops 升级工程内全部系统）。
    ///
    /// 为什么不能直接拿组件名当 NAME：组件是**发布口径**（`galaxy-ops` / `galaxy-flow` /
    /// `wist-gateway-stack` …），工程里不一定有同名系统。直接传的后果：
    /// `gops prj upgrade ... galaxy-ops` → 「未知系统或无效版本 spec：`galaxy-ops`」；
    /// 若改成“回落成工程里唯一的系统”，更糟 —— 会拿 gops 的版本去升网关栈。
    /// 所以：对应不上就 **拒发**，并告诉人怎么声明（`[[upgrade.component]]` + `tool-copy`）。
    ///
    /// 解析出来的永远是 **`sys_model.yml` 的 `name`** —— gops 就按它匹配（目录名只当别名看）。
    fn resolve_name(&self, component: Option<&str>) -> Result<Option<String>, String> {
        let explicit = self
            .project_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        // 没工程根 = gops 会跑到进程 cwd（preflight 已拒）：显式配的照给，其余不猜。
        let Some(root) = self.project_dir.as_deref() else {
            return Ok(explicit.map(str::to_string));
        };
        let systems = crate::sys_root::systems_under(root);
        // gops 要的是模型名（目录名只是本地别名）：定位到系统就翻成模型名。
        let name_of = |sys: &Path| {
            crate::sys_root::model_name(sys).unwrap_or_else(|| crate::sys_root::dir_name(sys))
        };

        if let Some(name) = explicit {
            if let Some(found) = systems
                .iter()
                .find(|sys| crate::sys_root::system_matches(sys, name))
            {
                return Ok(Some(name_of(found)));
            }
            // 一个系统都认不出来（工程不在 / 结构不是 `<工程根>/<系统>`）：无从判断，原样交给 gops。
            if systems.is_empty() {
                return Ok(Some(name.to_string()));
            }
            // 有系统却对不上 = 名字写错：当场报，别等 gops 回一句「未知系统」。
            return Err(format!(
                "upgrade_project_name = `{name}` 找不到对应系统：{} 下没有同名目录，也没有 name 相同的系统（已找到：{}）",
                root.display(),
                crate::sys_root::list_systems(&systems)
            ));
        }

        let Some(component) = component else {
            // 计划没给组件：只有一个系统就点名（避免“升级工程内全部”的意外），多个交给 gops。
            return Ok(match systems.as_slice() {
                [only] => Some(name_of(only)),
                _ => None,
            });
        };
        let Some(found) = systems
            .iter()
            .find(|sys| crate::sys_root::system_matches(sys, component))
        else {
            return Err(format!(
                "组件 `{component}` 既不是本机声明的组件（`[[upgrade.component]]`），也不是 gops 工程 {} 里的系统（已导入：{}）；\
                 宿主工具（如 galaxy-ops / galaxy-flow）要在 `[[upgrade.component]]` 里标 `install = \"tool-copy\"`",
                root.display(),
                crate::sys_root::list_systems(&systems)
            ));
        };
        Ok(Some(name_of(found)))
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

    fn resolve_system_name(&self, component: Option<&str>) -> Result<Option<String>, String> {
        self.resolve_name(component)
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
        // NAME 由 [`Self::resolve_name`] 解析好传进来（不直接用计划里的组件名）。
        if let Some(name) = invocation.system_name {
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
