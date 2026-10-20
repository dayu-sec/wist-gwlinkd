//! 升级闭环：拉 desired → 幂等 → 驱动执行器 → **回执**。
//!
//! **不实现制品**（下载 / 校验 / 原子切换归执行器）；这里只**驱动 + 记账 + 回执**。
//! 执行器是**瞬态进程**，不被本常驻托管 —— 升级时本进程要能跨过它（这正是「容器外常驻」的意义）。
//!
//! 执行器经 [`crate::executor`] 的适配层调用（`gops` 只是其中一个实现）：本驱动只认
//! 「构造调用 + 归一化结局」。**成功要佐证**：执行器报成后，再用网关**自述面**独立确认
//! 「网关确实回来了且健康」——只信执行器一面之词会让 `done` 可能是假的。

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::center::CenterClient;
use crate::error::{UpgradeReason, UpgradeResult, chain_one_line, logged_op};
use crate::executor::{ExecutorInvocation, Outcome, UpgradeExecutor};
use crate::selfreport::SelfReportClient;
use crate::state::{self, UpgradeRecord};
use crate::task_log;
use crate::tool_install::ToolInstaller;

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

/// 本进程自身的组件名（与中心发布的 release `component` 对齐）。
///
/// 升级计划里出现它时**不走组件目录、也不走 gops**：覆盖的目标就是本进程自己，
/// 覆盖成功后由服务管理器拉起（systemd `Restart=always` / launchd `KeepAlive`）。
pub const SELF_COMPONENT: &str = "wist-gwlinkd";

/// 升级回执的投递方（中心客户端 + 状态/身份目录 + 自述面）。
///
/// **不**捕获凭据快照：升级可能跨越一次 `renew`（旧证书立即失效），回执时**现读**最新凭据。
#[derive(Debug, Clone)]
pub struct UpgradeReporter {
    pub client: CenterClient,
    /// 升级簿记（记录 / 游标 / 心跳）所在目录。
    pub state_dir: PathBuf,
    /// 长期身份（客户端证书）所在目录。
    ///
    /// 与 `state_dir` **可能不同**（网关栈的 `identity/gwlinkd` vs `configs/gwlinkd/state`）：
    /// 回执要现读凭据，读错目录会让回执被静默跳过。
    pub identity_dir: PathBuf,
    /// 网关自述面客户端（来自 `gateway_self_endpoint`）；**有它才能对「成功」做独立佐证**。
    pub self_client: Option<SelfReportClient>,
}

/// 自更新前置：本进程**确实由服务定义托管**，且定义里的二进制就是当前进程。
///
/// 两个条件缺一不可：覆盖成功后进程会退出并**指望服务管理器把它拉起来**；
/// 手动跑的进程退出了就再也没有下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateTarget {
    /// 服务定义里登记的二进制路径（可能是符号链接）。
    pub service_bin: PathBuf,
    /// 当前进程的可执行文件（用于确认真的是同一个目标）。
    pub running_exe: PathBuf,
}

impl SelfUpdateTarget {
    pub fn new(service_bin: impl Into<PathBuf>, running_exe: impl Into<PathBuf>) -> Self {
        Self {
            service_bin: service_bin.into(),
            running_exe: running_exe.into(),
        }
    }
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
    /// **无状态工具**的进程内安装器：目录里标了 `tool-copy` 的组件走它（不经 gops）。缺省无。
    pub tools: Option<Arc<ToolInstaller>>,
    /// 自更新目标：`Some` 才允许驱动 `wist-gwlinkd` 自己的升级。
    pub self_update: Option<SelfUpdateTarget>,
    /// 取中心制品的 HTTP 客户端（带 CA-S / 客户端证书）。
    ///
    /// **为什么驱动需要它**：`gops` 不持中心信任，自己 HEAD 中心地址会报
    /// `error sending request`（私有 CA）。所以 gwlinkd 先取包到本地，再给 gops 一个**路径**
    /// （`gops prj upgrade --to <URL|PATH>` 两种都收）。
    pub artifact_http: Option<reqwest::Client>,
    /// 自更新已落盘、等主循环退出后由服务管理器拉起（主循环取一次就清）。
    restart_requested: Arc<AtomicBool>,
}

impl UpgradeDriver {
    /// 建驱动：给定执行器适配器与状态目录。
    pub fn new(executor: impl UpgradeExecutor + 'static, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            executor: Arc::new(executor),
            state_dir: state_dir.into(),
            verify_timeout: RECOVERY_VERIFY_TIMEOUT,
            tools: None,
            self_update: None,
            artifact_http: None,
            restart_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 设成功佐证的观测窗口（缺省 [`RECOVERY_VERIFY_TIMEOUT`]）。
    pub fn with_verify_timeout(mut self, timeout: Duration) -> Self {
        self.verify_timeout = timeout;
        self
    }

    /// 装配**中心制品取件**客户端（带 CA-S / 客户端证书）。
    ///
    /// 装上后，`gops` 路径会先把 `artifact_url` 取到本地再交给执行器 ——
    /// 执行器不持中心信任，直接拿中心地址会因私有 CA 失败。
    pub fn with_artifact_client(mut self, client: reqwest::Client) -> Self {
        self.artifact_http = Some(client);
        self
    }

    /// 装配无状态工具的进程内安装器（目录里标了 `tool-copy` 的组件走它）。
    pub fn with_tools(mut self, tools: ToolInstaller) -> Self {
        self.tools = Some(Arc::new(tools));
        self
    }

    /// 声明自更新目标（服务定义里的二进制 + 当前进程）。
    ///
    /// 没设它时，升级计划里的 `wist-gwlinkd` 会**前置失败**：自更新必须能靠服务管理器拉起，
    /// 手动跑的进程退出后没人再拉起它。
    pub fn with_self_update_target(mut self, target: SelfUpdateTarget) -> Self {
        self.self_update = Some(target);
        self
    }

    /// 取一次「自更新已落盘、请退出让服务管理器拉起」的标记（取后清零）。
    pub fn take_restart_requested(&self) -> bool {
        self.restart_requested.swap(false, Ordering::SeqCst)
    }

    /// 驱动一次升级：按**组件**判机制 —— 自身（[`SELF_COMPONENT`]）走自更新，
    /// 标了 `tool-copy` 的组件走 [`Self::start_tool`]（进程内装无状态工具），
    /// 其余走 [`Self::start_gops`]（gops 执行器）。
    ///
    /// 不额外声明期望摘要：无状态工具路径会从**内容寻址的制品名**（`pkg-<hex16>`）自推前缀校验。
    pub async fn start(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        component: Option<&str>,
        reporter: Option<UpgradeReporter>,
        artifact_url: Option<&str>,
    ) -> UpgradeResult<()> {
        self.start_with_digest(
            work_id,
            from_version,
            to_version,
            component,
            reporter,
            artifact_url,
            None,
        )
        .await
    }

    /// 同 [`Self::start`]，但额外带**中心契约的期望摘要**（`artifact_sha256`）。
    ///
    /// gops 路径忽略它（执行器自己取件自验）；copy / 自更新路径用它该校取的制品。
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_digest(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        component: Option<&str>,
        reporter: Option<UpgradeReporter>,
        artifact_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> UpgradeResult<()> {
        if component == Some(SELF_COMPONENT) {
            return self
                .start_self_update(
                    work_id,
                    from_version,
                    to_version,
                    reporter,
                    artifact_url,
                    expected_sha256,
                )
                .await;
        }
        if let (Some(tools), Some(component)) = (self.tools.as_ref(), component)
            && tools.handles(component)
        {
            return self
                .start_tool(
                    work_id,
                    from_version,
                    to_version,
                    component,
                    reporter,
                    artifact_url,
                    expected_sha256,
                )
                .await;
        }
        self.start_gops(
            work_id,
            from_version,
            to_version,
            component,
            reporter,
            artifact_url,
            expected_sha256,
        )
        .await
    }

    /// 驱动一次**自更新**：把中心发布的 `wist-gwlinkd` 制品**就地覆盖到本进程自己**。
    ///
    /// 与 [`Self::start_tool`] 的差别：
    /// - **覆盖目标**是服务定义里的二进制（经符号链接解析到真身），不是 `PATH` 上的某个工具；
    /// - **必须由服务管理器托管**：覆盖后本进程要退出，靠 systemd `Restart=always` / launchd
    ///   `KeepAlive` 拉起新版本；手动 `run` 的进程退出后没人再拉起它；
    /// - **成功不在当下回执**：写 `restarting` 记录后退出，新进程启动时用自身版本核对，
    ///   再把 `done` / `failed` 回执中心（见 [`settle_pending_self_update`]）。
    ///
    /// 失败也落终态并回执，然后返回 `Ok`：这份计划已经“驱过”，主循环据此落游标，
    /// 不每轮重驱；重试由中心新建补跑计划（与 tool-copy 同一取舍）。
    #[allow(clippy::too_many_arguments)]
    async fn start_self_update(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        reporter: Option<UpgradeReporter>,
        artifact_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> UpgradeResult<()> {
        let mut record = UpgradeRecord {
            work_id: work_id.to_string(),
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            step: "preflight".to_string(),
            status: "running".to_string(),
            detail: String::new(),
            log_dir: None,
        };

        let Some(target) = self.self_update.as_ref() else {
            return self
                .fail_self_update(
                    &mut record,
                    "preflight",
                    "未检测到已安装的服务定义：自更新必须由 systemd / launchd 托管（先 `wist-gwlinkd service install`）；手动 run 的进程退出后没人拉起",
                    reporter.as_ref(),
                )
                .await;
        };
        let Some(tools) = self.tools.as_ref() else {
            return self
                .fail_self_update(
                    &mut record,
                    "preflight",
                    "自更新要求装配 ToolInstaller（with_tools）—— 这是内部接线错误",
                    reporter.as_ref(),
                )
                .await;
        };

        // 服务定义里的二进制必须就是当前进程：否则覆盖的是“别人”，退出后也不会拉起它。
        let service_bin = canonical(&target.service_bin);
        let running_exe = canonical(&target.running_exe);
        if service_bin != running_exe {
            return self
                .fail_self_update(
                    &mut record,
                    "preflight",
                    format!(
                        "服务定义指向 {}，当前进程是 {}：不覆盖别的二进制（请确认服务由同一份安装拉起）",
                        service_bin.display(),
                        running_exe.display()
                    ),
                    reporter.as_ref(),
                )
                .await;
        }

        // 写权限要在下载前就判：root 拥有 + 非 root 运行是安装形态问题，不是网络问题。
        let Some(dir) = service_bin.parent() else {
            return self
                .fail_self_update(
                    &mut record,
                    "preflight",
                    format!("自更新目标没有父目录：{}", service_bin.display()),
                    reporter.as_ref(),
                )
                .await;
        };
        let probe = dir.join(format!(".wist-gwlinkd-self-update.{}", std::process::id()));
        if let Err(err) = std::fs::write(&probe, b"") {
            let _ = std::fs::remove_file(&probe);
            return self
                .fail_self_update(
                    &mut record,
                    "preflight",
                    format!(
                        "二进制目录不可写（{}）：{err}。自更新要求安装目录对运行用户可写；\
                         非 root 服务请重跑中心「只装 gwlinkd」命令（真实二进制会落到运行用户可写目录）",
                        dir.display()
                    ),
                    reporter.as_ref(),
                )
                .await;
        }
        let _ = std::fs::remove_file(&probe);

        // 进入安装：写 running + 心跳（取件 / 校验可能跨多秒）。
        state::write_upgrade_record(&self.state_dir, &record)?;
        state::touch_heartbeat(&self.state_dir)?;
        let heartbeat_state = self.state_dir.clone();
        let _heartbeat = AbortOnDrop(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
            loop {
                ticker.tick().await;
                if let Err(err) = state::touch_heartbeat(&heartbeat_state) {
                    log::warn!(
                        "event=UpgradeHeartbeatFailed error={}",
                        chain_one_line(&err)
                    );
                }
            }
        }));

        let binary_name = service_bin
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(SELF_COMPONENT);
        let detail = match tools
            .install_to(
                SELF_COMPONENT,
                binary_name,
                &service_bin,
                artifact_url.unwrap_or(to_version),
                expected_sha256,
            )
            .await
        {
            Ok(detail) => detail,
            Err(reason) => {
                return self
                    .fail_self_update(
                        &mut record,
                        "install",
                        reason.to_string(),
                        reporter.as_ref(),
                    )
                    .await;
            }
        };

        // 覆盖成功：**先不回执**。新进程启动时会核对版本并回执（见 `settle_pending_self_update`）。
        record.status = "restarting".to_string();
        record.step = "restart".to_string();
        record.detail =
            format!("{detail}；新版本已落盘，进程退出后由服务管理器拉起（重启后核对版本并回执）");
        if let Err(err) = state::write_upgrade_record(&self.state_dir, &record) {
            log::warn!(
                "event=UpgradeRecordWriteFailed error={}",
                chain_one_line(&err)
            );
        }
        self.restart_requested.store(true, Ordering::SeqCst);
        log::info!(
            "event=SelfUpdateRestarting work_id={work_id} from_version={from_version} to_version={to_version}"
        );
        Ok(())
    }

    /// 自更新前置/安装失败：落可读终态 + 回执。返回 `Ok` 让调用方落游标（见 [`Self::start_self_update`]）。
    async fn fail_self_update(
        &self,
        record: &mut UpgradeRecord,
        step: &str,
        reason: impl Into<String>,
        reporter: Option<&UpgradeReporter>,
    ) -> UpgradeResult<()> {
        record.step = step.to_string();
        record.status = "failed".to_string();
        record.detail = reason.into();
        log::error!(
            "event=SelfUpdateFailed work_id={} step={} detail={}",
            record.work_id,
            record.step,
            record.detail
        );
        if let Err(err) = state::write_upgrade_record(&self.state_dir, record) {
            log::warn!(
                "event=UpgradeRecordWriteFailed error={}",
                chain_one_line(&err)
            );
        }
        if let Some(reporter) = reporter {
            report_record(reporter, record).await;
        }
        Ok(())
    }

    /// 驱动一次升级（gops 执行器）：写 `running` 记录，起执行器，**升级期间持续刷心跳**；执行器结束后写终态记录，
    /// （若报成）用自述面**佐证**「网关确实回来了」，再（若给了 `reporter`）**回执**中心。
    ///
    /// 心跳生产者就是本进程：它持有子进程句柄，知道执行器还活着 —— 这正是「判死判据」需要的信号源
    /// （判据在 [`state::upgrader_is_declared_dead`]，但**必须有生产者**，否则长升级会被判假死）。
    ///
    /// `component` 来自升级计划：当执行器未配固定系统名时，作为 gops 的 NAME 传入（只升该系统）。
    ///
    /// [`artifact_url`]：中心**派生**的制品下发地址。执行器取件的 `--to` 用它（`gops --to <url>`）；
    /// 为 `None`（无对应 release）才回落用 [`to_version`]。**台账与回执仍记 `to_version`**（版本）。
    #[allow(clippy::too_many_arguments)]
    async fn start_gops(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        component: Option<&str>,
        reporter: Option<UpgradeReporter>,
        artifact_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> UpgradeResult<()> {
        let mut record = UpgradeRecord {
            work_id: work_id.to_string(),
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            step: "fetch".to_string(),
            status: "running".to_string(),
            detail: String::new(),
            log_dir: None,
        };
        state::write_upgrade_record(&self.state_dir, &record)?;
        state::touch_heartbeat(&self.state_dir)?;

        // 任务执行记录（P0）：每个 plan 一个目录，落 task.json / events / 执行器输出。
        // 建失败只告警，不影响升级。
        let task = task_log::TaskLog::create(
            &self.state_dir,
            "upgrade",
            work_id,
            serde_json::json!({
                "component": component,
                "from_version": from_version,
                "to_version": to_version,
                "artifact_url": artifact_url,
                "expected_sha256": expected_sha256,
                "executor": self.executor.program(),
            }),
        );
        if let Some(task) = &task {
            task.event("run.start", None);
        }

        // 前置校验：执行器缺必要配置（gops 的工程根）就**别发** —— 发出去也只会以退出码失败，
        // 回执读不出原因。这里前置成可读失败，并照常回执（否则中心条目会停在 dispatched）。
        if let Err(reason) = self.executor.preflight() {
            record.step = "preflight".to_string();
            record.status = "failed".to_string();
            record.detail = reason.to_string();
            append_task_dir(&task, &mut record);
            if let Err(werr) = state::write_upgrade_record(&self.state_dir, &record) {
                log::warn!(
                    "event=UpgradeRecordWriteFailed error={}",
                    chain_one_line(&werr)
                );
            }
            if let Some(reporter) = reporter {
                report_record(&reporter, &record).await;
            }
            return Err(reason);
        }

        // 取件下沉：**gops 不持中心信任**（私有 CA 下 HEAD/GET 中心地址直接 `error sending request`）。
        //  gwlinkd 用自己的制品客户端（带 CA-S / 客户端证书）先取到本地，再给执行器一个本机路径。
        //  （`gops prj upgrade --to <URL|PATH>` 两种都收；本地路径还能顺带按中心摘要校验。）
        let staged = match (artifact_url, self.artifact_http.as_ref()) {
            (Some(url), Some(client)) if is_http_url(url) => {
                match stage_artifact(&self.state_dir, client, url, expected_sha256).await {
                    Ok(path) => Some(path),
                    Err(reason) => {
                        record.step = "fetch".to_string();
                        record.status = "failed".to_string();
                        record.detail = reason.to_string();
                        if let Err(werr) = state::write_upgrade_record(&self.state_dir, &record) {
                            log::warn!(
                                "event=UpgradeRecordWriteFailed error={}",
                                chain_one_line(&werr)
                            );
                        }
                        if let Some(reporter) = reporter {
                            report_record(&reporter, &record).await;
                        }
                        return Err(reason);
                    }
                }
            }
            // 无客户端 / 非 HTTP 来源（本机路径 / 版本号）：交给执行器原处理。
            _ => None,
        };
        // 本机路径必须活到 `command()` 构建完（`ExecutorInvocation` 借用它）。
        let staged_to = staged
            .as_ref()
            .map(|path| path.to_string_lossy().to_string());

        // 执行器的 stdout/stderr 落日志文件（失败时运维要能看现场）。stderr 由驱动接管；
        // stdout 由适配器置为管道，供解读结局。
        let log_path = self.state_dir.join(UPGRADER_LOG_FILE);
        // 本次运行前日志的长度：失败时只读**本次新增**的那段（不把上次的错误也带进回执）。
        let stderr_offset = std::fs::metadata(&log_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        let stderr = match &log {
            Some(file) => Stdio::from(
                file.try_clone()
                    .map_err(|err| UpgradeReason::Io.err(format!("复制日志句柄失败: {err}")))?,
            ),
            None => Stdio::null(),
        };

        let invocation = ExecutorInvocation {
            // 执行器取件的 `--to`：优先用**已取到本地**的路径，其次中心地址，最后回落版本。
            to_version: staged_to.as_deref().or(artifact_url).unwrap_or(to_version),
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
                append_task_dir(&task, &mut record);
                if let Err(werr) = state::write_upgrade_record(&self.state_dir, &record) {
                    log::warn!(
                        "event=UpgradeRecordWriteFailed error={}",
                        chain_one_line(&werr)
                    );
                }
                // 回执：否则中心条目会停在 dispatched（起不来也是终态，得让中心知道）。
                if let Some(reporter) = reporter {
                    report_record(&reporter, &record).await;
                }
                return Err(UpgradeReason::Executor.err(detail));
            }
        };
        let stdout = child.stdout.take();

        let state_dir = self.state_dir.clone();
        let executor = Arc::clone(&self.executor);
        let verify_timeout = self.verify_timeout;
        let log_path = log_path.clone();
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
                        log::warn!(
                            "event=UpgradeHeartbeatFailed error={}",
                            chain_one_line(&err)
                        );
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
            // 本次运行 stderr 的范围：**先读一次**，失败回执与任务日志共用。
            let stderr_tail = read_run_stderr(&log_path, stderr_offset, 4096);
            // 失败时把**本次运行**的执行器 stderr（关键错误行）折进回执说明 ——
            // 只留退出码（255）等于让人去翻日志。
            let fallback = if ok || stderr_tail.is_empty() {
                detail
            } else {
                format!("{detail}; {}", oneline(&stderr_tail))
            };
            let mut outcome = executor.interpret(ok, &raw, fallback);

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
                    log::warn!(
                        "event=UpgradeRecordWriteFailed error={}",
                        chain_one_line(&err)
                    );
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
                                        append_detail(&mut outcome.detail, reason.to_string());
                                        log::warn!(
                                            "event=UpgradeUnverified work_id={} {}",
                                            record.work_id,
                                            chain_one_line(&reason)
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
            if let Some(task) = &task {
                task.output(&format!(
                    "--- stdout ---\n{raw}\n--- stderr ---\n{stderr_tail}\n"
                ));
            }
            append_task_dir(&task, &mut record);
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                log::warn!(
                    "event=UpgradeRecordWriteFailed error={}",
                    chain_one_line(&err)
                );
            }
            if let Some(reporter) = reporter {
                // 回执**现读**最新凭据：升级可能跨越一次 renew（旧证书立即失效）。
                report_record(&reporter, &record).await;
            }
        });
        Ok(())
    }

    /// 驱动一次升级（**进程内装无状态工具**）：写 `running` 记录 → 后台取制品 / 核摘要 / 核架构 / 解包 / 覆盖。
    ///
    /// 与 [`Self::start_gops`] 的差别：**不起子进程**（解包 + 覆盖在本进程 blocking 线程里做）、
    /// **不做成功佐证**（工具不影响网关容器/自述面 —— 佐证无对象）。取制品用带信任锚的 HTTP 客户端。
    #[allow(clippy::too_many_arguments)]
    async fn start_tool(
        &self,
        work_id: &str,
        from_version: &str,
        to_version: &str,
        component: &str,
        reporter: Option<UpgradeReporter>,
        artifact_url: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> UpgradeResult<()> {
        let Some(tools) = self.tools.as_ref() else {
            // 内部接线错误：`tool` 升级路径必须经 `with_tools` 装配 `ToolInstaller`。
            // 返回错误而非 panic —— 常驻进程里 panic 会整机停。
            return Err(UpgradeReason::Preflight
                .err("tool 安装路径要求装配 ToolInstaller（with_tools）—— 这是内部接线错误"));
        };
        let tools = Arc::clone(tools);
        let mut record = UpgradeRecord {
            work_id: work_id.to_string(),
            from_version: from_version.to_string(),
            to_version: to_version.to_string(),
            step: "fetch".to_string(),
            status: "running".to_string(),
            detail: String::new(),
            log_dir: None,
        };
        state::write_upgrade_record(&self.state_dir, &record)?;
        state::touch_heartbeat(&self.state_dir)?;

        // 前置：组件在目录里、binary 能定位（找不到就**别发** —— 发出去只会以不可读的方式失败）。
        if let Err(reason) = tools.preflight(component).map(|_| ()) {
            record.step = "preflight".to_string();
            record.status = "failed".to_string();
            record.detail = reason.to_string();
            if let Err(werr) = state::write_upgrade_record(&self.state_dir, &record) {
                log::warn!(
                    "event=UpgradeRecordWriteFailed error={}",
                    chain_one_line(&werr)
                );
            }
            if let Some(reporter) = reporter {
                report_record(&reporter, &record).await;
            }
            return Err(reason);
        }

        let state_dir = self.state_dir.clone();
        let component = component.to_string();
        let artifact = artifact_url.unwrap_or(to_version).to_string();
        // 摘要要在 spawn 前转成 owned（异步块要求 'static）。
        let expected_sha256 = expected_sha256.map(str::to_string);
        tokio::spawn(async move {
            // 心跳生产者覆盖**整个安装事务**（取制品 + 解包 + 覆盖可能跨多秒）。
            let heartbeat_state = state_dir.clone();
            let _heartbeat = AbortOnDrop(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
                loop {
                    ticker.tick().await;
                    if let Err(err) = state::touch_heartbeat(&heartbeat_state) {
                        log::warn!(
                            "event=UpgradeHeartbeatFailed error={}",
                            chain_one_line(&err)
                        );
                    }
                }
            }));

            // 进安装步就把台账步进到 install，诊断看起来才与事实一致。
            record.step = "install".to_string();
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                log::warn!(
                    "event=UpgradeRecordWriteFailed error={}",
                    chain_one_line(&err)
                );
            }

            let outcome = match logged_op(
                module_path!(),
                "tool install",
                &[("component", component.clone())],
                tools
                    .install_with_digest(&component, &artifact, expected_sha256.as_deref())
                    .await,
            ) {
                Ok(detail) => Outcome {
                    ok: true,
                    status: "done".to_string(),
                    step: "install".to_string(),
                    detail,
                },
                Err(reason) => {
                    log::warn!(
                        "event=ToolInstallFailed component={component} {}",
                        chain_one_line(&reason)
                    );
                    Outcome {
                        ok: false,
                        status: "failed".to_string(),
                        step: "install".to_string(),
                        detail: reason.to_string(),
                    }
                }
            };

            record.status = outcome.status;
            record.step = outcome.step;
            record.detail = outcome.detail;
            if let Err(err) = state::write_upgrade_record(&state_dir, &record) {
                log::warn!(
                    "event=UpgradeRecordWriteFailed error={}",
                    chain_one_line(&err)
                );
            }
            if let Some(reporter) = reporter {
                // 回执**现读**最新凭据（安装可能跨多次心跳，期间可能已 renew）。
                report_record(&reporter, &record).await;
            }
        });
        Ok(())
    }
}

/// 把一条终态记录回执给中心（现读最新凭据；无长期身份 / 网络失败返回 `false`）。
async fn report_record(reporter: &UpgradeReporter, record: &UpgradeRecord) -> bool {
    match state::load_credential(&reporter.identity_dir) {
        Some(credential) => match reporter
            .client
            .report_upgrade_result(&credential.bundle.gateway_id, record)
            .await
        {
            Ok(()) => {
                log::info!("event=UpgradeReported work_id={}", record.work_id);
                true
            }
            Err(err) => {
                log::warn!("event=UpgradeReportFailed error={}", chain_one_line(&err));
                false
            }
        },
        None => {
            log::warn!("event=UpgradeReportSkipped 无长期身份");
            false
        }
    }
}

/// 重启后收尾**自更新**：上一条记录停在 `restarting` 说明上一进程已换完二进制并退出。
///
/// 用**当前进程的编译版本**核对计划目标：一致 → `done`；不一致 → `failed`（写明两侧版本）。
/// 核对完才回执中心 —— 这是自更新路径的“成功佐证”（不能只信“文件已覆盖”）。
pub async fn settle_pending_self_update(
    state_dir: &Path,
    identity_dir: &Path,
    client: CenterClient,
    current_version: &str,
) {
    let Some(record) = state::read_upgrade_record(state_dir) else {
        return;
    };
    if record.status != "restarting" {
        return;
    }

    let (status, step, detail) = self_update_settlement(&record.to_version, current_version);
    let mut settled = record.clone();
    settled.status = status;
    settled.step = step;
    settled.detail = detail;

    let reporter = UpgradeReporter {
        client,
        state_dir: state_dir.to_path_buf(),
        identity_dir: identity_dir.to_path_buf(),
        self_client: None,
    };
    // 先回执、后落终态：回执失败就保持 `restarting`，下一轮再试（不把“没送到”记成“已完成”）。
    if !report_record(&reporter, &settled).await {
        log::warn!(
            "event=SelfUpdateSettleRetry work_id={} status={}（回执未送达，保持 restarting）",
            settled.work_id,
            settled.status
        );
        return;
    }
    if let Err(err) = state::write_upgrade_record(state_dir, &settled) {
        log::warn!(
            "event=UpgradeRecordWriteFailed error={}",
            chain_one_line(&err)
        );
    }
    log::info!(
        "event=SelfUpdateSettled work_id={} status={} current_version={}",
        settled.work_id,
        settled.status,
        current_version
    );
}

/// 自更新的版本判定（纯函数，便于测试）：`(status, step, detail)`。
fn self_update_settlement(to_version: &str, current_version: &str) -> (String, String, String) {
    let expected = normalize_version(to_version);
    let current = normalize_version(current_version);
    if expected.is_empty() {
        return (
            "failed".to_string(),
            "verify".to_string(),
            "重启后无法核对版本：计划未带目标版本".to_string(),
        );
    }
    if versions_match(&expected, &current) {
        (
            "done".to_string(),
            "verify".to_string(),
            format!("重启后自更新完成：当前版本 {current_version}"),
        )
    } else {
        (
            "failed".to_string(),
            "verify".to_string(),
            format!(
                "重启后版本不符：当前 {current_version}，期望 {to_version}（未回到目标版本，请查看服务日志）"
            ),
        )
    }
}

/// 两侧版本是不是**同一个版本**：先比归一后的整串，再比**数字核心**。
///
/// 为什么还要比核心：中心记的版本来自**制品文件名 / tag**（`v0.13.1-alpha`），而二进制自报的
/// 是 `version.txt`（`0.13.1`）—— 通道后缀（`-alpha` / `-beta`）只存在于 tag，不在包版本里。
/// 只比整串会把一次成功的自更新判成「版本不符」（实测：当前 0.13.1，期望 v0.13.1-alpha）。
///
/// 反过来 `0.13.1` 与 `0.13.2` 的核心不同，仍然判失败 —— 不放宽真实的版本差异；
/// 认不出的版本串也不猜（回落到整串比较的结果）。
fn versions_match(expected: &str, current: &str) -> bool {
    if expected == current {
        return true;
    }
    match (
        wist_artifact::version::parse_version(expected),
        wist_artifact::version::parse_version(current),
    ) {
        (Some(expected), Some(current)) => expected == current,
        _ => false,
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("https://") || value.starts_with("http://")
}

/// 取中心制品到 `<state>/artifacts/`（可选校验摘要），返回**本机路径**。
///
/// 为什么由 gwlinkd 取：`gops` 不持中心信任，把中心地址交给它会在 HEAD 阶段就
/// `error sending request`（私有 CA）。取包下沉到持信任的组件是既定分层
/// （见 `wist-design/doc/design/edge/center-content-delivery.md`）。
async fn stage_artifact(
    state_dir: &Path,
    client: &reqwest::Client,
    url: &str,
    expected_sha256: Option<&str>,
) -> UpgradeResult<PathBuf> {
    let filename = artifact_filename(url)?;
    let bytes = wist_artifact::source::read_source_with_client(
        client,
        url,
        wist_artifact::source::MAX_ARTIFACT_BYTES,
        wist_artifact::source::FETCH_TIMEOUT,
    )
    .await
    .map_err(|err| UpgradeReason::Artifact.err(format!("取包失败 {url}: {err}")))?;

    // 中心带摘要就核（① 升级目前不带，仅 gwlinkd 自更新 / ② 带）。
    if let Some(expected) = expected_sha256
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let expected = wist_artifact::digest::parse_digest(expected).map_err(|err| {
            UpgradeReason::Artifact.err(format!("期望摘要形态不对（{expected}）：{err}"))
        })?;
        let actual = wist_artifact::digest::sha256_hex_bytes(&bytes);
        if actual != expected {
            return Err(UpgradeReason::Artifact
                .err(format!("制品摘要不符：期望 {expected}，实得 {actual}")));
        }
    }

    let dir = state_dir.join("artifacts");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| UpgradeReason::Io.err(format!("建制品目录失败 {}: {err}", dir.display())))?;
    let target = dir.join(&filename);
    // 先写 `<name>.partial` 再改名：半截文件不会被执行器读到。
    let partial = dir.join(format!("{filename}.partial"));
    tokio::fs::write(&partial, &bytes)
        .await
        .map_err(|err| UpgradeReason::Io.err(format!("写制品失败 {}: {err}", partial.display())))?;
    tokio::fs::rename(&partial, &target).await.map_err(|err| {
        UpgradeReason::Io.err(format!("落盘制品失败 {}: {err}", target.display()))
    })?;

    prune_artifacts(&dir, 5).await;
    Ok(target)
}

/// 从制品地址取**安全文件名**（末段，去 query / fragment）。
fn artifact_filename(url: &str) -> UpgradeResult<String> {
    let name = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(UpgradeReason::Artifact.err(format!("从制品地址取不出安全文件名：{url}")));
    }
    Ok(name.to_string())
}

/// 制品目录是升级中转区：只留最新 `keep` 份（防止无界增长）。失败只告警。
async fn prune_artifacts(dir: &Path, keep: usize) {
    let dir = dir.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let meta = entry.metadata().ok()?;
                if !meta.is_file() {
                    return None;
                }
                Some((meta.modified().unwrap_or(std::time::UNIX_EPOCH), path))
            })
            .collect();
        files.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
        for (_, path) in files.into_iter().skip(keep) {
            let _ = std::fs::remove_file(path);
        }
    })
    .await;
}

/// 版本串归一化：去空白 + 去前导 `v` / `V`（中心 release 版本可能带 `v`，编译版本不带）。
fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches(['v', 'V']).to_string()
}

/// 能规范化就规范化（路径不存在时原样返回，交给后续可读失败）。
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 读执行器日志里**本次运行**新增的那段（有界）。失败时把关键行进回执，少让人翻日志。
fn read_run_stderr(path: &std::path::Path, offset: u64, cap: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    if len <= offset {
        return String::new();
    }
    // 只取尾部 `cap` 字节（错误行通常在末尾），且不越过本次运行的起点。
    let start = offset.max(len.saturating_sub(cap));
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    let _ = file.take(cap).read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).trim().to_string()
}

/// 把多行错误折成一行（折叠空白），便于进 `detail`。
fn oneline(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

/// 把任务日志目录拼进回执说明。
///
/// P0 阶段先用 `detail` 承载（中心原样展示）；P1 再把 `log_dir` 做成独立字段。
fn append_task_dir(task: &Option<std::sync::Arc<task_log::TaskLog>>, record: &mut UpgradeRecord) {
    let Some(task) = task else {
        return;
    };
    let dir = task.finish(&record.status, &record.detail);
    record.log_dir = Some(dir.clone());
    append_detail(&mut record.detail, format!("日志: {dir}"));
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
) -> UpgradeResult<String> {
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
            Ok(Err(err)) => err.to_string(),
            Err(_) => "自述面探测超时".to_string(),
        };
        if Instant::now() >= deadline {
            return Err(UpgradeReason::Recovery.err(format!(
                "执行器报成但未能佐证（{}s 内）：{last}",
                timeout.as_secs()
            )));
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
    use crate::target::HostTarget;
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

    /// 测试用 gops 执行器：`program` 的 basename 必须是 `gops`（那才走 gops 的前置校验），
    /// 并给一个含 `ops-prj.yml` 的工程根。
    fn gops_script(dir: &Path, program: String) -> GopsExecutor {
        std::fs::write(dir.join("ops-prj.yml"), "kind: ops\n").expect("write ops-prj.yml");
        GopsExecutor::new(program).with_project(Some(dir.to_path_buf()), None)
    }

    /// 造一个 tar.gz：包内单条目 `entry` = `payload`。
    fn tool_archive(entry: &str, payload: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, entry, payload)
                .expect("append");
            builder.finish().expect("finish");
        }
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_bytes).expect("gzip");
        encoder.finish().expect("gzip finish")
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
            .start("w-1", "0.1.0", "0.1.16", None, None, None)
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
            .start("w-2", "0.1.0", "0.1.16", None, None, None)
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
            .start("w-3", "0.1.0", "0.1.16", Some("gw-stack"), None, None)
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
    async fn start_prefers_the_derived_artifact_url_over_the_version() {
        let dir = temp_dir("args-artifact");
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        );
        let url = "https://center.example/api/v1/releases/artifact/warp-gateway/0.1.27/warp-gateway-0.1.27.tar.gz";
        driver
            .start("w-url", "0.1.0", "0.1.27", None, None, Some(url))
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(args.contains(&format!("--to {url}")), "{args}");
        // 台账/回执仍记**版本**（不是 URL）——回执语义要的是版本。
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.to_version, "0.1.27");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 极简制品桩：任意 GET 都回这段字节（用于「取件下沉」用例）。
    async fn serve_bytes(bytes: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let bytes = bytes.clone();
                tokio::spawn(async move {
                    let mut buf = [0_u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&bytes).await;
                    let _ = sock.flush().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// ① 升级的取件也下沉到 gwlinkd：装上制品客户端后，执行器拿到的是**本机路径**而不是中心地址
    /// （gops 不持中心信任，拿中心地址会在 HEAD 阶段 `error sending request`）。
    #[tokio::test]
    async fn start_stages_a_center_artifact_locally_before_calling_the_executor() {
        let dir = temp_dir("args-staged");
        let payload = b"fake-gateway-stack-package".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let base = serve_bytes(payload.clone()).await;
        let url = format!("{base}/wist-gateway-stack-v0.1.44-alpha.tar.gz");

        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_artifact_client(reqwest::Client::new());
        driver
            .start_with_digest(
                "w-stage",
                "0.1.43",
                "v0.1.44-alpha",
                Some("wist-gateway-stack"),
                None,
                Some(&url),
                Some(&digest),
            )
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;

        let staged = dir.join("artifacts/wist-gateway-stack-v0.1.44-alpha.tar.gz");
        assert_eq!(
            std::fs::read(&staged).unwrap_or_else(|err| panic!("{}: {err}", staged.display())),
            payload,
            "制品要落成可读的本机文件"
        );
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(
            !args.contains("http://"),
            "执行器不该再拿到（它取不到的中心）地址：{args}"
        );
        assert!(
            args.contains(&format!("--to {}", staged.display())),
            "--to 应是本机路径：{args}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 摘要不符：**不发执行器**，落可读失败（否则 gops 会拿错包去升级）。
    #[tokio::test]
    async fn start_refuses_a_staged_artifact_with_the_wrong_digest() {
        let dir = temp_dir("args-staged-digest");
        let payload = b"actual-bytes".to_vec();
        let base = serve_bytes(payload).await;
        let url = format!("{base}/wist-gateway-stack-v0.1.44-alpha.tar.gz");
        let wrong = "0".repeat(64);

        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_artifact_client(reqwest::Client::new());
        let err = driver
            .start_with_digest(
                "w-stage-bad",
                "0.1.43",
                "v0.1.44-alpha",
                Some("wist-gateway-stack"),
                None,
                Some(&url),
                Some(&wrong),
            )
            .await
            .expect_err("digest mismatch must fail");
        assert!(err.to_string().contains("摘要不符"), "{err}");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(record.detail.contains("摘要不符"), "{}", record.detail);
        assert!(!dir.join("args.txt").exists(), "摘要不符时执行器不该被启动");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 非 HTTP 来源（本机路径 / 版本号）**不取件**，原样交给执行器。
    ///
    /// 这条钉住「下沉只针对中心地址」：有人在现场用本地包路径时，gwlinkd 不该去拉它。
    #[tokio::test]
    async fn start_passes_a_local_path_through_without_staging() {
        let dir = temp_dir("args-local-path");
        let local = dir.join("wist-gateway-stack-local.tar.gz");
        std::fs::write(&local, b"local-package").expect("write local");

        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        // 即使装了取件客户端，非 HTTP 来源也不该走网络。
        .with_artifact_client(reqwest::Client::new());
        driver
            .start_with_digest(
                "w-local",
                "0.1.43",
                "v0.1.44-alpha",
                Some("wist-gateway-stack"),
                None,
                Some(local.to_string_lossy().as_ref()),
                None,
            )
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;

        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(
            args.contains(&format!("--to {}", local.display())),
            "本机路径应原样透传：{args}"
        );
        assert!(
            !dir.join("artifacts").exists(),
            "非 HTTP 来源不该产生制品中转文件"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 从制品地址取安全文件名：去 query / fragment，拒空 / `.` / `..` / 分隔符 / 控制字符。
    #[test]
    fn artifact_filename_keeps_only_a_safe_last_segment() {
        assert_eq!(
            artifact_filename("https://c/a/wist-gateway-stack-v1.tar.gz?x=1").unwrap(),
            "wist-gateway-stack-v1.tar.gz"
        );
        assert_eq!(
            artifact_filename("https://c/a/p.tar.gz#frag").unwrap(),
            "p.tar.gz"
        );
        for bad in ["https://c/", "https://c/..", "https://c/a\\b.tar.gz"] {
            assert!(artifact_filename(bad).is_err(), "{bad}");
        }
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
            .start("w-4", "0.1.0", "0.1.16", None, None, None)
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

    /// 自更新：覆盖**本进程自己的二进制**、落 `restarting`、请主循环退出让服务管理器拉起。
    #[tokio::test]
    async fn a_self_update_replaces_its_own_binary_and_requests_restart() {
        use std::collections::BTreeMap;

        let dir = temp_dir("self-update");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "wist-gwlinkd", "OLD");
        let artifact = dir.join("wist-gwlinkd.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive(
                "wist-gwlinkd-0.12.0-aarch64-apple-darwin/wist-gwlinkd",
                b"NEW",
            ),
        )
        .expect("artifact");

        let tools = ToolInstaller::new(BTreeMap::new(), &dir, reqwest::Client::new())
            .with_host_target(HostTarget::new("aarch64", "macos", true));
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_tools(tools)
        .with_self_update_target(SelfUpdateTarget::new(target.clone(), target.clone()));
        driver
            .start(
                "w-self",
                "0.11.0",
                "0.12.0",
                Some(SELF_COMPONENT),
                None,
                Some(artifact.to_string_lossy().as_ref()),
            )
            .await
            .expect("self update is handled");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "restarting", "{}", record.detail);
        assert_eq!(record.step, "restart");
        assert!(
            record.detail.contains("服务管理器拉起"),
            "{}",
            record.detail
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        // 取一次就清零（主循环只该退出一次）。
        assert!(driver.take_restart_requested());
        assert!(!driver.take_restart_requested());
        assert!(!dir.join("args.txt").exists(), "自更新不该走 gops 执行器");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没装成服务（无自更新目标）→ 发之前就落可读失败，且**不请求退出**（手动 run 退出后没人拉起）。
    #[tokio::test]
    async fn a_self_update_without_a_managed_service_fails_before_fetch() {
        let dir = temp_dir("self-update-unmanaged");
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start(
                "w-self",
                "0.11.0",
                "0.12.0",
                Some(SELF_COMPONENT),
                None,
                None,
            )
            .await
            .expect("failure is recorded, not propagated");

        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "failed");
        assert_eq!(record.step, "preflight");
        assert!(record.detail.contains("服务定义"), "{}", record.detail);
        assert!(!driver.take_restart_requested());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 服务定义指向的二进制 ≠ 当前进程 → 不覆盖「别人」（覆盖了也不会被拉起）。
    #[tokio::test]
    async fn a_self_update_refuses_a_service_bin_that_is_not_the_running_exe() {
        use std::collections::BTreeMap;

        let dir = temp_dir("self-update-other-bin");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let service_bin = write_exec(&bin_dir, "wist-gwlinkd", "SERVICE");
        let running_exe = write_exec(&bin_dir, "gwlinkd-dev", "RUNNING");

        let tools = ToolInstaller::new(BTreeMap::new(), &dir, reqwest::Client::new());
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_tools(tools)
        .with_self_update_target(SelfUpdateTarget::new(service_bin, running_exe));
        driver
            .start(
                "w-self",
                "0.11.0",
                "0.12.0",
                Some(SELF_COMPONENT),
                None,
                None,
            )
            .await
            .expect("failure is recorded");

        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "failed");
        assert!(
            record.detail.contains("不覆盖别的二进制"),
            "{}",
            record.detail
        );
        assert!(!driver.take_restart_requested());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 重启后的版本核对：`v` 前缀归一化；不一致 / 计划没带版本都算失败。
    #[test]
    fn self_update_settlement_compares_normalized_versions() {
        assert_eq!(
            self_update_settlement("v0.12.0-alpha", "0.12.0-alpha").0,
            "done"
        );
        assert_eq!(self_update_settlement("0.12.0", "0.11.0").0, "failed");
        assert_eq!(self_update_settlement("", "0.11.0").0, "failed");
    }

    /// 回归：中心记的是 **tag**（`v0.13.1-alpha`），二进制自报的是 **`version.txt`**（`0.13.1`）。
    /// 通道后缀只在 tag 上 —— 只比整串会把一次成功的自更新判成「版本不符」。
    #[test]
    fn self_update_settlement_treats_a_channel_suffix_as_the_same_version() {
        for (target, current) in [
            ("v0.13.1-alpha", "0.13.1"),
            ("0.13.1-alpha", "0.13.1"),
            ("v0.13.1-beta", "0.13.1"),
            ("0.13.1", "0.13.1"),
            ("  v0.13.1-alpha  ", "0.13.1"),
        ] {
            assert_eq!(
                self_update_settlement(target, current).0,
                "done",
                "{target} vs {current} 应判为同一版本"
            );
        }
        // 真实版本差异不能放宽：核心不同仍然失败。
        for (target, current) in [
            ("v0.13.2-alpha", "0.13.1"),
            ("v0.13.1-alpha", "0.13.0"),
            ("v0.14.0", "0.13.9"),
        ] {
            assert_eq!(
                self_update_settlement(target, current).0,
                "failed",
                "{target} vs {current} 是真实差异，不能判成功"
            );
        }
    }

    /// 自更新摘要不符 → 落可读失败、旧二进制原封不动、**不请求退出**。
    #[tokio::test]
    async fn a_self_update_with_a_digest_mismatch_leaves_the_binary_intact() {
        use std::collections::BTreeMap;

        let dir = temp_dir("self-update-digest");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "wist-gwlinkd", "OLD");
        let artifact = dir.join("wist-gwlinkd.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive(
                "wist-gwlinkd-0.12.0-aarch64-apple-darwin/wist-gwlinkd",
                b"NEW",
            ),
        )
        .expect("artifact");

        let tools = ToolInstaller::new(BTreeMap::new(), &dir, reqwest::Client::new())
            .with_host_target(HostTarget::new("aarch64", "macos", true));
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_tools(tools)
        .with_self_update_target(SelfUpdateTarget::new(target.clone(), target.clone()));
        driver
            .start_with_digest(
                "w-self",
                "0.11.0",
                "0.12.0",
                Some(SELF_COMPONENT),
                None,
                Some(artifact.to_string_lossy().as_ref()),
                // 与制品内容不符的完整摘要（形态合法）。
                Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
            )
            .await
            .expect("failure is recorded");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(record.detail.contains("摘要不符"), "{}", record.detail);
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        assert!(!driver.take_restart_requested());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 中心没派制品地址（只剩裸版本号）→ 发之前落可读失败，不覆盖、不退出。
    #[tokio::test]
    async fn a_self_update_without_an_artifact_url_fails_before_overwriting() {
        use std::collections::BTreeMap;

        let dir = temp_dir("self-update-no-url");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "wist-gwlinkd", "OLD");

        let tools = ToolInstaller::new(BTreeMap::new(), &dir, reqwest::Client::new());
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_tools(tools)
        .with_self_update_target(SelfUpdateTarget::new(target.clone(), target.clone()));
        driver
            .start(
                "w-self",
                "0.11.0",
                "0.12.0",
                Some(SELF_COMPONENT),
                None,
                None,
            )
            .await
            .expect("failure is recorded");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(
            record.detail.contains("没有可取的制品地址"),
            "{}",
            record.detail
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        assert!(!driver.take_restart_requested());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 重启后收尾：回执必须读**身份目录**里的凭据（与 state_dir 不同），成功后落 `done`。
    #[tokio::test]
    async fn settle_pending_self_update_reports_done_using_the_identity_dir() {
        let dir = temp_dir("settle-done");
        let state_dir = dir.join("state");
        let identity_dir = dir.join("identity");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::create_dir_all(&identity_dir).expect("identity dir");
        write_credential(&identity_dir, "gw-1");
        state::write_upgrade_record(
            &state_dir,
            &UpgradeRecord {
                work_id: "plan-1".into(),
                from_version: "0.11.0".into(),
                to_version: "v0.12.0".into(),
                step: "restart".into(),
                status: "restarting".into(),
                detail: String::new(),
                log_dir: None,
            },
        )
        .expect("record");

        let (endpoint, captured) = capture_one_request().await;
        settle_pending_self_update(
            &state_dir,
            &identity_dir,
            CenterClient::new(endpoint),
            "0.12.0",
        )
        .await;

        let request = captured.await.expect("captured request");
        assert!(
            request.starts_with("POST /api/v1/gateway/upgrade-result"),
            "{request}"
        );
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(body.contains("\"status\":\"done\""), "{body}");
        assert!(body.contains("\"gateway_id\":\"gw-1\""), "{body}");
        let record = state::read_upgrade_record(&state_dir).expect("record");
        assert_eq!(record.status, "done");
        assert_eq!(record.step, "verify");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回执送不出去（无身份 / 网络失败）→ 保持 `restarting`，下一轮再试；不得提前落 `done`。
    #[tokio::test]
    async fn settle_pending_self_update_keeps_restarting_when_the_report_fails() {
        let dir = temp_dir("settle-retry");
        let state_dir = dir.join("state");
        let identity_dir = dir.join("identity");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::create_dir_all(&identity_dir).expect("identity dir");
        state::write_upgrade_record(
            &state_dir,
            &UpgradeRecord {
                work_id: "plan-2".into(),
                from_version: "0.11.0".into(),
                to_version: "0.12.0".into(),
                step: "restart".into(),
                status: "restarting".into(),
                detail: String::new(),
                log_dir: None,
            },
        )
        .expect("record");

        settle_pending_self_update(
            &state_dir,
            &identity_dir,
            CenterClient::new("http://127.0.0.1:1"),
            "0.12.0",
        )
        .await;

        let record = state::read_upgrade_record(&state_dir).expect("record");
        assert_eq!(record.status, "restarting");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 重启后版本不符 → 回执 `failed` 并落终态（不把“启动起来了”当成“升对了”）。
    #[tokio::test]
    async fn settle_pending_self_update_marks_failed_on_a_version_mismatch() {
        let dir = temp_dir("settle-mismatch");
        let state_dir = dir.join("state");
        let identity_dir = dir.join("identity");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::create_dir_all(&identity_dir).expect("identity dir");
        write_credential(&identity_dir, "gw-1");
        state::write_upgrade_record(
            &state_dir,
            &UpgradeRecord {
                work_id: "plan-3".into(),
                from_version: "0.11.0".into(),
                to_version: "0.12.0".into(),
                step: "restart".into(),
                status: "restarting".into(),
                detail: String::new(),
                log_dir: None,
            },
        )
        .expect("record");

        let (endpoint, captured) = capture_one_request().await;
        settle_pending_self_update(
            &state_dir,
            &identity_dir,
            CenterClient::new(endpoint),
            "0.11.0",
        )
        .await;

        let request = captured.await.expect("captured request");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        assert!(body.contains("\"status\":\"failed\""), "{body}");
        let record = state::read_upgrade_record(&state_dir).expect("record");
        assert_eq!(record.status, "failed");
        assert!(record.detail.contains("版本不符"), "{}", record.detail);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 接收一次 HTTP 请求并把原文回传（用于断言回执的 path / body）。
    async fn capture_one_request() -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // 小请求一次就读齐；随后等一个短超时确认没有更多字节（客户端在等响应）。
            loop {
                match tokio::time::timeout(Duration::from_millis(200), sock.read(&mut chunk)).await
                {
                    Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            let body = "{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
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
        let healthy = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"last_error":null}"#;
        let client = SelfReportClient::new(serve_self_state(healthy).await);
        let evidence = corroborate_recovery(&client, "gw-1", Duration::from_secs(5))
            .await
            .expect("healthy gateway corroborates");
        assert!(evidence.contains("佐证"), "{evidence}");

        let degraded = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":false,"agent_count":0,"last_error":null}"#;
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
            .start("w-c", "0.1.0", "0.1.16", None, None, None)
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert_eq!(record.step, "custom");
        // P0：回执说明会追加「日志: <任务目录>」。
        assert!(
            record.detail.starts_with("by custom executor"),
            "{}",
            record.detail
        );
        assert!(
            record.detail.contains("日志: ") && record.detail.contains("/tasks/"),
            "回执要带任务日志目录：{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 目录里标了 `tool-copy` 的组件走**进程内安装**（解包覆盖），**不经 gops 执行器**，也不佐证。
    #[tokio::test]
    async fn a_tool_copy_component_is_installed_in_process_not_via_gops() {
        use std::collections::BTreeMap;

        let dir = temp_dir("tool-route");
        // 已安装的工具（原位置，可执行）与待装的制品。
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "gops", "OLD");
        let artifact = dir.join("galaxy-ops.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW"),
        )
        .expect("artifact");

        // 若路由错走 gops，这个脚本会落下 args.txt —— 用它证明「没走 gops」。
        let gops = GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string());
        let tools = ToolInstaller::new(
            BTreeMap::from([(
                "galaxy-ops".to_string(),
                target.to_string_lossy().to_string(),
            )]),
            &dir,
            reqwest::Client::new(),
        )
        .with_host_target(HostTarget::new("aarch64", "macos", true));
        let driver = UpgradeDriver::new(gops, &dir).with_tools(tools);
        driver
            .start(
                "w-tool",
                "0.0.1",
                "0.1.0",
                Some("galaxy-ops"),
                None,
                Some(artifact.to_string_lossy().as_ref()),
            )
            .await
            .expect("start");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert_eq!(record.step, "install");
        assert!(
            record.detail.contains("已就地覆盖 gops"),
            "{}",
            record.detail
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        assert!(!dir.join("args.txt").exists(), "不该走 gops 执行器");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 标了 `tool-copy` 但制品架构与本机不符 → 落**可读失败**，旧二进制原封不动（不经 gops）。
    #[tokio::test]
    async fn a_tool_copy_component_with_a_mismatched_arch_fails_without_touching_the_binary() {
        use std::collections::BTreeMap;

        let dir = temp_dir("tool-arch-mismatch");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "gx", "OLD");
        // macOS arm64 宿主上的 x86_64 Linux 制品 —— 覆盖上去会让工具静默报废。
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-musl/gx", b"NEW"),
        )
        .expect("artifact");

        let gops = GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string());
        let tools = ToolInstaller::new(
            BTreeMap::from([(
                "galaxy-flow".to_string(),
                target.to_string_lossy().to_string(),
            )]),
            &dir,
            reqwest::Client::new(),
        )
        .with_host_target(HostTarget::new("aarch64", "macos", true));
        let driver = UpgradeDriver::new(gops, &dir).with_tools(tools);
        driver
            .start(
                "w-tool-arch",
                "0.0.1",
                "0.15.1",
                Some("galaxy-flow"),
                None,
                Some(artifact.to_string_lossy().as_ref()),
            )
            .await
            .expect("start");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(record.detail.contains("架构校验失败"), "{}", record.detail);
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"OLD",
            "错架构制品绝不得覆盖旧二进制"
        );
        assert!(!dir.join("args.txt").exists(), "不该走 gops 执行器");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 放宽架构要求（`require_verified_arch=false`）：读不出架构的制品能装上，且成功说明标注「不可校验」。
    #[tokio::test]
    async fn a_relaxed_tool_copy_installs_an_unverifiable_artifact() {
        use std::collections::BTreeMap;

        let dir = temp_dir("tool-arch-relaxed");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = write_exec(&bin_dir, "gops", "OLD");
        // 包内与文件名都不带三元组（也不是内容寻址名）。
        let artifact = dir.join("pkg-unnamed");
        std::fs::write(&artifact, tool_archive("pkg/gops", b"NEW")).expect("artifact");

        let gops = GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string());
        let tools = ToolInstaller::new(
            BTreeMap::from([(
                "galaxy-ops".to_string(),
                target.to_string_lossy().to_string(),
            )]),
            &dir,
            reqwest::Client::new(),
        )
        .with_host_target(HostTarget::new("aarch64", "macos", true))
        .with_require_verified_arch(false);
        let driver = UpgradeDriver::new(gops, &dir).with_tools(tools);
        driver
            .start(
                "w-tool-relaxed",
                "0.0.1",
                "0.1.0",
                Some("galaxy-ops"),
                None,
                Some(artifact.to_string_lossy().as_ref()),
            )
            .await
            .expect("start");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        assert!(record.detail.contains("架构不可校验"), "{}", record.detail);
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        assert!(!dir.join("args.txt").exists(), "不该走 gops 执行器");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 标了 `tool-copy` 但 `binary` 在 `PATH` 上找不到 → **发之前**就落可读失败（不静默回落 gops）。
    #[tokio::test]
    async fn a_tool_copy_component_without_a_located_binary_fails_before_install() {
        use std::collections::BTreeMap;

        let dir = temp_dir("tool-preflight");
        let gops = GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string());
        let tools = ToolInstaller::new(
            BTreeMap::from([("galaxy-flow".to_string(), "/nonexistent/gx".to_string())]),
            &dir,
            reqwest::Client::new(),
        );
        let driver = UpgradeDriver::new(gops, &dir).with_tools(tools);
        let err = driver
            .start("w-tool", "0.0.1", "0.1.0", Some("galaxy-flow"), None, None)
            .await
            .expect_err("preflight must fail");
        assert!(err.contains("PATH"), "{err}");
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "failed");
        assert_eq!(record.step, "preflight");
        assert!(!dir.join("args.txt").exists(), "不该发执行器");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 装了工具安装器，但**组件不在目录里** → 仍走 gops 执行器（工具目录不是一刀切）。
    #[tokio::test]
    async fn a_component_outside_the_tool_catalog_still_goes_to_gops() {
        use std::collections::BTreeMap;

        let dir = temp_dir("tool-route-fallback");
        let tools = ToolInstaller::new(
            BTreeMap::from([("galaxy-ops".to_string(), "/bin/true".to_string())]),
            &dir,
            reqwest::Client::new(),
        );
        let driver = UpgradeDriver::new(
            GopsExecutor::new(arg_recording_script(&dir, 0).to_string_lossy().to_string()),
            &dir,
        )
        .with_tools(tools);
        driver
            .start(
                "w-g",
                "0.1.0",
                "0.1.16",
                Some("wist-gateway-stack"),
                None,
                None,
            )
            .await
            .expect("start");

        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        // 走了 gops：记录脚本落下 args.txt，且没有工具安装的痕迹。
        assert!(dir.join("args.txt").exists(), "应走 gops 执行器");
        assert!(!record.detail.contains("就地覆盖"), "{}", record.detail);
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
            .start("w-d", "0.1.0", "0.1.16", None, None, None)
            .await
            .expect("start");
        let _ = wait_terminal(&dir).await;
        let args = std::fs::read_to_string(dir.join("args.txt")).expect("args");
        assert!(args.contains("--on-failure rollback-all"), "{args}");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── 驱动级「成功佐证」映射 ──

    const SELF_STATE_HEALTHY: &str = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"last_error":null}"#;
    const SELF_STATE_DEGRADED: &str = r#"{"gateway_id":"gw-1","version":"0.1.18-alpha","collected_at":"2026-10-04T00:00:00Z","store_healthy":false,"agent_count":0,"last_error":null}"#;

    /// 佐证分支要 `gateway_id`（从长期身份读）——所以得先存在一份凭据。
    fn write_credential(dir: &Path, gateway_id: &str) {
        let credential = state::StoredCredential {
            bundle: wist_control::GatewayCredentialBundle {
                credential_id: "cred-test".into(),
                gateway_id: gateway_id.into(),
                instance_id: None,
                certificate: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .into(),
                ca_bundle: None,
                issued_at: wist_control::DateTime::from_rfc3339("2026-10-04T00:00:00Z")
                    .expect("rfc3339"),
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
            identity_dir: dir.to_path_buf(),
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
            .start(
                "w-ok",
                "0",
                "1",
                None,
                Some(reporter(&dir, Some(endpoint))),
                None,
            )
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
                None,
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
            .start("w-none", "0", "1", None, Some(reporter(&dir, None)), None)
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
                None,
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
                None,
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
        let err = driver
            .start("w-s", "0.1.0", "0.1.16", None, None, None)
            .await;
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
            .start("w-chatty", "0.1.0", "0.1.16", None, None, None)
            .await
            .expect("start");
        // 旧实现会卡在 wait()（子进程阻塞在写）→ 记录一直 running → 这里超时 panic。
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "done");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_gops_executor_without_a_project_root_fails_fast_without_spawning() {
        // 缺 `upgrade_project_dir`：别把 gops 发出去（它只会以退出码 255 失败、还读不出原因）。
        let dir = temp_dir("preflight-none");
        let driver = UpgradeDriver::new(GopsExecutor::new("gops"), &dir);
        let err = driver
            .start("w-pf", "0.1.0", "0.1.16", None, None, None)
            .await;
        assert!(err.is_err(), "缺工程根应前置失败");
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.status, "failed");
        assert_eq!(record.step, "preflight");
        assert!(
            record.detail.contains("upgrade_project_dir"),
            "回执应说明缺配什么：{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_gops_project_dir_without_ops_prj_yml_fails_fast() {
        let dir = temp_dir("preflight-marker");
        // 显式给了工程根，但里面没有 ops-prj.yml。
        let driver = UpgradeDriver::new(
            GopsExecutor::new("gops").with_project(Some(dir.clone()), None),
            &dir,
        );
        let err = driver
            .start("w-pf2", "0.1.0", "0.1.16", None, None, None)
            .await;
        assert!(err.is_err());
        let record = state::read_upgrade_record(&dir).expect("record");
        assert_eq!(record.step, "preflight");
        assert!(
            record.detail.contains("ops-prj.yml"),
            "回执应点名缺 ops-prj.yml：{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_gops_program_with_a_project_root_proceeds_to_execution() {
        // 前置校验过了才真发：脚本名就叫 `gops`（basename 判据），工程根带 ops-prj.yml。
        let dir = temp_dir("preflight-ok");
        let script = write_exec(&dir, "gops", "#!/bin/sh\nexit 0\n");
        let driver = UpgradeDriver::new(
            gops_script(&dir, script.to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start("w-ok", "0.1.0", "0.1.16", None, None, None)
            .await
            .expect("start");
        assert_eq!(wait_terminal(&dir).await.status, "done");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failing_executor_stderr_reaches_the_record_detail() {
        // 只留 `executor exit status: 255` 等于让人去翻日志；关键错误行要进回执。
        let dir = temp_dir("stderr-detail");
        let body = "#!/bin/sh\necho 'Run Error (Code: 203)' >&2\necho '当前目录没有 ops-prj.yml' >&2\nexit 255\n";
        let script = write_exec(&dir, "noisy-fail.sh", body);
        let driver = UpgradeDriver::new(
            gops_script(&dir, script.to_string_lossy().to_string()),
            &dir,
        );
        driver
            .start("w-detail", "0.1.0", "0.1.16", None, None, None)
            .await
            .expect("start");
        let record = wait_terminal(&dir).await;
        assert_eq!(record.status, "failed");
        assert!(
            record.detail.contains("ops-prj.yml"),
            "stderr 关键行应进 detail：{}",
            record.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
