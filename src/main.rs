//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `version`。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::center::{self, CenterClient};
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::selfreport::SelfReportClient;
use wist_gwlinkd::state::{self, UpgradeCursor};
use wist_gwlinkd::upgrade::{DEFAULT_UPGRADER_PROGRAM, UpgradeDriver, UpgradeReporter};

/// 运行期状态上报周期（秒）。
const STATUS_INTERVAL_SECS: u64 = 30;
/// renew 失败后的初始退避（秒）。
const RENEW_BACKOFF_BASE_SECS: u64 = 60;
/// renew 退避上限（秒）。
const RENEW_BACKOFF_MAX_SECS: u64 = 1800;

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
    let trust = if trust_bundle.exists() {
        Some(trust_bundle)
    } else {
        eprintln!(
            "event=TrustBundleMissing path={}（回落公共根）",
            trust_bundle.display()
        );
        None
    };
    let http = center::build_http_client(trust)?;
    let client = CenterClient::with_client(config.control_center_endpoint.clone(), http);

    if state::load_credential(&config.state_dir).is_none() {
        onboard(&client, config, &identity).await?;
    }
    let mut credential = state::load_credential(&config.state_dir)
        .ok_or_else(|| "注册后仍无运行期凭据".to_string())?;

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
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(STATUS_INTERVAL_SECS));
    // renew 退避：失败时指数退避，避免每 tick 猛击中心。
    let mut renew_backoff = Duration::ZERO;
    let mut next_renew_at = Instant::now();

    loop {
        ticker.tick().await;

        // 到期前续期（旧凭据立即失效，新凭据**原子落盘**）。
        if DateTime::now().seconds_until(&credential.expires_at) <= renew_lead
            && Instant::now() >= next_renew_at
        {
            match client.renew_credential(&credential).await {
                Ok(renewed) => {
                    state::save_credential(&config.state_dir, &renewed)?;
                    credential = renewed;
                    renew_backoff = Duration::ZERO;
                    next_renew_at = Instant::now();
                    println!("event=CredentialRenewed gateway_id={}", config.gateway_id);
                }
                Err(err) => {
                    renew_backoff = if renew_backoff.is_zero() {
                        Duration::from_secs(RENEW_BACKOFF_BASE_SECS)
                    } else {
                        (renew_backoff * 2).min(Duration::from_secs(RENEW_BACKOFF_MAX_SECS))
                    };
                    next_renew_at = Instant::now() + renew_backoff;
                    eprintln!("event=RenewFailed backoff={renew_backoff:?} error={err}");
                }
            }
        }

        // 拉升级目标（CR-002 C2）：有在飞升级则**互斥跳过**；否则「未驱过的计划」才驱动。
        if !state::upgrade_in_flight(&config.state_dir, SystemTime::now()) {
            match client
                .get_upgrade_plan(&credential, &config.gateway_id)
                .await
            {
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
                            "event=UpgradeDriven plan_id={:?} to_version={to_version}",
                            plan.plan_id
                        );
                        let reporter = UpgradeReporter {
                            client: client.clone(),
                            credential: credential.clone(),
                        };
                        driver
                            .start(
                                plan.plan_id.as_deref().unwrap_or("plan"),
                                &from_version,
                                &to_version,
                                Some(reporter),
                            )
                            .await?;
                        // 先落游标再继续：跨重启幂等据此判定。
                        state::save_upgrade_cursor(
                            &config.state_dir,
                            &UpgradeCursor {
                                last_plan_id: plan.plan_id.clone(),
                                last_to_version: to_version,
                            },
                        )?;
                    }
                }
                Ok(_) => {}
                Err(err) => eprintln!("event=UpgradePlanFailed error={err}"),
            }
        }

        // 拉网关自述面（准确状态的来源）；不答则把「沉默」当判断，上报 unknown。
        let health = match &self_client {
            Some(self_client) => match self_client.fetch(&config.gateway_id).await {
                Ok(self_state) => self_state.health().to_string(),
                Err(err) => {
                    eprintln!("event=SelfStateFailed error={err}");
                    "unknown".to_string()
                }
            },
            None => "ok".to_string(),
        };

        let payload = ReportGatewayStatus {
            gateway_id: config.gateway_id.clone(),
            instance_id: credential.instance_id.clone(),
            version: wist_gwlinkd::VERSION.to_string(),
            status: "running".to_string(),
            health,
            memory_bytes: None,
            cpu_percent: None,
            reported_at: DateTime::now(),
        };
        match client.report_status(&credential, &payload).await {
            Ok(()) => println!("event=StatusReported gateway_id={}", config.gateway_id),
            Err(err) if is_auth_failure(&err) => {
                // 运行期凭据被拒（多为 renew 后崩溃丢新凭据）：要重走置备。
                eprintln!(
                    "event=CredentialRejected gateway_id={}（运行期凭据已失效，需重新置备：设 WIST_GWLINKD_BOOTSTRAP_TOKEN 后重跑）error={err}",
                    config.gateway_id
                );
            }
            Err(err) => eprintln!("event=StatusReportFailed error={err}"),
        }
    }
}

/// 401/403 视为凭据失效（`decode` 的错误串里带状态码）。
fn is_auth_failure(err: &str) -> bool {
    err.contains("401") || err.contains("403")
}

/// 首跑置备：link-upstream（一次性 bootstrap + 身份头）→ 落链接配置 → register → 落运行期凭据。
async fn onboard(client: &CenterClient, config: &Config, identity: &str) -> Result<(), String> {
    let bootstrap = std::env::var("WIST_GWLINKD_BOOTSTRAP_TOKEN").map_err(|_| {
        "首跑需要 WIST_GWLINKD_BOOTSTRAP_TOKEN（中心 admin 创建实例时签发的引导 Token）".to_string()
    })?;
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;

    println!("event=LinkUpstream gateway_id={}", config.gateway_id);
    let returned = client
        .link_upstream(&config.gateway_id, &bootstrap, Some(identity))
        .await?;
    // 链接配置（信任锚 / 协议版本 / 注册 token 引用）落盘留痕 —— 不再丢弃。
    state::save_link_config(&config.state_dir, &returned.config)?;
    let regist_token = returned
        .regist_token
        .ok_or_else(|| "中心未返回 regist_token（网关可能已初始化）".to_string())?;

    println!("event=Register gateway_id={}", config.gateway_id);
    let result = client.register(&regist_token, &instance_id).await?;
    state::save_credential(&config.state_dir, &result.credential_bundle)?;
    println!(
        "event=Registered gateway_id={} credential_id={}",
        result.gateway_id, result.credential_id
    );
    Ok(())
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
