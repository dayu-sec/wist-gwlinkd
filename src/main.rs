//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `service` / `version`。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::center::{self, CenterClient, CenterError};
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::executor::{DEFAULT_ON_FAILURE, DEFAULT_UPGRADER_PROGRAM, GopsExecutor};
use wist_gwlinkd::identity;
use wist_gwlinkd::link_request::LinkRequestClient;
use wist_gwlinkd::linkd_status::{
    self, GwlinkdStatus, STATE_DEGRADED, STATE_LINKED, STATE_WAITING_LINK_REQUEST,
};
use wist_gwlinkd::selfreport::SelfReportClient;
use wist_gwlinkd::service;
use wist_gwlinkd::state::{self, CredentialStatus, UpgradeCursor};
use wist_gwlinkd::upgrade::{RECOVERY_VERIFY_TIMEOUT, UpgradeDriver, UpgradeReporter};

/// 运行期状态上报周期（秒）。
const STATUS_INTERVAL_SECS: u64 = 30;
/// 退避初始值（秒）：renew / status 被拒后指数退避，避免猛击中心。
const BACKOFF_BASE_SECS: u64 = 60;
/// 退避上限（秒）。
const BACKOFF_MAX_SECS: u64 = 1800;

fn config_path() -> PathBuf {
    std::env::var("WIST_GWLINKD_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(wist_gwlinkd::DEFAULT_CONFIG_PATH))
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_else(|| "run".to_string());
    match command.as_str() {
        "version" | "-V" | "--version" => {
            println!("wist-gwlinkd {}", wist_gwlinkd::VERSION);
            ExitCode::SUCCESS
        }
        "diagnose" => match Config::load(&config_path()) {
            Ok(config) => run_diagnose(&config),
            Err(err) => {
                eprintln!("[FAIL] 配置不可读：{err}");
                ExitCode::FAILURE
            }
        },
        "service" => run_service(&std::env::args().skip(2).collect::<Vec<_>>()),
        "run" => match Config::load(&config_path()) {
            Ok(config) => match run(&config).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("[FAIL] {err}");
                    ExitCode::FAILURE
                }
            },
            Err(err) => {
                eprintln!("[FAIL] 配置不可读：{err}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unknown command: {other}（可用：run | diagnose | service | version）");
            ExitCode::from(2)
        }
    }
}

/// 常驻：单实例 → 首跑置备（若未注册）→ 周期【续期 / 拉升级目标 / 拉自述面 / 上报状态】。
async fn run(config: &Config) -> Result<(), String> {
    // 单实例：同机只允许一个常驻（否则双重上报 + 双重驱动升级）。持有到进程退出。
    let _lock = state::acquire_single_instance_lock(&config.state_dir)?;

    let identity = state::load_or_create_identity(&config.state_dir)?;
    // 信任锚：配置给了且文件在，就作为自定义根（自签中心必需）；否则回落公共根（并记一笔）。
    let trust_bundle = config.trust_bundle.as_path();
    let trust: Option<&Path> = if trust_bundle.exists() {
        Some(trust_bundle)
    } else {
        eprintln!(
            "event=TrustBundleMissing path={}（回落公共根）",
            trust_bundle.display()
        );
        None
    };

    match state::credential_status(&config.state_dir) {
        CredentialStatus::Present(_) => {
            // 已有客户端证书：遗留的 RegistToken 一定是旧的，清掉（避免误用）。
            state::clear_regist_token(&config.state_dir);
        }
        CredentialStatus::Missing => {
            // 首跑还没客户端证书：先看网关页有没有提交「接入请求」（环回），否则回退 env 券。
            first_run(config, trust, &identity).await?;
        }
        CredentialStatus::Corrupt(detail) => {
            // 损坏 ≠ 缺失：不能静默重置备（会覆盖掉唯一一份长期身份）。
            return Err(format!(
                "长期身份损坏：{detail}；修复或删除 state/credential.json 后重跑（若中心已初始化该实例，需先在中心重置）"
            ));
        }
    }
    let mut credential = state::load_credential(&config.state_dir)
        .ok_or_else(|| "注册后仍无长期身份".to_string())?;
    // 注册后所有网关面调用都走 **mTLS**（客户端证书认人）；续期后重建。
    let mut client = mtls_client(config, trust, &credential)?;
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;

    let self_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(SelfReportClient::with_trust(
            base,
            config.gateway_self_ca.as_deref(),
        )?),
        None => None,
    };
    // gwlinkd 心跳：推自身状态给网关（页面拉不到 gwlinkd —— 它纯出站）。同环回面 + 同一信任锚。
    let linkd_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(LinkRequestClient::with_trust(
            base,
            config.gateway_self_ca.as_deref(),
        )?),
        None => None,
    };
    let renew_lead = config.renew_lead_seconds.unwrap_or(3600);
    let driver = UpgradeDriver::new(
        GopsExecutor::new(
            config
                .upgrader_program
                .clone()
                .unwrap_or_else(|| DEFAULT_UPGRADER_PROGRAM.to_string()),
        )
        .with_on_failure(
            config
                .upgrade_on_failure
                .clone()
                .unwrap_or_else(|| DEFAULT_ON_FAILURE.to_string()),
        )
        .with_project(
            config.upgrade_project_dir.clone(),
            config.upgrade_project_name.clone(),
        )
        .with_health_check(
            config.upgrade_health_cmd.clone(),
            config.upgrade_health_timeout_seconds,
        ),
        config.state_dir.clone(),
    )
    .with_verify_timeout(Duration::from_secs(
        config
            .upgrade_verify_timeout_seconds
            .unwrap_or(RECOVERY_VERIFY_TIMEOUT.as_secs()),
    ));

    let mut ticker = tokio::time::interval(Duration::from_secs(STATUS_INTERVAL_SECS));
    // 一轮里有多次（30s 超时）的网络调用；慢轮后别突发补 tick。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renew_backoff = Duration::ZERO;
    let mut next_renew_at = Instant::now();
    let mut status_backoff = Duration::ZERO;
    let mut next_status_at = Instant::now();
    // 轮换收尾的三态对齐标记（内存 credential / 磁盘 / mTLS 客户端）：失败则下一轮自愈重试。
    let mut credential_dirty = false;
    let mut client_stale = false;
    // gwlinkd 自身状态（心跳）的派生态：默认已接入；有失败转 `Degraded` 并记原因（成功即清）。
    let mut linkd_state = STATE_LINKED.to_string();
    let mut linkd_last_error: Option<String> = None;
    let mut linkd_last_report_at: Option<String> = None;

    // 启动收尾：上次运行留下的「被判死」升级（升级器被中断，机器可能停在中间态）。
    // 放在循环外只做一次（这是「上一次运行」的遗留态，不是运行中会反复出现的东西）。
    if state::upgrader_is_declared_dead(&config.state_dir, SystemTime::now())
        && state::load_upgrade_cursor(&config.state_dir)
            .last_plan_id
            .is_some()
    {
        if config.upgrade_retry_on_dead.unwrap_or(true) {
            // 清游标 → 同一计划可被下面的循环重新驱动。并发安全：gops 自带工程交付锁串行化两次调用。
            eprintln!("event=DeadUpgradeDetected 清游标以便重驱同一计划");
            if let Err(err) =
                state::save_upgrade_cursor(&config.state_dir, &UpgradeCursor::default())
            {
                eprintln!("event=CursorClearFailed error={err}");
            }
        } else {
            eprintln!(
                "event=DeadUpgradeDetected 未自动重试（upgrade_retry_on_dead=false）：请到管理面重派升级"
            );
        }
    }

    loop {
        ticker.tick().await;

        // 上一轮轮换若有收尾未完成（内存/磁盘/客户端未对齐），先自愈重试。
        if credential_dirty && let Err(err) = state::save_credential(&config.state_dir, &credential)
        {
            eprintln!("event=CredentialSaveRetryFailed error={err}");
        } else if credential_dirty {
            credential_dirty = false;
            println!("event=CredentialSaved gateway_id={}", config.gateway_id);
        }
        if client_stale {
            match mtls_client(config, trust, &credential) {
                Ok(rebuilt) => {
                    client = rebuilt;
                    client_stale = false;
                    println!("event=MtlsClientRebuilt gateway_id={}", config.gateway_id);
                }
                Err(err) => eprintln!("event=MtlsClientRebuildRetryFailed error={err}"),
            }
        }

        // 到期前轮换：当场再生成一套密钥对 → 以当前证书证明身份 + 新 CSR → 换新证书（旧证书作废）。
        if credential.seconds_remaining() <= renew_lead && Instant::now() >= next_renew_at {
            let keypair = match identity::generate_client_keypair(&config.gateway_id) {
                Ok(keypair) => keypair,
                Err(err) => {
                    renew_backoff = back_off(renew_backoff);
                    next_renew_at = Instant::now() + renew_backoff;
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("续期密钥生成失败：{err}"));
                    eprintln!("event=RenewFailed backoff={renew_backoff:?} error={err}");
                    continue;
                }
            };
            let current_serial = credential.certificate_serial_hex().unwrap_or_default();
            match client
                .renew_credential(&config.gateway_id, &current_serial, &keypair.csr_pem)
                .await
            {
                Ok(renewed) => {
                    let new_credential = state::StoredCredential {
                        bundle: renewed,
                        private_key_pem: keypair.private_key_pem,
                    };
                    // 先换内存态 + mTLS 客户端（旧证书已在中心侧作废），再落盘；
                    // 落盘/重建失败均置脏标记，下一轮循环自愈重试（不拖死常驻）。
                    credential = new_credential;
                    match mtls_client(config, trust, &credential) {
                        Ok(rebuilt) => {
                            client = rebuilt;
                            client_stale = false;
                        }
                        Err(err) => {
                            client_stale = true;
                            eprintln!("event=MtlsClientRebuildFailed error={err}");
                        }
                    }
                    match state::save_credential(&config.state_dir, &credential) {
                        Ok(()) => credential_dirty = false,
                        Err(err) => {
                            credential_dirty = true;
                            eprintln!("event=CredentialSaveFailed error={err}");
                        }
                    }
                    renew_backoff = Duration::ZERO;
                    next_renew_at = Instant::now();
                    println!("event=CredentialRenewed gateway_id={}", config.gateway_id);
                }
                Err(err) => {
                    renew_backoff = back_off(renew_backoff);
                    next_renew_at = Instant::now() + renew_backoff;
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("凭据续期失败：{err}"));
                    eprintln!("event=RenewFailed backoff={renew_backoff:?} error={err}");
                }
            }
        }

        // 拉升级目标（CR-002 C2）：有在飞升级则**互斥跳过**；否则「未驱过的计划」才驱动。
        if !state::upgrade_in_flight(&config.state_dir, SystemTime::now()) {
            match client.get_upgrade_plan(&config.gateway_id).await {
                Ok(plan) if plan.has_plan => {
                    let cursor = state::load_upgrade_cursor(&config.state_dir);
                    let already = plan.plan_id.is_some() && plan.plan_id == cursor.last_plan_id;
                    let to_version = plan.to_version.clone().unwrap_or_default();
                    if !already && !to_version.is_empty() {
                        // 从版本取游标记的「上次目标」；不知道就 unknown（**不再拿 gwlinkd 自身版本硬比** —— 版本空间不同）。
                        let from_version = if cursor.last_to_version.is_empty() {
                            "unknown".to_string()
                        } else {
                            cursor.last_to_version.clone()
                        };
                        println!(
                            "event=UpgradeDriven plan_id={:?} to_version={to_version} component={:?}",
                            plan.plan_id, plan.component
                        );
                        let reporter = UpgradeReporter {
                            client: client.clone(),
                            state_dir: config.state_dir.clone(),
                            self_client: self_client.clone(),
                        };
                        // **不**用 `?`：驱动失败（执行器缺失/架构不符…）绝不能把链路常驻整个拖死。
                        match driver
                            .start(
                                plan.plan_id.as_deref().unwrap_or("plan"),
                                &from_version,
                                &to_version,
                                plan.component.as_deref(),
                                Some(reporter),
                            )
                            .await
                        {
                            Ok(()) => {
                                // 先落游标再继续：跨重启幂等据此判定。
                                if let Err(err) = state::save_upgrade_cursor(
                                    &config.state_dir,
                                    &UpgradeCursor {
                                        last_plan_id: plan.plan_id.clone(),
                                        last_to_version: to_version,
                                    },
                                ) {
                                    eprintln!("event=CursorSaveFailed error={err}");
                                }
                            }
                            Err(err) => eprintln!("event=UpgradeDriveFailed error={err}"),
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => eprintln!("event=UpgradePlanFailed error={err}"),
            }
        }

        // 拉网关自述面：**准确状态 + 网关版本**的来源；不答则把「沉默」当判断。
        let (health, gateway_version) = match &self_client {
            Some(self_client) => match self_client.fetch(&config.gateway_id).await {
                Ok(self_state) => (self_state.health().to_string(), self_state.version.clone()),
                Err(err) => {
                    eprintln!("event=SelfStateFailed error={err}");
                    ("unknown".to_string(), "unknown".to_string())
                }
            },
            // 未配自述面：**无证据**，报 unknown（不假装健康）。
            None => ("unknown".to_string(), "unknown".to_string()),
        };

        if Instant::now() >= next_status_at {
            let payload = ReportGatewayStatus {
                gateway_id: config.gateway_id.clone(),
                instance_id: instance_id.clone(),
                // 报的是**网关（容器）版本**，不是 gwlinkd 自身版本 —— 这条状态描述的是网关。
                version: gateway_version,
                status: "running".to_string(),
                health,
                memory_bytes: None,
                cpu_percent: None,
                reported_at: DateTime::now(),
            };
            match client.report_status(&payload).await {
                Ok(()) => {
                    status_backoff = Duration::ZERO;
                    linkd_state = STATE_LINKED.to_string();
                    linkd_last_error = None;
                    linkd_last_report_at = Some(linkd_status::now_rfc3339());
                    println!("event=StatusReported gateway_id={}", config.gateway_id);
                }
                Err(err) if err.is_unauthorized() => {
                    // 客户端证书被拒：**退避**（不每 30s 猛击），并明确要中心重置该实例。
                    status_backoff = back_off(status_backoff);
                    next_status_at = Instant::now() + status_backoff;
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("中心拒绝了客户端证书：{err}"));
                    eprintln!(
                        "event=CredentialRejected gateway_id={} backoff={status_backoff:?}（客户端证书已失效；需管理员在中心重置该实例后重新置备）error={err}",
                        config.gateway_id
                    );
                }
                Err(err) => {
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("状态上报失败：{err}"));
                    eprintln!("event=StatusReportFailed error={err}");
                }
            }
        }

        // 推 gwlinkd 自身状态（心跳）给网关：页面据此展示「宿主侧常驻在不在跑」。
        if let Some(linkd_client) = &linkd_client {
            let status = GwlinkdStatus {
                gateway_id: config.gateway_id.clone(),
                instance_id: instance_id.clone(),
                version: wist_gwlinkd::VERSION.to_string(),
                center_endpoint: config.control_center_endpoint.clone(),
                state: linkd_state.clone(),
                credential_expires_at: credential.bundle.not_after.clone(),
                last_center_report_at: linkd_last_report_at.clone(),
                last_error: linkd_last_error.clone(),
                reported_at: DateTime::now(),
            };
            if let Err(err) = linkd_client.report_linkd_status(&status).await {
                eprintln!("event=LinkdStatusPushFailed error={err}");
            }
        }
    }
}

/// 指数退避：0 → 初值，否则翻倍到上限。
fn back_off(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_secs(BACKOFF_BASE_SECS)
    } else {
        (current * 2).min(Duration::from_secs(BACKOFF_MAX_SECS))
    }
}

/// 读取一次性接入券：优先 `WIST_GWLINKD_LINK_TOKEN`，回退旧名
/// `WIST_GWLINKD_BOOTSTRAP_TOKEN`（弃用告警，下一版移除）。
fn link_token_from_env() -> Option<String> {
    if let Ok(token) = std::env::var("WIST_GWLINKD_LINK_TOKEN") {
        return Some(token);
    }
    match std::env::var("WIST_GWLINKD_BOOTSTRAP_TOKEN") {
        Ok(token) => {
            eprintln!(
                "warning: WIST_GWLINKD_BOOTSTRAP_TOKEN 已更名为 WIST_GWLINKD_LINK_TOKEN，请更新（旧名下一版移除）"
            );
            Some(token)
        }
        Err(_) => None,
    }
}

/// 首跑：确保拿到长期身份（可能等待页面提交的接入请求）。
async fn first_run(config: &Config, trust: Option<&Path>, identity: &str) -> Result<(), String> {
    // 页面发起（环回接入请求）优先。
    if let Some(base) = config.gateway_self_endpoint.as_deref() {
        let client = LinkRequestClient::with_trust(base, config.gateway_self_ca.as_deref())?;
        if link_token_from_env().is_none() {
            // 没有 env 券 → 页面发起路径：等到页面上提交了再接入，失败则继续等下一次。
            return wait_for_gateway_request(config, trust, identity, &client).await;
        }
        // 有 env 券：优先环回请求，没有/失败则回退旧路径。
        match onboard_from_gateway(config, trust, identity, &client).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => eprintln!("event=LinkRequestFailed error={err}（回退 env 券路径）"),
        }
    }
    // 旧路径：env 券（或报错）+ 配置 endpoint/trust。
    let link_client = CenterClient::with_client(
        config.control_center_endpoint.clone(),
        center::build_http_client(trust)?,
    );
    onboard(
        &link_client,
        config,
        identity,
        link_token_from_env().as_deref(),
    )
    .await
}

/// 一次「从环回接入请求接入」：有待办则接入并回报结果，成功返回 `Ok(true)`；无待办 `Ok(false)`。
async fn onboard_from_gateway(
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
    client: &LinkRequestClient,
) -> Result<bool, String> {
    let request = client.fetch(&config.gateway_id).await?;
    if !request.has_request {
        return Ok(false);
    }
    // 页面带的 CA 落盘（免得还要先手工预置信任锚）。
    let trust_path = if request.trust_bundle_pem.trim().is_empty() {
        None
    } else {
        Some(state::save_trust_bundle(
            &config.state_dir,
            &request.trust_bundle_pem,
        )?)
    };
    let effective_trust = trust_path.as_deref().or(trust);
    let link_client = CenterClient::with_client(
        request.center_endpoint.clone(),
        center::build_http_client(effective_trust)?,
    );
    println!(
        "event=LinkRequestPicked gateway_id={} center={}",
        config.gateway_id, request.center_endpoint
    );
    match onboard(&link_client, config, identity, Some(&request.link_token)).await {
        Ok(()) => {
            let _ = client
                .report_result(&config.gateway_id, "Connected", "")
                .await;
            Ok(true)
        }
        Err(err) => {
            let _ = client
                .report_result(&config.gateway_id, "Failed", &err)
                .await;
            Err(err)
        }
    }
}

/// 等待页面提交接入请求，出现即接入；失败则记结果后继续等下一次。
///
/// 每轮先尝试**遗留 RegistToken** 的免接入券注册（link-upstream 已成功、register 未成、
/// 随后终态而不再派发时靠它自愈）。
async fn wait_for_gateway_request(
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
    client: &LinkRequestClient,
) -> Result<(), String> {
    println!(
        "event=WaitingLinkRequest gateway_id={}（等待页面「链接上级」提交接入请求）",
        config.gateway_id
    );
    loop {
        match retry_leftover_registration(config, trust, identity).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => eprintln!("event=RegisterRetryFailed error={err}"),
        }
        match onboard_from_gateway(config, trust, identity, client).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => eprintln!("event=LinkRequestFailed error={err}"),
        }
        // 心率：接入前也推一条「在跑、等待接入」——免得页面把「还没接」当成「gwlinkd 没跑」。
        let waiting = GwlinkdStatus {
            gateway_id: config.gateway_id.clone(),
            instance_id: state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)
                .unwrap_or_default(),
            version: wist_gwlinkd::VERSION.to_string(),
            center_endpoint: config.control_center_endpoint.clone(),
            state: STATE_WAITING_LINK_REQUEST.to_string(),
            credential_expires_at: None,
            last_center_report_at: None,
            last_error: None,
            reported_at: DateTime::now(),
        };
        if let Err(err) = client.report_linkd_status(&waiting).await {
            eprintln!("event=LinkdStatusPushFailed error={err}");
        }
        tokio::time::sleep(Duration::from_secs(STATUS_INTERVAL_SECS)).await;
    }
}

/// 遗留 RegistToken 的免接入券注册：endpoint 优先取已落盘链接配置，trust 优先取页面带来的 CA。
/// 返回 `Ok(true)` 已注册；`Ok(false)` 无遗留 token；`Err` 注册失败（unauthorized 时 onboard 已清 token）。
async fn retry_leftover_registration(
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
) -> Result<bool, String> {
    if state::load_regist_token(&config.state_dir).is_none() {
        return Ok(false);
    }
    let endpoint = state::load_link_config(&config.state_dir)
        .map(|saved| saved.control_center_endpoint)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.control_center_endpoint.clone());
    let saved_ca = config.state_dir.join(state::TRUST_BUNDLE_FILE);
    let effective_trust: Option<&Path> = if saved_ca.exists() {
        Some(saved_ca.as_path())
    } else {
        trust
    };
    let client = CenterClient::with_client(endpoint, center::build_http_client(effective_trust)?);
    onboard(&client, config, identity, None)
        .await
        .map(|()| true)
}

/// 首跑置备：link-upstream（一次性接入券 + 身份头）→ 落链接配置 → 生成密钥对+CSR →
/// register（中心签出客户端证书）→ 落长期身份（证书 + 私钥）。
///
/// **可重试**：`link-upstream` 成功即把 RegistToken 落盘；若随后 `register` 失败（网络等），
/// 下次直接拿落盘的 token 重试，**不再需要接入券**（已被消费）。
async fn onboard(
    client: &CenterClient,
    config: &Config,
    identity: &str,
    link: Option<&str>,
) -> Result<(), String> {
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;

    // 复用上次未消费的 RegistToken（link-upstream 已成功、register 未成的遗留）：直接重试注册。
    if let Some(regist_token) = state::load_regist_token(&config.state_dir) {
        println!("event=RegisterRetry gateway_id={}", config.gateway_id);
        match register_once(client, config, &regist_token, &instance_id).await {
            Ok(()) => {
                state::clear_regist_token(&config.state_dir);
                return Ok(());
            }
            Err(err) if err.is_unauthorized() => {
                // token 已失效/已被消费：丢弃，走完整首跑（需接入券）。
                eprintln!("event=RegistTokenStale 清掉遗留 token，重走首跑");
                state::clear_regist_token(&config.state_dir);
            }
            // 网络类错误：保留 token，下次再试。
            Err(err) => return Err(err.to_string()),
        }
    }

    let link = link.ok_or_else(|| {
        "首跑需要 WIST_GWLINKD_LINK_TOKEN（中心 admin 创建实例时签发的接入 token）；\
         若本机曾有身份，请检查 state/credential.json 是否损坏"
            .to_string()
    })?;

    println!("event=LinkUpstream gateway_id={}", config.gateway_id);
    let returned = client
        .link_upstream(&config.gateway_id, link, Some(identity))
        .await
        .map_err(|err| err.to_string())?;
    // 链接配置（信任锚 / 协议版本 / 注册 token 引用）落盘留痕 —— 不再丢弃。
    state::save_link_config(&config.state_dir, &returned.config)?;
    let regist_token = returned.regist_token.ok_or_else(|| {
        "中心认为该网关**已初始化**，但本地无客户端证书 —— 无法自动恢复（CR-003 尚缺「身份重置」路径）。\
         请在中心重置该实例后重跑，或把既有的客户端证书/私钥写入 state/credential.json"
            .to_string()
    })?;
    // **先落盘再注册**：接入券已消费，注册失败也要能靠这个 token 重试。
    state::save_regist_token(&config.state_dir, &regist_token)?;

    register_once(client, config, &regist_token, &instance_id)
        .await
        .map_err(|err| err.to_string())?;
    state::clear_regist_token(&config.state_dir);
    Ok(())
}

/// 用 RegistToken 完成一次注册：当场生成密钥对（私钥不上送，只交 CSR）、落长期身份。
async fn register_once(
    client: &CenterClient,
    config: &Config,
    regist_token: &str,
    instance_id: &str,
) -> Result<(), CenterError> {
    let keypair =
        identity::generate_client_keypair(&config.gateway_id).map_err(CenterError::Other)?;
    println!("event=Register gateway_id={}", config.gateway_id);
    let result = client
        .register(regist_token, instance_id, &keypair.csr_pem)
        .await?;
    let credential = state::StoredCredential {
        bundle: result.credential_bundle,
        private_key_pem: keypair.private_key_pem,
    };
    state::save_credential(&config.state_dir, &credential).map_err(CenterError::Other)?;
    println!(
        "event=Registered gateway_id={} credential_id={}",
        result.gateway_id, result.credential_id
    );
    Ok(())
}

/// 以当前长期身份建 mTLS 客户端（网关面调用用它认人）。
fn mtls_client(
    config: &Config,
    trust: Option<&Path>,
    credential: &state::StoredCredential,
) -> Result<CenterClient, String> {
    let http = center::build_mtls_http_client(trust, &credential.identity_pem())?;
    Ok(CenterClient::with_client(
        config.control_center_endpoint.clone(),
        http,
    ))
}

fn run_diagnose(config: &Config) -> ExitCode {
    let report = doctor::diagnose(config);
    for check in &report.checks {
        let tag = match check.status {
            Status::Ok => "[OK]",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        };
        println!("{tag} {}\n       {}", check.title, check.detail);
        if let Some(hint) = &check.hint {
            println!("       → {hint}");
        }
    }
    match report.worst() {
        Status::Fail => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

// ─────────────────────────── service（OS 服务管理器托管） ───────────────────────

fn run_service(args: &[String]) -> ExitCode {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!(
            "用法：wist-gwlinkd service <print|install|uninstall|status> [--system|--user] [--bin PATH] [--config PATH] [--force] [--no-activate]"
        );
        return ExitCode::from(2);
    };
    match service_command(action, &args[1..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[FAIL] {err}");
            ExitCode::FAILURE
        }
    }
}

fn service_command(action: &str, rest: &[String]) -> Result<(), String> {
    let platform = service::ServicePlatform::current()
        .ok_or_else(|| "当前平台非 Linux/macOS，不支持 service 托管".to_string())?;

    let mut scope: Option<service::ServiceScope> = None;
    let mut bin: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut force = false;
    let mut activate = true;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--system" => {
                scope = Some(service::ServiceScope::System);
                i += 1;
            }
            "--user" => {
                scope = Some(service::ServiceScope::User);
                i += 1;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            "--no-activate" => {
                activate = false;
                i += 1;
            }
            "--bin" => {
                bin = Some(PathBuf::from(need_arg(rest, i + 1, "--bin")?));
                i += 2;
            }
            "--config" => {
                config = Some(PathBuf::from(need_arg(rest, i + 1, "--config")?));
                i += 2;
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }

    // 默认 system（正式运行即系统级常驻）；要用户级就显式 --user。
    let scope = scope.unwrap_or(service::ServiceScope::System);
    let bin = match bin {
        Some(bin) => bin,
        None => service::default_bin()?,
    };
    let config = match config {
        Some(config) => config,
        None => service::default_config_path(scope)?,
    };
    let layout = service::ServiceLayout::resolve(platform, scope)?;
    let spec = service::ServiceSpec::new(scope, bin, config);

    match action {
        "print" => {
            print!("{}", service::render(&layout, &spec));
            Ok(())
        }
        "install" => {
            let report = service::install(&layout, &spec, force)?;
            println!(
                "service 定义已写：{}（{}{}）",
                report.definition_path.display(),
                report.platform.as_str(),
                if report.overwritten { "，覆盖" } else { "" }
            );
            println!("bin={}", service::path_state(&spec.bin));
            if activate {
                run_service_commands(service::activate_commands(&layout, &spec))?;
                println!("已启用并启动。");
            } else {
                println!("（--no-activate：只写定义，未启用）");
            }
            println!("logs: {}", service::log_hint(&layout)?);
            for command in service::inspect_commands(platform, &spec) {
                println!("check: {}", command.display_line());
            }
            Ok(())
        }
        "uninstall" => {
            run_service_commands(service::deactivate_commands(platform, &spec))?;
            let removed = service::remove(&layout)?;
            println!(
                "service 定义 {}：{}",
                layout.definition_path.display(),
                if removed { "已删除" } else { "本就不存在" }
            );
            Ok(())
        }
        "status" => {
            let status = service::status(&layout, &spec)?;
            println!("platform={}", status.platform.as_str());
            println!("scope={}", status.scope.as_str());
            println!("definition={}", service::path_state(&status.definition_path));
            println!("bin={}", service::path_state(&status.bin));
            println!("config={}", service::path_state(&status.config_path));
            if let Some(err) = &status.config_error {
                println!("config_error={err}");
            }
            if let Some(dir) = &status.state_dir {
                println!("state_dir={}", dir.display());
            }
            println!(
                "running={}",
                match status.running {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            );
            println!("logs={}", status.log_hint);
            for command in service::inspect_commands(platform, &spec) {
                println!("check={}", command.display_line());
            }
            Ok(())
        }
        other => Err(format!(
            "未知 service 动作：{other}（可用：print | install | uninstall | status）"
        )),
    }
}

fn need_arg(args: &[String], index: usize, flag: &str) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| format!("{flag} 需要一个值"))
}

fn run_service_commands(commands: Vec<service::ServiceCommand>) -> Result<(), String> {
    let mut failures = Vec::new();
    for command in commands {
        // 服务管理器拆除是异步的（尤其 launchd），瞬时失败靠有界重试吃掉。
        let outcome = service::run_with_retries(&command)?;
        if outcome.success || command.ignore_failure {
            continue;
        }
        let detail = if outcome.stderr.is_empty() {
            outcome.stdout.clone()
        } else {
            outcome.stderr.clone()
        };
        failures.push(format!("`{}` 失败: {detail}", command.display_line()));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_off_starts_then_doubles_to_the_cap() {
        let first = back_off(Duration::ZERO);
        assert_eq!(first, Duration::from_secs(BACKOFF_BASE_SECS));
        assert_eq!(back_off(first), Duration::from_secs(BACKOFF_BASE_SECS * 2));
        assert_eq!(
            back_off(Duration::from_secs(BACKOFF_MAX_SECS)),
            Duration::from_secs(BACKOFF_MAX_SECS)
        );
    }

    // ── onboard 语义测试（中心用桩）──

    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Stub {
        /// 记录的 `"METHOD path"`（去 query）。
        calls: Vec<String>,
        /// link-upstream 的固定响应 `(status, body)`。
        link: Option<(u16, String)>,
        /// 网关环回 `link-request` 的固定响应 `(status, body)`。
        link_request: Option<(u16, String)>,
        /// register 依次响应（多余调用重复最后一个）。
        registers: Vec<(u16, String)>,
        reg_idx: usize,
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn test_config(dir: &std::path::Path, endpoint: String) -> Config {
        Config {
            control_center_endpoint: endpoint,
            trust_bundle: dir.join("ca.pem"),
            state_dir: dir.to_path_buf(),
            gateway_id: "gw-1".into(),
            gateway_self_endpoint: None,
            gateway_self_ca: None,
            renew_lead_seconds: None,
            upgrader_program: None,
            upgrade_on_failure: None,
            upgrade_health_cmd: None,
            upgrade_health_timeout_seconds: None,
            upgrade_verify_timeout_seconds: None,
            upgrade_project_dir: None,
            upgrade_project_name: None,
            upgrade_retry_on_dead: None,
        }
    }

    fn link_body(regist_token: Option<&str>) -> String {
        serde_json::json!({
            "config": {
                "gateway_id": "gw-1",
                "control_center_endpoint": "https://center",
                "trust_bundle": serde_json::Value::Null,
                "server_tls_required": false,
                "protocol_version": "1.0",
                "enrollment_token_id": "enroll-1",
            },
            "regist_token": regist_token,
        })
        .to_string()
    }

    fn register_result_body() -> String {
        serde_json::json!({
            "status": "accepted",
            "gateway_id": "gw-1",
            "instance_id": "inst-1",
            "credential_id": "cred-1",
            "initial_config": "v1",
            "credential_bundle": {
                "credential_id": "cred-1",
                "gateway_id": "gw-1",
                "instance_id": "inst-1",
                "certificate": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
                "ca_bundle": serde_json::Value::Null,
                "issued_at": "2026-10-05T00:00:00Z",
                "not_before": serde_json::Value::Null,
                "not_after": serde_json::Value::Null,
            }
        })
        .to_string()
    }

    /// 极简中心桩：按路径回固定响应，并记录调用序列。
    async fn serve_center(stub: Arc<Mutex<Stub>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let stub = Arc::clone(&stub);
                tokio::spawn(async move {
                    // 排空请求（头 + body）：小载荷一次就能读完，空闲即止。
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match tokio::time::timeout(
                            Duration::from_millis(200),
                            sock.read(&mut chunk),
                        )
                        .await
                        {
                            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let request_line = head.lines().next().unwrap_or_default();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let route = path.split('?').next().unwrap_or_default().to_string();
                    let (status, body) = {
                        let mut guard = stub.lock().unwrap();
                        guard.calls.push(format!("{method} {route}"));
                        if route == "/api/v1/gateway/link-upstream" {
                            guard.link.clone().unwrap_or((404, "no link".into()))
                        } else if route == "/api/v1/gateway/link-request" {
                            guard
                                .link_request
                                .clone()
                                .unwrap_or((404, "no request".into()))
                        } else if route == "/api/v1/gateway/link-result" {
                            (200, "{}".into())
                        } else if route == "/api/v1/gateway/register" {
                            let idx = guard.reg_idx.min(guard.registers.len().saturating_sub(1));
                            guard.reg_idx += 1;
                            guard
                                .registers
                                .get(idx)
                                .cloned()
                                .unwrap_or((404, "no register".into()))
                        } else {
                            (404, "not found".into())
                        }
                    };
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        500 => "Internal Server Error",
                        _ => "Not Found",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn onboard_retries_with_a_leftover_regist_token() {
        let dir = temp_dir("onboard-retry");
        wist_gwlinkd::state::save_regist_token(&dir, "reg-leftover").expect("save");
        let stub = Arc::new(Mutex::new(Stub {
            registers: vec![(200, register_result_body())],
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        // 遗留 token → 直接注册，**不**再走 link-upstream（也不需要 link）。
        onboard(&client, &config, "ident-1", None)
            .await
            .expect("onboard");
        assert!(
            wist_gwlinkd::state::load_credential(&dir).is_some(),
            "应落长期身份"
        );
        assert!(
            wist_gwlinkd::state::load_regist_token(&dir).is_none(),
            "RegistToken 应已清"
        );
        assert_eq!(
            stub.lock().unwrap().calls,
            vec!["POST /api/v1/gateway/register"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 页面发起接入：有待办 → 接入并回报 `Connected`。
    #[tokio::test]
    async fn onboard_from_gateway_consumes_the_page_request() {
        let dir = temp_dir("link-request");
        let stub = Arc::new(Mutex::new(Stub {
            link: Some((200, link_body(Some("reg-x")))),
            registers: vec![(200, register_result_body())],
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        // 网关桩的 link-request 指向同一桩作 Center（endpoint 即 url）。
        stub.lock().unwrap().link_request = Some((
            200,
            serde_json::json!({
                "has_request": true,
                "gateway_id": "gw-1",
                "center_endpoint": url,
                "link_token": "link_abc",
                "trust_bundle_pem": "",
                "status": "Pending",
            })
            .to_string(),
        ));
        let client = LinkRequestClient::new(url.clone());
        let config = test_config(&dir, url);

        let registered = onboard_from_gateway(&config, None, "ident-1", &client)
            .await
            .expect("onboard");
        assert!(registered, "有待办应完成接入");
        assert!(wist_gwlinkd::state::load_credential(&dir).is_some());
        let calls = stub.lock().unwrap().calls.clone();
        assert!(
            calls
                .iter()
                .any(|c| c == "GET /api/v1/gateway/link-request"),
            "应拉取待办：{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c == "POST /api/v1/gateway/link-result"),
            "应回报结果：{calls:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 无待办：返回 `Ok(false)`，不接入。
    #[tokio::test]
    async fn onboard_from_gateway_reports_no_request() {
        let dir = temp_dir("link-request-none");
        let stub = Arc::new(Mutex::new(Stub {
            link_request: Some((
                200,
                r#"{"has_request":false,"gateway_id":"","center_endpoint":"","link_token":"","trust_bundle_pem":"","status":""}"#.into(),
            )),
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = LinkRequestClient::new(url.clone());
        let config = test_config(&dir, url);

        let registered = onboard_from_gateway(&config, None, "ident-1", &client)
            .await
            .expect("ok");
        assert!(!registered);
        assert!(wist_gwlinkd::state::load_credential(&dir).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn onboard_falls_back_to_full_provision_when_token_is_stale() {
        let dir = temp_dir("onboard-stale");
        wist_gwlinkd::state::save_regist_token(&dir, "reg-stale").expect("save");
        let stub = Arc::new(Mutex::new(Stub {
            link: Some((200, link_body(Some("reg-new")))),
            registers: vec![(401, "unauthorized".into()), (200, register_result_body())],
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        onboard(&client, &config, "ident-1", Some("link-1"))
            .await
            .expect("onboard");
        assert!(wist_gwlinkd::state::load_credential(&dir).is_some());
        assert!(wist_gwlinkd::state::load_regist_token(&dir).is_none());
        assert_eq!(
            stub.lock().unwrap().calls,
            vec![
                "POST /api/v1/gateway/register",
                "GET /api/v1/gateway/link-upstream",
                "POST /api/v1/gateway/register",
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn onboard_keeps_the_regist_token_on_a_transient_register_failure() {
        let dir = temp_dir("onboard-transient");
        wist_gwlinkd::state::save_regist_token(&dir, "reg-keep").expect("save");
        let stub = Arc::new(Mutex::new(Stub {
            registers: vec![(500, "boom".into())],
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        let err = onboard(&client, &config, "ident-1", None).await;
        assert!(err.is_err(), "500 应报错");
        assert_eq!(
            wist_gwlinkd::state::load_regist_token(&dir).as_deref(),
            Some("reg-keep"),
            "网络/服务错应**保留** token 以便下次重试"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn onboard_errors_when_center_says_initialized_but_no_local_credential() {
        let dir = temp_dir("onboard-initialized");
        let stub = Arc::new(Mutex::new(Stub {
            link: Some((200, link_body(None))),
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        let err = onboard(&client, &config, "ident-1", Some("link-1"))
            .await
            .expect_err("应报错");
        assert!(err.contains("身份重置"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn onboard_requires_a_link_token_on_first_run() {
        let dir = temp_dir("onboard-noboot");
        let stub = Arc::new(Mutex::new(Stub::default()));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        let err = onboard(&client, &config, "ident-1", None)
            .await
            .expect_err("应报错");
        assert!(err.contains("WIST_GWLINKD_LINK_TOKEN"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
