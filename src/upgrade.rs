//! 升级闭环：拉 desired → 幂等 → 驱动执行器（`gops prj upgrade`）→ **回执**。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归 `gops`）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。
//!
//! gops 的调用契约（2.2.x）：`gops prj upgrade --to <版本|URL|路径> --on-failure <rollback-all|halt>
//! [--json] [NAME]`。`--on-failure` **现阶段必填**；`--json` 出机读结局（成功 / 失败 / 已回滚）。
//! gops 从 **cwd** 解析工程（`ops-prj.yml`），故需 `project_dir`。

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::center::CenterClient;
use crate::state::{self, UpgradeRecord};

/// 缺省升级执行器程序名。
pub const DEFAULT_UPGRADER_PROGRAM: &str = "gops";
/// 缺省失败处置：**全回滚**（设计稿 §3.3 决策）。
pub const DEFAULT_ON_FAILURE: &str = "rollback-all";
/// 升级执行器日志文件（放状态目录，运维固定地方找）。
pub const UPGRADER_LOG_FILE: &str = "wist-upgrader.log";

/// 升级期间的心跳刷新周期（远小于 [`state::UPGRADER_DEAD_AFTER`]）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// 升级回执的投递方（中心客户端 + 状态目录）。
///
/// **不**捕获凭据快照：升级可能跨越一次 `renew`（旧 `rt_` 立即失效），回执时**现读**最新凭据。
#[derive(Debug, Clone)]
pub struct UpgradeReporter {
    pub client: CenterClient,
    pub state_dir: PathBuf,
}

/// 升级驱动：把一次「升到某版本」交给执行器，并在状态目录记账 + 心跳。
#[derive(Debug, Clone)]
pub struct UpgradeDriver {
    /// 执行器程序（如 `gops`）。
    pub program: String,
    /// 状态目录（写 [`crate::state::UPGRADE_RECORD_FILE`]、心跳与执行器日志）。
    pub state_dir: PathBuf,
    /// `--on-failure`（`rollback-all` | `halt`）。
    pub on_failure: String,
    /// gops 工程根（含 `ops-prj.yml`）：gops 从 cwd 解析工程，不设则用本进程 cwd。
    pub project_dir: Option<PathBuf>,
    /// 只升级该系统（gops 位置参数 NAME；缺省 = 工程里已导入的全部系统）。
    pub project_name: Option<String>,
}

impl UpgradeDriver {
    /// 建驱动（缺省 `--on-failure rollback-all`，不指定工程/系统）。
    pub fn new(program: impl Into<String>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            state_dir: state_dir.into(),
            on_failure: DEFAULT_ON_FAILURE.to_string(),
            project_dir: None,
            project_name: None,
        }
    }

    /// 设 `--on-failure`。
    pub fn with_on_failure(mut self, on_failure: impl Into<String>) -> Self {
        self.on_failure = on_failure.into();
        self
    }

    /// 设 gops 工程根与（可选的）目标系统名。
    pub fn with_project(mut self, dir: Option<PathBuf>, name: Option<String>) -> Self {
        self.project_dir = dir;
        self.project_name = name;
        self
    }

    /// 驱动一次升级：写 `running` 记录，起执行器，**升级期间持续刷心跳**；执行器结束后写终态记录，
    /// 并（若给了 `reporter`）**回执**中心。
    ///
    /// 心跳生产者就是本进程：它持有子进程句柄，知道执行器还活着 —— 这正是「判死判据」需要的信号源
    /// （判据在 [`state::upgrader_is_declared_dead`]，但**必须有生产者**，否则长升级会被判假死）。
    ///
    /// `component` 来自升级计划：当驱动未配 `project_name` 时，作为 gops 的 NAME 传入（只升该系统）。
    pub async fn start(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        component: Option<&str>,
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

        // 执行器的 stdout/stderr 落日志文件（失败时运维要能看现场）。
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_dir.join(UPGRADER_LOG_FILE))
            .ok();
        let stderr = match &log {
            Some(file) => Stdio::from(
                file.try_clone()
                    .map_err(|err| format!("复制日志句柄失败: {err}"))?,
            ),
            None => Stdio::null(),
        };

        let name = self.project_name.as_deref().or(component);
        let mut args = vec![
            "prj".to_string(),
            "upgrade".to_string(),
            "--to".to_string(),
            to_version.to_string(),
            "--on-failure".to_string(),
            self.on_failure.clone(),
            "--json".to_string(),
        ];
        if let Some(name) = name {
            args.push(name.to_string());
        }

        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(stderr)
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
        let mut child = command
            .spawn()
            .map_err(|err| format!("启动升级执行器 {} 失败: {err}", self.program))?;
        let stdout = child.stdout.take();

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
            let mut raw = String::new();
            if let Some(mut out) = stdout {
                let _ = out.read_to_string(&mut raw).await;
            }
            let outcome = interpret(ok, &raw, detail);
            record.status = outcome.status;
            record.step = outcome.step;
            record.detail = outcome.detail;
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                eprintln!("event=UpgradeRecordWriteFailed error={err}");
            }
            if let Some(reporter) = reporter {
                match state::load_credential(&reporter.state_dir) {
                    Some(credential) => match reporter
                        .client
                        .report_upgrade_result(&credential, &record)
                        .await
                    {
                        Ok(()) => println!("event=UpgradeReported work_id={}", record.work_id),
                        Err(err) => eprintln!("event=UpgradeReportFailed error={err}"),
                    },
                    None => eprintln!("event=UpgradeReportSkipped 无运行期凭据"),
                }
            }
        });
        Ok(())
    }
}

/// 一次执行的结局（记账用）。
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    ok: bool,
    status: String,
    step: String,
    detail: String,
}

/// `--json` 单行输出（只取关心的字段）。
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

/// 把「退出码 + stdout」折成记账结局：优先信 `--json` 的 record（能区分失败/已回滚），否则回落退出码。
fn interpret(exit_ok: bool, raw: &str, fallback_detail: String) -> Outcome {
    let parsed = serde_json::from_str::<GopsJson>(raw.trim())
        .ok()
        .and_then(|json| json.record);
    match parsed {
        Some(rec) => {
            let ok = rec.status == "succeeded";
            let status = match rec.status.as_str() {
                "succeeded" => "done",
                "rolled_back" => "rolled_back",
                "failed" => "failed",
                "" if exit_ok => "done",
                "" => "failed",
                _ => "failed",
            }
            .to_string();
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

/// 把执行器退出结果折成 `(成功?, 说明)`。
fn outcome_of(status: std::io::Result<ExitStatus>) -> (bool, String) {
    match status {
        Ok(exit) if exit.success() => (true, format!("executor {exit}")),
        Ok(exit) => (false, format!("executor {exit}")),
        Err(err) => (false, format!("wait failed: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gwlinkd-upgrade-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// 写一个忽略参数、按给定码退出的脚本（跨平台无关：只靠 `sh`）。
    fn script(dir: &Path, name: &str, exit_code: i32) -> PathBuf {
        write_exec(dir, name, &format!("#!/bin/sh\nexit {exit_code}\n"))
    }

    /// 写一个把参数落到 `<dir>/args.txt` 再退出的脚本（用于断言 gops 命令行）。
    fn arg_recording_script(dir: &Path, exit_code: i32) -> PathBuf {
        let body = format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > {}\nexit {exit_code}\n",
            dir.join("args.txt").display()
        );
        write_exec(dir, "record-args.sh", &body)
    }

    fn write_exec(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    #[test]
    fn interpret_prefers_json_outcome_and_maps_rolled_back() {
        let succeeded = r#"{"record":{"step":"done","status":"succeeded","detail":"ok"}}"#;
        assert_eq!(
            interpret(true, succeeded, "fallback".into()),
            Outcome {
                ok: true,
                status: "done".into(),
                step: "done".into(),
                detail: "ok".into(),
            }
        );
        let rolled_back = r#"{"record":{"step":"rollback","status":"rolled_back","backup_id":"bk-1","detail":"bad"}}"#;
        let outcome = interpret(true, rolled_back, "fallback".into());
        assert!(!outcome.ok);
        assert_eq!(outcome.status, "rolled_back");
        assert_eq!(outcome.step, "rollback");
        assert_eq!(outcome.detail, "bad; backup=bk-1");
        // 没有 JSON：回落退出码。
        let fallback = interpret(false, "not json", "executor exited 1".into());
        assert!(!fallback.ok);
        assert_eq!(fallback.status, "failed");
    }

    #[test]
    fn outcome_of_classifies_success_and_failure() {
        let dir = temp_dir("outcome");
        let ok = std::process::Command::new(script(&dir, "ok.sh", 0))
            .status()
            .expect("ok");
        assert!(outcome_of(Ok(ok)).0);
        let bad = std::process::Command::new(script(&dir, "bad.sh", 3))
            .status()
            .expect("bad");
        assert!(!outcome_of(Ok(bad)).0);
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn wait_terminal(state_dir: &Path) -> UpgradeRecord {
        for _ in 0..100 {
            if let Some(record) = state::read_upgrade_record(state_dir)
                && record.status != "running"
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("record never reached a terminal status");
    }

    #[tokio::test]
    async fn start_writes_running_with_heartbeat_then_finishes_done() {
        let dir = temp_dir("done");
        let driver =
            UpgradeDriver::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string(), &dir);
        driver
            .start("w-1", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        // 起手即 running + 心跳（判死判据的生产者）。
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "running");
        assert!(state::heartbeat_is_fresh(
            &dir,
            std::time::SystemTime::now()
        ));
        // 执行器结束后写终态。
        assert_eq!(wait_terminal(&dir).await.status, "done");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failing_executor_finishes_failed() {
        let dir = temp_dir("failed");
        let driver = UpgradeDriver::new(
            script(&dir, "bad.sh", 1).to_string_lossy().to_string(),
            &dir,
        );
        driver
            .start("w-2", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        assert_eq!(wait_terminal(&dir).await.status, "failed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn start_passes_on_failure_json_and_component_to_the_executor() {
        let dir = temp_dir("args");
        let driver = UpgradeDriver::new(
            arg_recording_script(&dir, 0).to_string_lossy().to_string(),
            &dir,
        )
        .with_on_failure("halt");
        driver
            .start("w-3", "0.1.0", "0.1.16", Some("gw-stack"), None)
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(args.contains("prj upgrade"), "{args}");
        assert!(args.contains("--to 0.1.16"), "{args}");
        assert!(args.contains("--on-failure halt"), "{args}");
        assert!(args.contains("--json"), "{args}");
        assert!(args.ends_with("gw-stack"), "{args}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
