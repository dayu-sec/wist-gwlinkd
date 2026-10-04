//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `version`。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::center::CenterClient;
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::selfreport::SelfReportClient;
use wist_gwlinkd::state;
use wist_gwlinkd::upgrade::{DEFAULT_UPGRADER_PROGRAM, UpgradeDriver, UpgradeReporter};

/// 运行期状态上报周期（秒）。
const STATUS_INTERVAL_SECS: u64 = 30;

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

/// 常驻：首跑置备（若未注册），随后**续期 + 拉自述面 + 周期上报**。
async fn run(config: &Config) -> Result<(), String> {
    let identity = state::load_or_create_identity(&config.state_dir)?;
    let client = CenterClient::new(config.control_center_endpoint.clone());

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

    // 手动触发一次升级（占位 CR-002 C2 的「拉 desired」）：设了 WIST_GWLINKD_UPGRADE_TO 就驱动一次。
    if let Ok(to_version) = std::env::var("WIST_GWLINKD_UPGRADE_TO") {
        let reporter = UpgradeReporter {
            client: client.clone(),
            credential: credential.clone(),
        };
        driver
            .start("manual", wist_gwlinkd::VERSION, &to_version, Some(reporter))
            .await?;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(STATUS_INTERVAL_SECS));
    // 已驱过的 plan_id：同一计划不重复驱动（幂等；跨重启由 upgrade.json 留痕继续判断）。
    let mut last_driven: Option<String> = None;
    loop {
        ticker.tick().await;

        // 到期前续期（旧凭据立即失效，新凭据落盘）。
        if DateTime::now().seconds_until(&credential.expires_at) <= renew_lead {
            match client.renew_credential(&credential).await {
                Ok(renewed) => {
                    state::save_credential(&config.state_dir, &renewed)?;
                    credential = renewed;
                    println!("event=CredentialRenewed gateway_id={}", config.gateway_id);
                }
                Err(err) => eprintln!("event=RenewFailed error={err}"),
            }
        }

        // 拉升级目标（CR-002 C1/C2）：有覆盖本网关、目标非当前版本、且未驱过 → 驱动 + 回执。
        match client
            .get_upgrade_plan(&credential, &config.gateway_id)
            .await
        {
            Ok(plan) if plan.has_plan => {
                let to_version = plan.to_version.clone().unwrap_or_default();
                let already = last_driven.as_deref() == plan.plan_id.as_deref();
                if !to_version.is_empty() && to_version != wist_gwlinkd::VERSION && !already {
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
                            wist_gwlinkd::VERSION,
                            &to_version,
                            Some(reporter),
                        )
                        .await?;
                    last_driven = plan.plan_id;
                }
            }
            Ok(_) => {}
            Err(err) => eprintln!("event=UpgradePlanFailed error={err}"),
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
            Err(err) => eprintln!("event=StatusReportFailed error={err}"),
        }
    }
}

/// 首跑置备：link-upstream（一次性 bootstrap + 身份头）→ register → 落运行期凭据。
async fn onboard(client: &CenterClient, config: &Config, identity: &str) -> Result<(), String> {
    let bootstrap = std::env::var("WIST_GWLINKD_BOOTSTRAP_TOKEN").map_err(|_| {
        "首跑需要 WIST_GWLINKD_BOOTSTRAP_TOKEN（中心 admin 创建实例时签发的引导 Token）".to_string()
    })?;
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;

    println!("event=LinkUpstream gateway_id={}", config.gateway_id);
    let returned = client
        .link_upstream(&config.gateway_id, &bootstrap, Some(identity))
        .await?;
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
