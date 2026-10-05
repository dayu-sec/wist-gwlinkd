//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `version`。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::center::{self, CenterClient, CenterError};
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::executor::{DEFAULT_ON_FAILURE, DEFAULT_UPGRADER_PROGRAM, GopsExecutor};
use wist_gwlinkd::identity;
use wist_gwlinkd::selfreport::SelfReportClient;
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
            eprintln!("unknown command: {other}（可用：run | diagnose | version）");
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
            // 首跑还没客户端证书：用无身份的客户端走 link-upstream + register（接入）。
            let link_client = CenterClient::with_client(
                config.control_center_endpoint.clone(),
                center::build_http_client(trust)?,
            );
            // 接入券只从环境拿一次、作为可选传入 —— 有遗留 RegistToken 时不需要它。
            let link_token = link_token_from_env();
            onboard(&link_client, config, &identity, link_token.as_deref()).await?;
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

    let self_client = config
        .gateway_self_endpoint
        .as_deref()
        .map(SelfReportClient::new);
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
                    println!("event=StatusReported gateway_id={}", config.gateway_id);
                }
                Err(err) if err.is_unauthorized() => {
                    // 客户端证书被拒：**退避**（不每 30s 猛击），并明确要中心重置该实例。
                    status_backoff = back_off(status_backoff);
                    next_status_at = Instant::now() + status_backoff;
                    eprintln!(
                        "event=CredentialRejected gateway_id={} backoff={status_backoff:?}（客户端证书已失效；需管理员在中心重置该实例后重新置备）error={err}",
                        config.gateway_id
                    );
                }
                Err(err) => eprintln!("event=StatusReportFailed error={err}"),
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
