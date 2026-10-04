//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `diagnose` / `version`。

use std::path::PathBuf;
use std::process::ExitCode;

use wist_control::ReportGatewayStatus;
use wist_gwlinkd::center::CenterClient;
use wist_gwlinkd::config::Config;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::state;

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

/// 常驻：首跑置备（若未注册），随后周期上报状态。
async fn run(config: &Config) -> Result<(), String> {
    let identity = state::load_or_create_identity(&config.state_dir)?;
    let client = CenterClient::new(config.control_center_endpoint.clone());

    if state::load_credential(&config.state_dir).is_none() {
        onboard(&client, config, &identity).await?;
    }
    let credential = state::load_credential(&config.state_dir)
        .ok_or_else(|| "注册后仍无运行期凭据".to_string())?;

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(STATUS_INTERVAL_SECS));
    loop {
        ticker.tick().await;
        let payload = ReportGatewayStatus {
            gateway_id: config.gateway_id.clone(),
            instance_id: credential.instance_id.clone(),
            version: wist_gwlinkd::VERSION.to_string(),
            status: "running".to_string(),
            health: "ok".to_string(),
            memory_bytes: None,
            cpu_percent: None,
            reported_at: wist_control::DateTime::now(),
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
