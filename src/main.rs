//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `version`。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::center::{self, CenterClient};
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::identity;
use wist_gwlinkd::selfreport::SelfReportClient;
use wist_gwlinkd::state::{self, CredentialStatus, UpgradeCursor};
use wist_gwlinkd::upgrade::{
    DEFAULT_ON_FAILURE, DEFAULT_UPGRADER_PROGRAM, UpgradeDriver, UpgradeReporter,
};

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
        CredentialStatus::Present(_) => {}
        CredentialStatus::Missing => {
            // 首跑还没客户端证书：用无身份的客户端走 link-upstream + register（bootstrap）。
            let bootstrap = center::build_http_client(trust)?;
            let bootstrap_client =
                CenterClient::with_client(config.control_center_endpoint.clone(), bootstrap);
            onboard(&bootstrap_client, config, &identity).await?;
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
        config
            .upgrader_program
            .clone()
            .unwrap_or_else(|| DEFAULT_UPGRADER_PROGRAM.to_string()),
        config.state_dir.clone(),
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
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(STATUS_INTERVAL_SECS));
    // 一轮里有多次（30s 超时）的网络调用；慢轮后别突发补 tick。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renew_backoff = Duration::ZERO;
    let mut next_renew_at = Instant::now();
    let mut status_backoff = Duration::ZERO;
    let mut next_status_at = Instant::now();

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
                    // 先重建 mTLS 客户端（旧证书已在中心侧作废）；落盘失败不拖死常驻（下一轮还会再续）。
                    match mtls_client(config, trust, &new_credential) {
                        Ok(new_client) => client = new_client,
                        Err(err) => eprintln!("event=MtlsClientRebuildFailed error={err}"),
                    }
                    if let Err(err) = state::save_credential(&config.state_dir, &new_credential) {
                        eprintln!("event=CredentialSaveFailed error={err}");
                    }
                    credential = new_credential;
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
                    // 运行期凭据被拒：**退避**（不每 30s 猛击），并明确要中心重置该实例。
                    status_backoff = back_off(status_backoff);
                    next_status_at = Instant::now() + status_backoff;
                    eprintln!(
                        "event=CredentialRejected gateway_id={} backoff={status_backoff:?}（运行期凭据已失效；需管理员在中心重置该实例后重新置备）error={err}",
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

/// 首跑置备：link-upstream（一次性 bootstrap + 身份头）→ 落链接配置 → 生成密钥对+CSR →
/// register（中心签出客户端证书）→ 落长期身份（证书 + 私钥）。
async fn onboard(client: &CenterClient, config: &Config, identity: &str) -> Result<(), String> {
    let bootstrap = std::env::var("WIST_GWLINKD_BOOTSTRAP_TOKEN").map_err(|_| {
        "首跑需要 WIST_GWLINKD_BOOTSTRAP_TOKEN（中心 admin 创建实例时签发的引导 Token）；\
         若本机曾有身份，请检查 state/credential.json 是否损坏"
            .to_string()
    })?;
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;

    println!("event=LinkUpstream gateway_id={}", config.gateway_id);
    let returned = client
        .link_upstream(&config.gateway_id, &bootstrap, Some(identity))
        .await
        .map_err(|err| err.to_string())?;
    // 链接配置（信任锚 / 协议版本 / 注册 token 引用）落盘留痕 —— 不再丢弃。
    state::save_link_config(&config.state_dir, &returned.config)?;
    let regist_token = returned.regist_token.ok_or_else(|| {
        "中心认为该网关**已初始化**，但本地无客户端证书 —— 无法自动恢复（CR-003 尚缺「身份重置」路径）。\
         请在中心重置该实例后重跑，或把既有的客户端证书/私钥写入 state/credential.json"
            .to_string()
    })?;

    // 首跑当场生成密钥对：私钥不上送，只交 CSR；中心用 CA-G 签出客户端证书。
    let keypair = identity::generate_client_keypair(&config.gateway_id)?;

    println!("event=Register gateway_id={}", config.gateway_id);
    let result = client
        .register(&regist_token, &instance_id, &keypair.csr_pem)
        .await
        .map_err(|err| err.to_string())?;
    let credential = state::StoredCredential {
        bundle: result.credential_bundle,
        private_key_pem: keypair.private_key_pem,
    };
    state::save_credential(&config.state_dir, &credential)?;
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
}
