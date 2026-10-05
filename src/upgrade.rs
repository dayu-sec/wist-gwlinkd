//! 升级闭环：拉 desired → 幂等 → 驱动执行器 → **回执**。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归执行器）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。
//!
//! 执行器经 [`crate::executor`] 的适配层调用（`gops` 只是其中一个实现）：本驱动只认
//! 「构造调用 + 归一化结局」。**成功要佐证**：执行器报成后，再用网关**自述面**独立确认
//! 「网关确实回来了且健康」——只信执行器一面之词会让 `done` 可能是假的。

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::center::CenterClient;
use crate::executor::{ExecutorInvocation, UpgradeExecutor};
use crate::selfreport::SelfReportClient;
use crate::state::{self, UpgradeRecord};

/// 升级执行器日志文件（放状态目录，运维固定地方找）。
pub const UPGRADER_LOG_FILE: &str = "wist-upgrader.log";

/// 升级期间的心跳刷新周期（远小于 [`state::UPGRADER_DEAD_AFTER`]）。
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// **成功佐证的观测窗口**：执行器报成后，等网关自述面恢复健康的最长时间。
///
/// 网关升级 = 重建网关容器，执行器返回时网关多半还在重启；给足恢复时间再判。
pub const RECOVERY_VERIFY_TIMEOUT: Duration = Duration::from_secs(300);

/// 佐证轮询间隔。
const RECOVERY_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// 升级回执的投递方（中心客户端 + 状态目录 + 自述面）。
///
/// **不**捕获凭据快照：升级可能跨越一次 `renew`（旧证书立即失效），回执时**现读**最新凭据。
#[derive(Debug, Clone)]
pub struct UpgradeReporter {
    pub client: CenterClient,
    pub state_dir: PathBuf,
    /// 网关自述面客户端（来自 `gateway_self_endpoint`）；**有它才能对「成功」做独立佐证**。
    pub self_client: Option<SelfReportClient>,
}

/// 升级驱动：把一次「升到某版本」交给执行器，并在状态目录记账 + 心跳。
#[derive(Clone)]
pub struct UpgradeDriver {
    /// 执行器适配器（`gops` 是其一；换执行器只换这里）。
    pub executor: Arc<dyn UpgradeExecutor>,
    /// 状态目录（写 [`crate::state::UPGRADE_RECORD_FILE`]、心跳与执行器日志）。
    pub state_dir: PathBuf,
    /// 成功佐证的观测窗口（执行器报成后，等网关自述面恢复健康的最长时间）。
    pub verify_timeout: Duration,
}

impl UpgradeDriver {
    /// 建驱动：给定执行器适配器与状态目录。
    pub fn new(executor: impl UpgradeExecutor + 'static, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            executor: Arc::new(executor),
            state_dir: state_dir.into(),
            verify_timeout: RECOVERY_VERIFY_TIMEOUT,
        }
    }

    /// 设成功佐证的观测窗口（缺省 [`RECOVERY_VERIFY_TIMEOUT`]）。
    pub fn with_verify_timeout(mut self, timeout: Duration) -> Self {
        self.verify_timeout = timeout;
        self
    }

    /// 驱动一次升级：写 `running` 记录，起执行器，**升级期间持续刷心跳**；执行器结束后写终态记录，
    /// （若报成）用自述面**佐证**「网关确实回来了」，再（若给了 `reporter`）**回执**中心。
    ///
    /// 心跳生产者就是本进程：它持有子进程句柄，知道执行器还活着 —— 这正是「判死判据」需要的信号源
    /// （判据在 [`state::upgrader_is_declared_dead`]，但**必须有生产者**，否则长升级会被判假死）。
    ///
    /// `component` 来自升级计划：当执行器未配固定系统名时，作为 gops 的 NAME 传入（只升该系统）。
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

        // 执行器的 stdout/stderr 落日志文件（失败时运维要能看现场）。stderr 由驱动接管；
        // stdout 由适配器置为管道，供解读结局。
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

        let invocation = ExecutorInvocation {
            to_version,
            component,
        };
        let mut command = self.executor.command(&invocation);
        command.stderr(stderr);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                // 起不来也是**终态**：别把台账停在 running（否则会被判死 / 误诊，且挡住重驱）。
                let detail = format!("启动升级执行器 {} 失败: {err}", self.executor.program());
                record.step = "spawn".to_string();
                record.status = "failed".to_string();
                record.detail = detail.clone();
                if let Err(werr) = state::write_upgrade_record(&self.state_dir, &record) {
                    eprintln!("event=UpgradeRecordWriteFailed error={werr}");
                }
                return Err(detail);
            }
        };
        let stdout = child.stdout.take();

        let state_dir = self.state_dir.clone();
        let executor = Arc::clone(&self.executor);
        let verify_timeout = self.verify_timeout;
        tokio::spawn(async move {
            // 心跳生产者：**覆盖整个升级事务**（执行器运行 + 成功佐证）。
            // 只在「等子进程」期间刷跳不够 —— 佐证最长 `verify_timeout`，期间若心跳陈旧：
            // ① 诊断会误报 FAIL；② 常驻重启会把同一计划当「判死」重驱（重复升级）。
            let heartbeat_state = state_dir.clone();
            let _heartbeat = AbortOnDrop(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
                loop {
                    ticker.tick().await;
                    if let Err(err) = state::touch_heartbeat(&heartbeat_state) {
                        eprintln!("event=UpgradeHeartbeatFailed error={err}");
                    }
                }
            }));

            // **并发排空 stdout**：子进程写满管道会阻塞退出 —— 只在 wait() 之后再读会死锁。
            let stdout_reader = stdout.map(|mut out| {
                tokio::spawn(async move {
                    let mut buf = String::new();
                    let _ = out.read_to_string(&mut buf).await;
                    buf
                })
            });

            let (ok, detail) = outcome_of(child.wait().await);
            let raw = match stdout_reader {
                Some(handle) => handle.await.unwrap_or_default(),
                None => String::new(),
            };
            let mut outcome = executor.interpret(ok, &raw, detail);

            // 佐证只用网关 id（不随 renew 变）；回执另现读最新凭据（见下）。
            let gateway_id = reporter
                .as_ref()
                .and_then(|reporter| state::load_credential(&reporter.state_dir))
                .map(|credential| credential.bundle.gateway_id);

            // **成功佐证**：执行器报成不等于网关真的起来了 —— 独立观测一次。
            if outcome.ok {
                // 佐证期间把台账步进到 verify，诊断看起来才与事实一致。
                record.step = "verify".to_string();
                if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                    eprintln!("event=UpgradeRecordWriteFailed error={err}");
                }
                if let Some(reporter) = reporter.as_ref() {
                    match reporter.self_client.as_ref() {
                        Some(self_client) => match gateway_id.as_deref() {
                            Some(gateway_id) => {
                                match corroborate_recovery(self_client, gateway_id, verify_timeout)
                                    .await
                                {
                                    Ok(evidence) => append_detail(&mut outcome.detail, evidence),
                                    Err(reason) => {
                                        // 执行器说成了、但网关未被观测到恢复 → 不认「done」。
                                        outcome.ok = false;
                                        outcome.status = "unverified".to_string();
                                        outcome.step = "verify".to_string();
                                        append_detail(&mut outcome.detail, reason.clone());
                                        eprintln!(
                                            "event=UpgradeUnverified work_id={} {reason}",
                                            record.work_id
                                        );
                                    }
                                }
                            }
                            None => append_detail(
                                &mut outcome.detail,
                                "未佐证（读不到长期身份）".to_string(),
                            ),
                        },
                        None => append_detail(
                            &mut outcome.detail,
                            "未佐证（未配置 gateway_self_endpoint，无法独立观测）".to_string(),
                        ),
                    }
                }
            }

            record.status = outcome.status;
            record.step = outcome.step;
            record.detail = outcome.detail;
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                eprintln!("event=UpgradeRecordWriteFailed error={err}");
            }
            if let Some(reporter) = reporter {
                // 回执**现读**最新凭据：升级可能跨越一次 renew（旧证书立即失效）。
                match state::load_credential(&reporter.state_dir) {
                    Some(credential) => match reporter
                        .client
                        .report_upgrade_result(&credential.bundle.gateway_id, &record)
                        .await
                    {
                        Ok(()) => println!("event=UpgradeReported work_id={}", record.work_id),
                        Err(err) => eprintln!("event=UpgradeReportFailed error={err}"),
                    },
                    None => eprintln!("event=UpgradeReportSkipped 无长期身份"),
                }
            }
        });
        Ok(())
    }
}

impl std::fmt::Debug for UpgradeDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeDriver")
            .field("executor", &self.executor.program())
            .field("state_dir", &self.state_dir)
            .field("verify_timeout", &self.verify_timeout)
            .finish()
    }
}

/// RAII 守卫：作用域退出（含 panic）即 abort 后台任务，避免心跳任务泄漏成孤儿。
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 把一句佐证/说明追加进 `detail`（分号分隔）。
fn append_detail(detail: &mut String, extra: String) {
    if !detail.is_empty() {
        detail.push('；');
    }
    detail.push_str(&extra);
}

/// **升级成功佐证**：执行器报成后，独立观测**网关自述面**是否恢复且健康。
///
/// 为什么不比版本：计划的 `to_version` 是**栈**版本，而自述面的 `version` 是**网关容器**版本
/// （两个版本空间，如栈 0.1.23 ↔ 网关镜像 v0.1.15），直接比会误判。故以「网关回来了且健康」
/// 为佐证 —— 正好覆盖「执行器说成了，但网关根本没起来」这个假成功。
async fn corroborate_recovery(
    self_client: &SelfReportClient,
    gateway_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    loop {
        // 单次探测不得超过剩余窗口：否则一次卡住的 fetch 会把窗口拖长到 HTTP_TIMEOUT(30s)。
        let probe_window = deadline.saturating_duration_since(Instant::now());
        let last = match tokio::time::timeout(probe_window, self_client.fetch(gateway_id)).await {
            Ok(Ok(state)) if state.health() == "ok" => {
                return Ok(format!(
                    "佐证：网关自述面已恢复且健康（version={}）",
                    state.version
                ));
            }
            Ok(Ok(state)) => format!("自述面健康度={}", state.health()),
            Ok(Err(err)) => err,
            Err(_) => "自述面探测超时".to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "执行器报成但未能佐证（{}s 内）：{last}",
                timeout.as_secs()
            ));
        }
        // 也别睡过 deadline。
        let sleep = RECOVERY_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()));
        tokio::time::sleep(sleep).await;
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
    use crate::executor::{GopsExecutor, Outcome};
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

    /// 写一个把参数落到 `<dir>/args.txt` 再退出的脚本（用于断言命令行）。
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
        let gops = GopsExecutor::new("gops");
        let succeeded = r#"{"record":{"step":"done","status":"succeeded","detail":"ok"}}"#;
        assert_eq!(
            gops.interpret(true, succeeded, "fallback".into()),
            Outcome {
                ok: true,
                status: "done".into(),
                step: "done".into(),
                detail: "ok".into(),
            }
        );
        let rolled_back = r#"{"record":{"step":"rollback","status":"rolled_back","backup_id":"bk-1","detail":"bad"}}"#;
        let outcome = gops.interpret(true, rolled_back, "fallback".into());
        assert!(!outcome.ok);
        assert_eq!(outcome.status, "rolled_back");
        assert_eq!(outcome.step, "rollback");
        assert_eq!(outcome.detail, "bad; backup=bk-1");
        // 没有 JSON：回落退出码。
        let fallback = gops.interpret(false, "not json", "executor exited 1".into());
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
        for _ in 0..600 {
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
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string()),
            &dir,
        );
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
            GopsExecutor::new(script(&dir, "bad.sh", 1).to_string_lossy().to_string()),
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
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string())
                .with_on_failure("halt"),
            &dir,
        );
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

    #[tokio::test]
    async fn start_passes_health_check_flags_when_configured() {
        let dir = temp_dir("health");
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string())
                .with_health_check(
                    Some("curl -fsS http://127.0.0.1:3000/health".to_string()),
                    Some(30),
                ),
            &dir,
        );
        driver
            .start("w-4", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(
            args.contains("--health-cmd curl -fsS http://127.0.0.1:3000/health"),
            "{args}"
        );
        assert!(args.contains("--health-timeout 30"), "{args}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 起一个只回一份固定 JSON 的极简 HTTP 服务，返回其 base URL。
    async fn serve_self_state(payload: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn corroborate_recovery_passes_when_healthy_and_fails_when_degraded() {
        let healthy = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"uplink_enabled":true,"last_error":null}"#;
        let client = SelfReportClient::new(serve_self_state(healthy).await);
        let evidence = corroborate_recovery(&client, "gw-1", Duration::from_secs(5))
            .await
            .expect("healthy gateway corroborates");
        assert!(evidence.contains("佐证"), "{evidence}");

        let degraded = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"uplink_enabled":false,"last_error":null}"#;
        let client = SelfReportClient::new(serve_self_state(degraded).await);
        let err = corroborate_recovery(&client, "gw-1", Duration::from_secs(4))
            .await
            .expect_err("degraded gateway must not corroborate");
        assert!(err.contains("未能佐证"), "{err}");
    }

    #[tokio::test]
    async fn corroborate_recovery_probe_is_bounded_by_the_window() {
        // 服务端接受连接但永不回包：不做 per-probe 约束会卡满 HTTP_TIMEOUT(30s)。
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                // 故意不关：让客户端一直等（否则 FIN 会让它立即读到 EOF）。
                std::mem::forget(sock);
            }
        });
        let client = SelfReportClient::new(format!("http://{addr}"));
        let start = Instant::now();
        let err = corroborate_recovery(&client, "gw-1", Duration::from_secs(1))
            .await
            .expect_err("stalled server must not corroborate");
        assert!(err.contains("未能佐证"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "elapsed={:?}",
            start.elapsed()
        );
    }

    /// 适配层的意义：驱动认的是 `UpgradeExecutor`，**不必然**是 gops。
    #[tokio::test]
    async fn driver_runs_any_executor_impl_not_just_gops() {
        struct FixedExecutor;
        impl UpgradeExecutor for FixedExecutor {
            fn program(&self) -> &str {
                "fixed-executor"
            }
            fn command(&self, _invocation: &ExecutorInvocation<'_>) -> tokio::process::Command {
                let mut command = tokio::process::Command::new("/bin/sh");
                command.arg("-c").arg("true");
                command
            }
            fn interpret(&self, _ok: bool, _raw: &str, _fallback: String) -> Outcome {
                Outcome {
                    ok: true,
                    status: "done".into(),
                    step: "custom".into(),
                    detail: "by custom executor".into(),
                }
            }
        }

        let dir = temp_dir("custom");
        let driver = UpgradeDriver::new(FixedExecutor, &dir);
        driver
            .start("w-c", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert_eq!(record.step, "custom");
        assert_eq!(record.detail, "by custom executor");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn default_on_failure_is_rollback_all() {
        let dir = temp_dir("default-onfail");
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start("w-d", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(args.contains("--on-failure rollback-all"), "{args}");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── 驱动级「成功佐证」映射 ──

    const SELF_STATE_HEALTHY: &str = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"uplink_enabled":true,"last_error":null}"#;
    const SELF_STATE_DEGRADED: &str = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"uplink_enabled":false,"last_error":null}"#;

    /// 佐证分支要 `gateway_id`（从长期身份读）——所以得先存在一份凭据。
    fn write_credential(dir: &Path, gateway_id: &str) {
        let credential = state::StoredCredential {
            bundle: wist_contracts::gateway_control::GatewayCredentialBundle {
                credential_id: "cred-test".into(),
                gateway_id: gateway_id.into(),
                instance_id: None,
                certificate: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .into(),
                ca_bundle: None,
                issued_at: "2026-10-04T00:00:00Z".into(),
                not_before: None,
                not_after: None,
            },
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n"
                .into(),
        };
        state::save_credential(dir, &credential).expect("credential");
    }

    /// 回执走 mTLS（身份在 `CenterClient` 里）；endpoint 故意不可达 —— 只关心台账，回执失败只记日志。
    fn reporter(dir: &Path, self_endpoint: Option<String>) -> UpgradeReporter {
        UpgradeReporter {
            client: crate::center::CenterClient::new("http://127.0.0.1:1"),
            state_dir: dir.to_path_buf(),
            self_client: self_endpoint.map(crate::selfreport::SelfReportClient::new),
        }
    }

    #[tokio::test]
    async fn a_healthy_gateway_corroborates_a_reported_success() {
        let dir = temp_dir("corr-ok");
        write_credential(&dir, "gw-1");
        let endpoint = serve_self_state(SELF_STATE_HEALTHY).await;
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_verify_timeout(Duration::from_secs(5));
        driver
            .start("w-ok", "0", "1", None, Some(reporter(&dir, Some(endpoint))))
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert!(record.detail.contains("佐证"), "{}", record.detail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_degraded_gateway_downgrades_success_to_unverified() {
        let dir = temp_dir("corr-degraded");
        write_credential(&dir, "gw-1");
        let endpoint = serve_self_state(SELF_STATE_DEGRADED).await;
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_verify_timeout(Duration::from_secs(4));
        driver
            .start(
                "w-bad",
                "0",
                "1",
                None,
                Some(reporter(&dir, Some(endpoint))),
            )
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "unverified");
        assert!(record.detail.contains("未能佐证"), "{}", record.detail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn success_without_self_endpoint_is_flagged_unobserved() {
        let dir = temp_dir("corr-none");
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start("w-none", "0", "1", None, Some(reporter(&dir, None)))
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert!(
            record
                .detail
                .contains("未佐证（未配置 gateway_self_endpoint"),
            "{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_executor_is_not_corroborated() {
        let dir = temp_dir("corr-fail");
        write_credential(&dir, "gw-1");
        let endpoint = serve_self_state(SELF_STATE_HEALTHY).await;
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "bad.sh", 1).to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start(
                "w-fail",
                "0",
                "1",
                None,
                Some(reporter(&dir, Some(endpoint))),
            )
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(!record.detail.contains("佐证"), "{}", record.detail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn success_with_self_endpoint_but_no_credential_is_flagged_unobserved() {
        // 有自述面但读不到网关 id（无长期身份）→ 佐证跑不起来，得如实标注。
        let dir = temp_dir("corr-nocred");
        let endpoint = serve_self_state(SELF_STATE_HEALTHY).await;
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script(&dir, "ok.sh", 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_verify_timeout(Duration::from_secs(3));
        driver
            .start(
                "w-nocred",
                "0",
                "1",
                None,
                Some(reporter(&dir, Some(endpoint))),
            )
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert!(
            record.detail.contains("未佐证（读不到长期身份）"),
            "{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_unspawnable_executor_leaves_a_terminal_failed_record() {
        // 起不来的执行器也算「终态」——否则台账停在 running 会被判死 / 误诊。
        let dir = temp_dir("spawn-fail");
        let driver = UpgradeDriver::new(GopsExecutor::new("/nonexistent/gops-xyz"), &dir);
        let err = driver.start("w-s", "0.1.0", "0.1.16", None, None).await;
        assert!(err.is_err(), "spawn 失败应返回 Err");
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "failed");
        assert_eq!(record.step, "spawn");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_chatty_executor_does_not_deadlock_the_driver() {
        // 子进程写满 stdout 管道会阻塞退出；只在 wait() 之后读会死锁。
        let dir = temp_dir("chatty");
        let body = "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 20000 ]; do\n  printf 'noise-%s-xxxxxxxxxxxxxxxx\\n' \"$i\"\n  i=$((i+1))\ndone\nexit 0\n";
        let script = write_exec(&dir, "chatty.sh", body);
        let driver = UpgradeDriver::new(
            GopsExecutor::new(script.to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start("w-chatty", "0.1.0", "0.1.16", None, None)
            .await
            .expect("start");
        // 旧实现会卡在 wait()（子进程阻塞在写）→ 记录一直 running → 这里超时 panic。
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        let _ = std::fs::remove_dir_all(dir);
    }
}
