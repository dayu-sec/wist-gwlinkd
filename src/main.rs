//! `wist-gwlinkd` CLI：`run`（常驻，默认）/ `unlink` / `diagnose` / `service` / `version`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use orion_error::prelude::*;
use wist_control::{DateTime, ReportGatewayStatus};
use wist_gwlinkd::agent_package::{AgentPackageClient, AgentPackageItem};
use wist_gwlinkd::center::{self, CenterClient};
use wist_gwlinkd::config::{
    Config, UpgradeInstall, pub_config_path, remove_link_token, upsert_link_settings,
};
use wist_gwlinkd::deps::ToolVersions;
use wist_gwlinkd::doctor::{self, Status};
use wist_gwlinkd::error::{
    CenterError, CenterReason, CenterResult, GwlinkdError, GwlinkdReason, GwlinkdResult,
    OpLoggable, chain_one_line, logged_op,
};
use wist_gwlinkd::executor::{DEFAULT_ON_FAILURE, DEFAULT_UPGRADER_PROGRAM, GopsExecutor};
use wist_gwlinkd::identity;
use wist_gwlinkd::knowledge_package::KnowledgePackageClient;
use wist_gwlinkd::link_request::LinkRequestClient;
use wist_gwlinkd::linkd_status::{
    self, GwlinkdStatus, STATE_DEGRADED, STATE_LINKED, STATE_WAITING_LINK_REQUEST,
};
use wist_gwlinkd::selfreport::SelfReportClient;
use wist_gwlinkd::service;
use wist_gwlinkd::state::{self, CredentialStatus, UpgradeCursor, UpgradeRecord};
use wist_gwlinkd::task_log::TaskLog;
use wist_gwlinkd::tool_install::ToolInstaller;
use wist_gwlinkd::unlink::{self, UnlinkReport};
use wist_gwlinkd::upgrade::{
    RECOVERY_VERIFY_TIMEOUT, SelfUpdateTarget, UpgradeDriver, UpgradeReporter,
    settle_pending_self_update,
};

/// 运行期状态上报周期（秒）。
const STATUS_INTERVAL_SECS: u64 = 30;
/// 退避初始值（秒）：renew / status 被拒后指数退避，避免猛击中心。
const BACKOFF_BASE_SECS: u64 = 60;
/// 退避上限（秒）。
const BACKOFF_MAX_SECS: u64 = 1800;

/// 宿主依赖工具版本刷新间隔（秒）：安装 / 升级 gops、gx 后不必重启 gwlinkd。
const TOOL_VERSION_REFRESH_SECS: u64 = 600;

fn config_path() -> PathBuf {
    std::env::var("WIST_GWLINKD_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(wist_gwlinkd::DEFAULT_CONFIG_PATH))
}

/// 解析**全局**选项 `--conf <path>` / `--pub <path>`（短名 `-c` / `-p`；也接受
/// `--conf=<path>` / `-c=<path>` 这类内联写法），把它们从参数里摘除；返回 `(conf, pub, 其余参数)`。
///
/// 为什么要它：主配置 / 现场态声明的可加载路径原本只能靠环境变量（`WIST_GWLINKD_CONFIG` /
/// `WIST_GWLINKD_PUB_CONFIG`）给。`-c` / `-p` / `--conf` / `--pub` 让手动跑（如 `diagnose`）不必先 export。
/// 与 `service` 子命令自己的 `--config` / `--pub-config` 不冲突（名不同、语义不同：那是装服务时写进 unit 的）。
///
/// 短名收在这里而不是让子命令各自实现：写 `-c <path>` 却没人认识时，它会被静默当成普通参数丢掉，
/// 于是配置回落到默认的 `./gwlinkd.toml`，报出来的是「读不到 gwlinkd.toml」—— 极易误判成配置坏了。
fn parse_global_paths(args: &[String]) -> (Option<PathBuf>, Option<PathBuf>, Vec<String>) {
    let mut conf = None;
    let mut pub_path = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        // 支持 `--x value` 与 `--x=value` 两种写法。
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let target = match flag {
            "--conf" | "-c" => Some(&mut conf),
            "--pub" | "-p" => Some(&mut pub_path),
            _ => None,
        };
        if let Some(slot) = target {
            let value = match inline {
                Some(value) => value,
                None => {
                    i += 1;
                    match args.get(i) {
                        Some(value) => value.clone(),
                        None => {
                            eprintln!("选项 {flag} 缺参数（用法：{flag} <path>）");
                            std::process::exit(2);
                        }
                    }
                }
            };
            *slot = Some(PathBuf::from(value));
        } else {
            rest.push(arg.clone());
        }
        i += 1;
    }
    (conf, pub_path, rest)
}

/// 把命令行给的相对路径**按当前工作目录绝对化**（已是绝对路径则原样返回）。
///
/// 为什么需要：`wist-config` 把现场态声明（`WIST_GWLINKD_PUB_CONFIG`）当**相对主配置目录**解析。
/// 若 `--conf` / `--pub` 都给相对路径、而主配置又在子目录里，`--pub` 会被算到主配置旁边而非
/// 用户在 shell 里写下它的那个目录 —— 不是命令行用法的预期。两个都先按 cwd 绝对化，寻址就只由
/// cwd 决定。不用 `canonicalize`：现场态声明可能**尚不存在**（接入时由 gwlinkd 创建）。
fn absolutize_from_cwd(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

/// `--conf` 缺省时，为**只读**子命令回落到已装服务定义里的配置。
///
/// 为什么：gwlinkd 是 host 侧常驻，它跑的那份配置就写在服务定义里（两个环境变量），而路径长到
/// 没人愿意敲两遍 —— 敲错的名字还会被静默丢掉、回落到 `./gwlinkd.toml`（2026-10-15 实际踩过）。
/// 有它，`wist-gwlinkd diagnose` 单独就够。回落发生时**明说一句**（写 stderr，不污染报告）。
fn fallback_to_installed_config() {
    if std::env::var_os("WIST_GWLINKD_CONFIG").is_some() {
        return;
    }
    let Some(installed) = service::any_installed_definition() else {
        return;
    };
    let Some(config) = installed.paths.config_path else {
        return;
    };
    if !config.is_file() {
        return;
    }
    eprintln!(
        "（未给 --conf/-c：回落已装服务定义 {} 的配置）",
        installed.definition_path.display()
    );
    // SAFETY: 单进程启动期，此处先于任何线程读环境变量，无并发 reader。
    unsafe { std::env::set_var("WIST_GWLINKD_CONFIG", &config) };
    if let Some(pub_config) = installed.paths.pub_config {
        // SAFETY: 同上。
        unsafe { std::env::set_var("WIST_GWLINKD_PUB_CONFIG", pub_config) };
    }
}

/// 按 `[log]` 段初始化运行日志（统一走共享 crate `wist-log`）。
fn log_init(section: &wist_log::LogSection) {
    wist_log::init(
        section,
        &wist_log::Identity::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
    );
}

/// 尽力按配置文件里的 `[log]` 段初始化日志；配置读不了就回落缺省（stderr / info）。
///
/// 用于不强制读配置的子命令（`service`）—— 有配置就尊重它的落点 / 级别，没有也不报错。
fn init_logging_from_config_or_default() {
    match Config::load(&config_path()) {
        Ok(config) => log_init(&config.log),
        Err(_) => log_init(&wist_log::LogSection::default()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // 日志在**读到配置之后**才初始化（`[log]` 段决定级别 / 格式 / 落点）；配置读不了时
    // 日志还没起来，就直写 stderr —— 不要让启动失败静默。
    //
    // 先摘出全局选项 `--conf` / `--pub`（它们决定配置落点），落成环境变量供 `wist-config`
    // 分层加载寻址 —— 这样手动跑（如 `diagnose`）不必先 export 那两个变量。
    let all: Vec<String> = std::env::args().collect();
    let (conf, pub_path, rest) = parse_global_paths(&all[1..]);
    if let Some(conf) = conf {
        // SAFETY: 单进程启动期，此处先于任何线程读环境变量，无并发 reader。
        unsafe { std::env::set_var("WIST_GWLINKD_CONFIG", absolutize_from_cwd(conf)) };
    }
    if let Some(pub_path) = pub_path {
        // SAFETY: 同上。
        unsafe { std::env::set_var("WIST_GWLINKD_PUB_CONFIG", absolutize_from_cwd(pub_path)) };
    }
    let command = rest.first().cloned().unwrap_or_else(|| "run".to_string());
    // 只读子命令：没给 `--conf`（也没设环境变量）时，回落到**已装服务定义**里那份配置。
    // 只给 `diagnose`：`run` 落到别的配置太隐晦；写类子命令（`service install|uninstall`）也不用。
    if command == "diagnose" {
        fallback_to_installed_config();
    }
    match command.as_str() {
        "version" | "-V" | "--version" => {
            println!("wist-gwlinkd {}", wist_gwlinkd::VERSION);
            ExitCode::SUCCESS
        }
        "diagnose" => match Config::load(&config_path()) {
            Ok(config) => {
                log_init(&config.log);
                run_diagnose(&config)
            }
            Err(err) => {
                eprintln!("[FAIL] 配置不可读：{}", err.display_chain());
                ExitCode::FAILURE
            }
        },
        "service" => {
            init_logging_from_config_or_default();
            run_service(&rest[1..])
        }
        "unlink" => run_unlink(&rest[1..]).await,
        "init-config" => match init_config_command(rest.get(1).cloned()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("[FAIL] {}", err.display_chain());
                ExitCode::FAILURE
            }
        },
        "run" => {
            let path = config_path();
            match Config::load(&path) {
                Ok(config) => {
                    log_init(&config.log);
                    match run(&config, &path).await {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(err) => {
                            log::error!("event=RunFailed error={}", chain_one_line(&err));
                            eprintln!("[FAIL] {}", err.display_chain());
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[FAIL] 配置不可读：{}", err.display_chain());
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!(
                "unknown command: {other}（可用：run | unlink | diagnose | service | init-config | version）"
            );
            ExitCode::from(2)
        }
    }
}

/// 生成一份带注释的 `gwlinkd.toml` 骶架 + 现场态 `gwlinkd-pub.toml`（`init-config [路径]`）。
///
/// 与 `wist-gateway init-config` 同一套路：由程序生成，避免手抄。**不含密钥**。
/// 主配置已存在则**覆盖**（它是交付物）；现场态配置**只在缺失时补**（它承载现场选择，
/// 绝不能被一次 init 抹掉）。
fn init_config_command(out_arg: Option<String>) -> GwlinkdResult<()> {
    let out_path = out_arg.map(PathBuf::from).unwrap_or_else(config_path);
    if let Some(parent) = out_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).source_err(
            GwlinkdReason::Config,
            format!("创建目录失败 {}", parent.display()),
        )?;
    }
    let existed = out_path.exists();
    std::fs::write(&out_path, wist_gwlinkd::config::default_config_text()).source_err(
        GwlinkdReason::Config,
        format!("写入失败 {}", out_path.display()),
    )?;
    if existed {
        println!("已覆盖原有配置：{}", out_path.display());
    } else {
        println!("已生成配置：{}", out_path.display());
    }

    // 现场态声明：**按缺补齐**（已存在就不动 —— 里面是声明，但现场可能改过变量名）。
    let pub_path = pub_config_path(&out_path);
    if pub_path.exists() {
        println!("现场态声明已存在，保留：{}", pub_path.display());
    } else {
        write_new(&pub_path, wist_gwlinkd::config::default_pub_config_text())?;
        println!("已生成现场态声明：{}", pub_path.display());
    }
    // 现场态**值**（`.env`）：同样**按缺补齐** —— 已有值一律保留（那是现场填的）。
    let env_path = wist_gwlinkd::config::pub_env_path(&pub_path);
    if env_path.exists() {
        println!("现场态值已存在，保留：{}", env_path.display());
    } else {
        write_new(&env_path, wist_gwlinkd::config::default_env_text())?;
        println!("已生成现场态值骨架：{}", env_path.display());
    }
    println!(
        "下一步：填 {} 里的 GWLINKD_CENTER_ENDPOINT（自签中心再填 GWLINKD_TRUST_BUNDLE），然后 `wist-gwlinkd run`；",
        env_path.display()
    );
    println!("       或走网关「链接上级」页提交接入链接 —— gwlinkd 会把值写回该文件。");
    Ok(())
}

/// 写一个新文件（父目录自建）；给“按缺补齐”的骨架用。
fn write_new(path: &Path, text: String) -> GwlinkdResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).source_err(
            GwlinkdReason::Config,
            format!("创建目录失败 {}", parent.display()),
        )?;
    }
    std::fs::write(path, text).source_err(
        GwlinkdReason::Config,
        format!("写入失败 {}", path.display()),
    )
}

/// 常驻：单实例 → 首跑置备（若未注册）→ 周期【续期 / 拉升级目标 / 拉自述面 / 上报状态】。
async fn run(config: &Config, path: &Path) -> GwlinkdResult<()> {
    // 单实例：同机只允许一个常驻（否则双重上报 + 双重驱动升级）。持有到进程退出。
    let _lock = state::acquire_single_instance_lock(&config.state_dir)
        .map_err(|err| GwlinkdReason::AlreadyRunning.err(err))?;

    let identity = state::load_or_create_identity(&config.state_dir)
        .map_err(|err| GwlinkdReason::Identity.err(err))?;

    match state::credential_status(config.identity_dir()) {
        CredentialStatus::Present(_) => {
            // 已有客户端证书：遗留的 RegistToken 一定是旧的，清掉（避免误用）。
            state::clear_regist_token(&config.state_dir);
        }
        CredentialStatus::Missing => {
            // 首跑还没客户端证书：先看网关页有没有提交「接入请求」（环回），否则回退 env 券。
            let trust_bundle = config.trust_bundle.clone();
            let trust: Option<&Path> = trust_bundle.exists().then_some(trust_bundle.as_path());
            first_run(path, config, trust, &identity).await?;
        }
        CredentialStatus::Corrupt(detail) => {
            // 损坏 ≠ 缺失：不能静默重置备（会覆盖掉唯一一份长期身份）。
            return Err(GwlinkdReason::Identity.err(format!(
                "长期身份损坏：{detail}；修复或删除身份目录下的 credential.json 后重跑（若中心已初始化该实例，需先在中心重置）"
            )));
        }
    }
    // 首跑那步可能**改写了配置文件**（写入 gateway_id / center endpoint / 券）——在这里**重读**一次：
    // 否则后续周期（上报 / 续期 / 升级）仍用**旧 gateway_id**，中心按客户端证书认人 → `certificate_mismatch`。
    let reloaded = Config::load(path).conv_err()?;
    let config: &Config = &reloaded;
    let trust_bundle = config.trust_bundle.clone();
    let trust: Option<&Path> = if trust_bundle.exists() {
        Some(trust_bundle.as_path())
    } else {
        log::warn!(
            "event=TrustBundleMissing path={}（回落公共根）",
            trust_bundle.display()
        );
        None
    };

    let mut credential = state::load_credential(config.identity_dir())
        .ok_or_else(|| GwlinkdReason::Identity.err("注册后仍无长期身份"))?;
    // 注册后所有网关面调用都走 **mTLS**（客户端证书认人）；续期后重建。
    let mut client = mtls_client(config, trust, &credential)?;
    // 规则发布命令中继：独立任务，短轮询 Center 命令队列并调用本机 wparse admin。
    // 任务内部自己重建 mTLS 客户端（证书轮换后旧客户端失效）。
    if config.wparse_relay.enabled {
        let relay_config = config.clone();
        tokio::spawn(async move {
            wist_gwlinkd::wparse_relay::run(relay_config).await;
        });
    }
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)
        .map_err(|err| GwlinkdReason::Identity.err(err))?;

    let self_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(
            SelfReportClient::with_trust(base, config.gateway_self_ca.as_deref())
                .map_err(|err| GwlinkdReason::system_error().err(err))?,
        ),
        None => None,
    };
    // gwlinkd 心跳：推自身状态给网关（页面拉不到 gwlinkd —— 它纯出站）。同环回面 + 同一信任锚。
    let linkd_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(
            LinkRequestClient::with_trust(base, config.gateway_self_ca.as_deref())
                .map_err(|err| GwlinkdReason::system_error().err(err))?,
        ),
        None => None,
    };
    // 「Agent 包下发」（发布 ②）：同环回面 + 同一信任锚，把中心派下的 agentd 包写进网关包管理。
    let agent_package_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(
            AgentPackageClient::with_trust(base, config.gateway_self_ca.as_deref())
                .map_err(|err| GwlinkdReason::system_error().err(err))?,
        ),
        None => None,
    };
    // 「知识库包下发」：同环回面 + 同一信任锚，把中心派下的 wist-knowledge 包写进网关知识库管理。
    let knowledge_package_client = match config.gateway_self_endpoint.as_deref() {
        Some(base) => Some(
            KnowledgePackageClient::with_trust(base, config.gateway_self_ca.as_deref())
                .map_err(|err| GwlinkdReason::system_error().err(err))?,
        ),
        None => None,
    };
    let renew_lead = config.renew_lead_seconds.unwrap_or(3600);
    let mut driver = UpgradeDriver::new(
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
    ))
    // ① 升级的取件也下沉到 gwlinkd：gops 不持中心信任，直接把中心地址交给它会在 HEAD 阶段
    // 报 `error sending request`（私有 CA）。装上制品客户端后，驱动先取到本地再给 gops 路径。
    .with_artifact_client(client.artifact_http_client());
    // 无状态工具目录（`[[upgrade.component]] install = "tool-copy"`）：装配进程内安装器。
    // 这类组件不经 gops 工程（也就绕开 `upgrade_project_dir` 前置），就地覆盖 `PATH` 上的二进制。
    let tool_binaries: BTreeMap<String, String> = config
        .upgrade
        .component
        .iter()
        .filter(|entry| entry.install == UpgradeInstall::ToolCopy)
        .map(|entry| {
            if entry.binary.as_deref().unwrap_or("").trim().is_empty() {
                log::warn!(
                    "event=UpgradeComponentInvalid name={} install=tool-copy 缺 binary（将没法安装）",
                    entry.name
                );
            }
            (
                entry.name.clone(),
                entry.binary.clone().unwrap_or_default(),
            )
        })
        .collect();
    // 即便组件目录为空也要装配安装器：`wist-gwlinkd` 自身的自更新走同一套取件 / 摘要 / 架构护栏。
    driver = driver.with_tools(
        ToolInstaller::new(
            tool_binaries,
            config.state_dir.clone(),
            client.artifact_http_client(),
        )
        // 缺省要求制品架构可校验且与本机一致（错架构会静默报废工具）。
        .with_require_verified_arch(config.upgrade_tool_require_arch.unwrap_or(true)),
    );
    // 自更新目标：服务定义里登记的二进制 + 当前进程。没有服务定义 = 手动 run，自更新会被前置拒掉
    // （覆盖后退出，没人再拉起）。
    if let Some(definition) = service::any_installed_definition()
        && let Some(service_bin) = definition.paths.bin
        && let Ok(running_exe) = std::env::current_exe()
    {
        driver = driver.with_self_update_target(SelfUpdateTarget::new(service_bin, running_exe));
    }

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
    // 宿主侧依赖工具（gops / gx，装在 `$HOME/bin`）版本：启动探一次，之后定期刷新。
    let mut tool_versions = ToolVersions::detect(config.upgrader_program.as_deref()).await;
    let mut next_tool_probe_at = Instant::now() + Duration::from_secs(TOOL_VERSION_REFRESH_SECS);

    // 启动收尾：上次运行留下的「被判死」升级（升级器被中断，机器可能停在中间态）。
    // 放在循环外只做一次（这是「上一次运行」的遗留态，不是运行中会反复出现的东西）。
    if state::upgrader_is_declared_dead(&config.state_dir, SystemTime::now())
        && state::load_upgrade_cursor(&config.state_dir)
            .last_plan_id
            .is_some()
    {
        if config.upgrade_retry_on_dead.unwrap_or(true) {
            // 清游标 → 同一计划可被下面的循环重新驱动。并发安全：gops 自带工程交付锁串行化两次调用。
            log::warn!("event=DeadUpgradeDetected 清游标以便重驱同一计划");
            if let Err(err) =
                state::save_upgrade_cursor(&config.state_dir, &UpgradeCursor::default())
            {
                log::error!("event=CursorClearFailed error={}", chain_one_line(&err));
            }
        } else {
            log::warn!(
                "event=DeadUpgradeDetected 未自动重试（upgrade_retry_on_dead=false）：请到管理面重派升级"
            );
        }
    }

    loop {
        ticker.tick().await;

        // 依赖工具版本变化很慢（安装 / 升级才变），不必每拍探测；定期刷新即可。
        if Instant::now() >= next_tool_probe_at {
            tool_versions = ToolVersions::detect(config.upgrader_program.as_deref()).await;
            next_tool_probe_at = Instant::now() + Duration::from_secs(TOOL_VERSION_REFRESH_SECS);
        }

        // 上一次是**自更新**（记录停在 `restarting`）→ 用当前编译版本核对并回执；
        // 回执失败保持 `restarting`，下一轮再试（不把“没送到”记成“已完成”）。
        settle_pending_self_update(
            &config.state_dir,
            config.identity_dir(),
            client.clone(),
            wist_gwlinkd::VERSION,
        )
        .await;

        // 上一轮轮换若有收尾未完成（内存/磁盘/客户端未对齐），先自愈重试。
        if credential_dirty
            && let Err(err) = state::save_credential(config.identity_dir(), &credential)
        {
            log::warn!(
                "event=CredentialSaveRetryFailed error={}",
                chain_one_line(&err)
            );
        } else if credential_dirty {
            credential_dirty = false;
            log::info!("event=CredentialSaved gateway_id={}", config.gateway_id);
        }
        if client_stale {
            match mtls_client(config, trust, &credential) {
                Ok(rebuilt) => {
                    client = rebuilt;
                    client_stale = false;
                    log::info!("event=MtlsClientRebuilt gateway_id={}", config.gateway_id);
                }
                Err(err) => log::warn!(
                    "event=MtlsClientRebuildRetryFailed error={}",
                    chain_one_line(&err)
                ),
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
                    log::warn!(
                        "event=RenewFailed backoff={renew_backoff:?} error={}",
                        chain_one_line(&err)
                    );
                    continue;
                }
            };
            let current_serial = credential.certificate_serial_hex().unwrap_or_default();
            match logged_op(
                module_path!(),
                "center renew credential",
                &[("gateway_id", config.gateway_id.clone())],
                client
                    .renew_credential(&config.gateway_id, &current_serial, &keypair.csr_pem)
                    .await,
            ) {
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
                            log::error!(
                                "event=MtlsClientRebuildFailed error={}",
                                chain_one_line(&err)
                            );
                        }
                    }
                    match state::save_credential(config.identity_dir(), &credential) {
                        Ok(()) => credential_dirty = false,
                        Err(err) => {
                            credential_dirty = true;
                            log::error!(
                                "event=CredentialSaveFailed error={}",
                                chain_one_line(&err)
                            );
                        }
                    }
                    renew_backoff = Duration::ZERO;
                    next_renew_at = Instant::now();
                    log::info!("event=CredentialRenewed gateway_id={}", config.gateway_id);
                }
                Err(err) => {
                    renew_backoff = back_off(renew_backoff);
                    next_renew_at = Instant::now() + renew_backoff;
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("凭据续期失败：{err}"));
                    log::warn!(
                        "event=RenewFailed backoff={renew_backoff:?} error={}",
                        chain_one_line(&err)
                    );
                }
            }
        }

        // 拉升级目标（CR-002 C2）：有在飞升级则**互斥跳过**；否则「未驱过的计划」才驱动。
        if !state::upgrade_in_flight(&config.state_dir, SystemTime::now()) {
            // 向中心声明本机平台（target-triple）：中心据此挑平台匹配的制品下发地址
            // （多平台组件不声明就会拿到错平台制品）。认不出平台的罕见主机退化为不声明。
            let host_platform = wist_gwlinkd::target::HostTarget::detect().target_triple();
            match client
                .get_upgrade_plan(&config.gateway_id, host_platform.as_deref())
                .await
            {
                Ok(plan) if plan.has_plan => {
                    let cursor = state::load_upgrade_cursor(&config.state_dir);
                    let already = plan.plan_id.is_some() && plan.plan_id == cursor.last_plan_id;
                    let to_version = plan.to_version.clone().unwrap_or_default();
                    // 动作缺省 = `upgrade`（老中心不带 `action`）。
                    let action = plan
                        .action
                        .as_deref()
                        .unwrap_or(wist_control::ACTION_UPGRADE);
                    let intent = plan_intent(action);
                    // **只有升级**必须有目标版本；包下发（agent / 知识库）与未识动作也要进来：
                    // 否则空版本的计划会每轮重试且永远没有回执。
                    if !already {
                        // 从版本取游标记的「上次目标」；不知道就 unknown（**不再拿 gwlinkd 自身版本硬比** —— 版本空间不同）。
                        let from_version = if cursor.last_to_version.is_empty() {
                            "unknown".to_string()
                        } else {
                            cursor.last_to_version.clone()
                        };
                        log::info!(
                            "event=UpgradeDriven plan_id={:?} action={action} to_version={to_version} component={:?} sha256={}",
                            plan.plan_id,
                            plan.component,
                            plan.artifact_sha256.is_some()
                        );
                        if intent == PlanIntent::PushAgentPackage {
                            // ②「Agent 包下发」：环回把包写进网关包管理 —— **不重建网关**，升不升由网关决定。
                            match &agent_package_client {
                                Some(pusher) => {
                                    drive_agent_package_push(config, &client, pusher, &plan).await
                                }
                                None => {
                                    log::warn!(
                                        "event=AgentPackagePushSkipped reason=no_gateway_self_endpoint plan_id={:?}",
                                        plan.plan_id
                                    );
                                    finish_plan(
                                        config,
                                        &client,
                                        &plan,
                                        "failed",
                                        "本机未配置 gateway_self_endpoint（无法环回交付网关）"
                                            .to_string(),
                                        None,
                                        None,
                                    )
                                    .await;
                                }
                            }
                        } else if intent == PlanIntent::PushKnowledgePackage {
                            // 「知识库包下发」：环回把包写进网关知识库管理（录入，可选激活）—— 不重建网关。
                            match &knowledge_package_client {
                                Some(pusher) => {
                                    drive_knowledge_package_push(config, &client, pusher, &plan)
                                        .await
                                }
                                None => {
                                    log::warn!(
                                        "event=KnowledgePackagePushSkipped reason=no_gateway_self_endpoint plan_id={:?}",
                                        plan.plan_id
                                    );
                                    finish_plan(
                                        config,
                                        &client,
                                        &plan,
                                        "failed",
                                        "本机未配置 gateway_self_endpoint（无法环回交付网关）"
                                            .to_string(),
                                        None,
                                        None,
                                    )
                                    .await;
                                }
                            }
                        } else if intent == PlanIntent::Upgrade && to_version.is_empty() {
                            // 升级没有目标版本：**终态失败**并落游标。
                            // 中心按版本反查制品，留空就派生不出地址 —— 网关无事可做；不落游标
                            // 就会每 30s 重取一次，条目永远停在 dispatched（现场就是“没有反馈”）。
                            log::warn!(
                                "event=UpgradePlanWithoutVersionSkipped plan_id={:?}",
                                plan.plan_id
                            );
                            finish_plan(
                                config,
                                &client,
                                &plan,
                                "failed",
                                "计划未带目标版本：中心无法派生制品地址（请重新创建带版本的计划）"
                                    .to_string(),
                                None,
                                None,
                            )
                            .await;
                        } else if intent == PlanIntent::Upgrade {
                            let reporter = UpgradeReporter {
                                client: client.clone(),
                                state_dir: config.state_dir.clone(),
                                identity_dir: config.identity_dir().to_path_buf(),
                                self_client: self_client.clone(),
                            };
                            // **不**用 `?`：驱动失败（执行器缺失/架构不符…）绝不能把链路常驻整个拖死。
                            match logged_op(
                                module_path!(),
                                "upgrade drive",
                                &[
                                    ("work_id", plan.plan_id.clone().unwrap_or_default()),
                                    ("to_version", to_version.clone()),
                                ],
                                driver
                                    .start_with_digest(
                                        plan.plan_id.as_deref().unwrap_or("plan"),
                                        &from_version,
                                        &to_version,
                                        plan.component.as_deref(),
                                        Some(reporter),
                                        // 中心派生的制品地址（执行器取件用它）；无则回落 `to_version`。
                                        plan.artifact_url.as_deref(),
                                        // 中心带的期望摘要；gops 路径忽略它，无状态工具路径用它。
                                        plan.artifact_sha256.as_deref(),
                                    )
                                    .await,
                            ) {
                                Ok(()) => {
                                    // 先落游标再继续：跨重启幂等据此判定。
                                    if let Err(err) = state::save_upgrade_cursor(
                                        &config.state_dir,
                                        &UpgradeCursor {
                                            last_plan_id: plan.plan_id.clone(),
                                            last_to_version: to_version.clone(),
                                        },
                                    ) {
                                        log::warn!(
                                            "event=CursorSaveFailed error={}",
                                            chain_one_line(&err)
                                        );
                                    }
                                    // 自更新已覆盖本进程的二进制：现在退出，由服务管理器拉起新版本；
                                    // 新进程启动时核对版本并回执中心（`settle_pending_self_update`）。
                                    if driver.take_restart_requested() {
                                        log::info!(
                                            "event=SelfUpdateExit plan_id={:?} to_version={to_version}",
                                            plan.plan_id
                                        );
                                        return Ok(());
                                    }
                                }
                                Err(err) => log::error!(
                                    "event=UpgradeDriveFailed error={}",
                                    chain_one_line(&err)
                                ),
                            }
                        } else {
                            // 未知动作：**不**回落成 `upgrade` —— 拿知识库 / 新类型包去跑 gops 会直接搞坏
                            // 网关。回执失败并落游标（不再每轮重试，也不再静默）。
                            log::warn!(
                                "event=UnknownPlanActionSkipped plan_id={:?} action={action}",
                                plan.plan_id
                            );
                            finish_plan(
                                config,
                                &client,
                                &plan,
                                "failed",
                                format!(
                                    "wist-gwlinkd 不支持计划动作 `{action}`（先升级 gwlinkd 再重派）"
                                ),
                                None,
                                None,
                            )
                            .await;
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => log::warn!("event=UpgradePlanFailed error={}", chain_one_line(&err)),
            }
        }

        // 拉网关自述面：**准确状态 + 网关版本 + 进程资源**的来源；不答则把「沉默」当判断。
        let self_state = match &self_client {
            Some(self_client) => match self_client.fetch(&config.gateway_id).await {
                Ok(self_state) => Some(self_state),
                Err(err) => {
                    log::warn!("event=SelfStateFailed error={}", chain_one_line(&err));
                    None
                }
            },
            None => None,
        };
        let health = self_state
            .as_ref()
            .map(|state| state.health().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let gateway_version = self_state
            .as_ref()
            .map(|state| state.version.clone())
            .unwrap_or_else(|| "unknown".to_string());

        if Instant::now() >= next_status_at {
            let payload = ReportGatewayStatus {
                gateway_id: config.gateway_id.clone(),
                instance_id: instance_id.clone(),
                // 网关对外域名（对外基址）：由自述面带来；老网关不吐这个键 → None。
                public_base_url: self_state
                    .as_ref()
                    .and_then(|state| state.public_base_url.clone()),
                // 报的是**网关（容器）版本**，不是 gwlinkd 自身版本 —— 这条状态描述的是网关。
                version: gateway_version,
                gops_version: tool_versions.gops.clone(),
                gx_version: tool_versions.gx.clone(),
                // 中心侧约定：`status` 是**在线/离线**（中心按 `== "online"` 计数与展示）。
                // 之前误发 `"running"`（那是生命周期 `Running` 的概念）→ 中心把在跑的网关算成离线。
                status: "online".to_string(),
                health,
                // 网关**进程自身**资源：自述面量到的原样上报（量不出则 None，不假装 0）。
                // `memory_bytes` 上报契约是 `i64`（自述面用 `u64`，正数转换无损）。
                memory_bytes: self_state
                    .as_ref()
                    .and_then(|state| state.memory_bytes)
                    .map(|bytes| bytes as i64),
                cpu_percent: self_state.as_ref().and_then(|state| state.cpu_percent),
                // 富化：机队 / 存储 / 数据面 / 主机资源（量不出即 None）—— 与自述面同一份值。
                uptime_seconds: self_state.as_ref().map(|state| state.uptime_seconds),
                agent_count: self_state.as_ref().map(|state| state.agent_count),
                online_agents: self_state.as_ref().map(|state| state.online_agents),
                offline_agents: self_state.as_ref().map(|state| state.offline_agents),
                last_seen_lag_seconds: self_state.as_ref().map(|state| state.last_seen_lag_seconds),
                store_bytes: self_state.as_ref().map(|state| state.store_bytes as i64),
                ingest_accepted_total: self_state
                    .as_ref()
                    .map(|state| state.ingest_accepted_total as i64),
                ingest_rejected_total: self_state
                    .as_ref()
                    .map(|state| state.ingest_rejected_total as i64),
                last_ingest_at: self_state
                    .as_ref()
                    .and_then(|state| state.last_ingest_at.clone()),
                memory_total_bytes: self_state
                    .as_ref()
                    .and_then(|state| state.memory_total_bytes)
                    .map(|bytes| bytes as i64),
                load_1m: self_state.as_ref().and_then(|state| state.load_1m),
                load_5m: self_state.as_ref().and_then(|state| state.load_5m),
                load_15m: self_state.as_ref().and_then(|state| state.load_15m),
                disk_usage_percent: self_state
                    .as_ref()
                    .and_then(|state| state.disk_usage_percent),
                disk_total_bytes: self_state
                    .as_ref()
                    .and_then(|state| state.disk_total_bytes)
                    .map(|bytes| bytes as i64),
                disk_available_bytes: self_state
                    .as_ref()
                    .and_then(|state| state.disk_available_bytes)
                    .map(|bytes| bytes as i64),
                reported_at: DateTime::now(),
            };
            match client.report_status(&payload).await {
                Ok(()) => {
                    status_backoff = Duration::ZERO;
                    linkd_state = STATE_LINKED.to_string();
                    linkd_last_error = None;
                    linkd_last_report_at = Some(linkd_status::now_rfc3339());
                    log::info!("event=StatusReported gateway_id={}", config.gateway_id);
                }
                Err(err) if err.is_unauthorized() => {
                    // 客户端证书被拒：**退避**（不每 30s 猛击），并明确要中心重置该实例。
                    status_backoff = back_off(status_backoff);
                    next_status_at = Instant::now() + status_backoff;
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("中心拒绝了客户端证书：{err}"));
                    log::error!(
                        "event=CredentialRejected gateway_id={} backoff={status_backoff:?}（客户端证书已失效；需管理员在中心重置该实例后重新置备）error={}",
                        config.gateway_id,
                        chain_one_line(&err)
                    );
                }
                Err(err) => {
                    linkd_state = STATE_DEGRADED.to_string();
                    linkd_last_error = Some(format!("状态上报失败：{err}"));
                    log::error!("event=StatusReportFailed error={}", chain_one_line(&err));
                }
            }
        }

        // 推 gwlinkd 自身状态（心跳）给网关：页面据此展示「宿主侧常驻在不在跑」。
        if let Some(linkd_client) = &linkd_client {
            let status = GwlinkdStatus {
                gateway_id: config.gateway_id.clone(),
                instance_id: instance_id.clone(),
                version: wist_gwlinkd::VERSION.to_string(),
                gops_version: tool_versions.gops.clone(),
                gx_version: tool_versions.gx.clone(),
                center_endpoint: config.control_center_endpoint.clone(),
                state: linkd_state.clone(),
                credential_expires_at: credential
                    .bundle
                    .not_after
                    .as_ref()
                    .map(|expires| expires.to_chrono().to_rfc3339()),
                last_center_report_at: linkd_last_report_at.clone(),
                last_error: linkd_last_error.clone(),
                reported_at: DateTime::now(),
            };
            if let Err(err) = linkd_client.report_linkd_status(&status).await {
                log::warn!("event=LinkdStatusPushFailed error={}", chain_one_line(&err));
            }
        }
    }
}

/// 计划动作 → gwlinkd 的执行意图。
///
/// **未知动作绝不回落成升级**（2026-10-18 事故：旧版本把 `push-knowledge-package` 当 `upgrade` 跑 gops，
/// 报「未知系统 wist-knowledge」）。这里把分派做成**纯函数**并单测钉住 —— 新增动作忘了加分支时，
/// 结果是「拒绝并留痕」而不是「拿错包去装网关」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanIntent {
    Upgrade,
    PushAgentPackage,
    PushKnowledgePackage,
    Unsupported,
}

fn plan_intent(action: &str) -> PlanIntent {
    match action {
        wist_control::ACTION_UPGRADE => PlanIntent::Upgrade,
        wist_control::ACTION_PUSH_AGENT_PACKAGE => PlanIntent::PushAgentPackage,
        wist_control::ACTION_PUSH_KNOWLEDGE_PACKAGE => PlanIntent::PushKnowledgePackage,
        _ => PlanIntent::Unsupported,
    }
}

/// 「一份计划只驱一次」的收尾：回报中心（成败都报）+ 落游标（成败都落）。
///
/// 落游标是**幂等键**：不落就会被每轮轮询反复驱动（旧版本在失败分支不落，导致同一份计划
/// 每 30s 重跑一次 gops）。回报失败只告警 —— 不能因为回执不通而让流程停摆。
async fn finish_plan(
    config: &Config,
    client: &CenterClient,
    plan: &wist_control::GatewayUpgradePlan,
    status: &str,
    detail: String,
    log_dir: Option<String>,
    task_id: Option<String>,
) {
    let plan_id = plan.plan_id.clone().unwrap_or_else(|| "plan".to_string());
    let to_version = plan.to_version.clone().unwrap_or_default();
    let record = UpgradeRecord {
        work_id: plan_id.clone(),
        from_version: "unknown".to_string(),
        to_version: to_version.clone(),
        step: "fetch".to_string(),
        status: status.to_string(),
        detail,
        log_dir,
        task_id,
    };
    if let Err(err) = client
        .report_upgrade_result(&config.gateway_id, &record)
        .await
    {
        log::warn!(
            "event=PlanResultReportFailed plan_id={plan_id} status={status} error={}",
            chain_one_line(&err)
        );
    }
    if let Err(err) = state::save_upgrade_cursor(
        &config.state_dir,
        &UpgradeCursor {
            last_plan_id: Some(plan_id),
            last_to_version: to_version,
        },
    ) {
        log::warn!("event=CursorSaveFailed error={}", chain_one_line(&err));
    }
}

/// 驱动一次「Agent 包下发」（发布 ②）：把中心派下的 agentd 包**环回**推进同机网关的包管理，再回报中心。
///
/// 与 ① 不同：**不重建网关** —— 只把包交给网关，升不升由网关决定。与 ① 同款「一份计划只驱一次」：
/// 不论成败都落游标（成败经回执让中心计划条目可见）；需要重来由管理面重派计划。
/// 见设计 `edge/agent-package-push-to-gateways.md`。
async fn drive_agent_package_push(
    config: &Config,
    client: &CenterClient,
    pusher: &AgentPackageClient,
    plan: &wist_control::GatewayUpgradePlan,
) {
    let plan_id = plan.plan_id.clone().unwrap_or_else(|| "plan".to_string());
    let to_version = plan.to_version.clone().unwrap_or_default();
    let task = TaskLog::create(
        &config.state_dir,
        "push-agent-package",
        &plan_id,
        serde_json::json!({
            "to_version": to_version,
            "artifact_url": plan.artifact_url,
            "artifact_sha256": plan.artifact_sha256,
            "action": plan.action,
        }),
    );
    if let Some(task) = &task {
        task.event("run.start", None);
    }
    let (status, mut detail) = match target_artifacts(plan) {
        Ok(targets) => match deliver_agent_package(config, client, pusher, &targets).await {
            Ok(()) => {
                let platforms = targets
                    .iter()
                    .map(|(platform, _, _)| platform.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                log::info!(
                    "event=AgentPackagePushed plan_id={plan_id} platforms={platforms} to_version={to_version}"
                );
                ("done", String::new())
            }
            Err(err) => {
                log::error!(
                    "event=AgentPackagePushFailed plan_id={plan_id} error={}",
                    chain_one_line(&err)
                );
                ("failed", err.to_string())
            }
        },
        Err(message) => ("failed", message.to_string()),
    };
    let finished = finish_task_log(&task, status, &detail);
    let (log_dir, task_id) = match &finished {
        Some((dir, id)) => {
            detail = append_log_dir(detail, dir);
            (Some(dir.clone()), Some(id.clone()))
        }
        None => (None, None),
    };
    // 回执中心：回填发布计划条目（状态折算在中心侧；`done → succeeded`）。
    finish_plan(config, client, plan, status, detail, log_dir, task_id).await;
}

/// 收尾任务日志并返回 `(日志目录, 任务 ID)`（`None` = 没有任务日志）。
fn finish_task_log(
    task: &Option<std::sync::Arc<TaskLog>>,
    status: &str,
    detail: &str,
) -> Option<(String, String)> {
    let task = task.as_ref()?;
    if !detail.is_empty() {
        task.output(&format!("{detail}\n"));
    }
    Some((task.finish(status, detail), task.task_id().to_string()))
}

/// 把任务日志目录拼进回执说明（P0 借 `detail` 承载）。
fn append_log_dir(detail: String, dir: &str) -> String {
    if detail.is_empty() {
        format!("日志: {dir}")
    } else {
        format!("{detail}；日志: {dir}")
    }
}

/// 驱动一次「知识库包下发」：把中心派下的 `wist-knowledge` 包**环回**推进网关知识库管理，再回报中心。
///
/// 与 agent 包同款「一份计划只驱一次」：不论成败都落游标（成败经回执让中心计划条目可见）。
/// 区别：知识库是**内容** —— 交付后不重建网关；`activate` 决定网关是否立刻切生效指针。
async fn drive_knowledge_package_push(
    config: &Config,
    client: &CenterClient,
    pusher: &KnowledgePackageClient,
    plan: &wist_control::GatewayUpgradePlan,
) {
    let plan_id = plan.plan_id.clone().unwrap_or_else(|| "plan".to_string());
    let to_version = plan.to_version.clone().unwrap_or_default();
    let activate = plan.activate.unwrap_or(false);
    let task = TaskLog::create(
        &config.state_dir,
        "push-knowledge-package",
        &plan_id,
        serde_json::json!({
            "to_version": to_version,
            "artifact_url": plan.artifact_url,
            "artifact_sha256": plan.artifact_sha256,
            "activate": activate,
        }),
    );
    if let Some(task) = &task {
        task.event("run.start", None);
    }
    let (status, mut detail) = match deliver_knowledge_package(
        config, client, pusher, plan, activate,
    )
    .await
    {
        Ok(()) => {
            log::info!(
                "event=KnowledgePackagePushed plan_id={plan_id} to_version={to_version} activate={activate}"
            );
            ("done", String::new())
        }
        Err(err) => {
            log::error!(
                "event=KnowledgePackagePushFailed plan_id={plan_id} error={}",
                chain_one_line(&err)
            );
            ("failed", err.to_string())
        }
    };
    let finished = finish_task_log(&task, status, &detail);
    let (log_dir, task_id) = match &finished {
        Some((dir, id)) => {
            detail = append_log_dir(detail, dir);
            (Some(dir.clone()), Some(id.clone()))
        }
        None => (None, None),
    };
    finish_plan(config, client, plan, status, detail, log_dir, task_id).await;
}

/// 取知识库包（gwlinkd 持中心信任）→ 落到投放目录（含 `.sig` 旁文件）→ 环回交付网关。
///
/// 签名旁文件**best-effort**：取得到就一起落地（配了验签公钥的网关会用它）；取不到不阻断 ——
/// 未配公钥的网关本来就只校验摘要。取件与校验口径同 agent 包：gwlinkd 取、网关只托管。
async fn deliver_knowledge_package(
    config: &Config,
    client: &CenterClient,
    pusher: &KnowledgePackageClient,
    plan: &wist_control::GatewayUpgradePlan,
    activate: bool,
) -> GwlinkdResult<()> {
    let artifact_url = plan.artifact_url.clone().ok_or_else(|| {
        GwlinkdReason::KnowledgePackage.err("中心未派生制品地址（该版本未发布？）")
    })?;
    let artifact_sha256 = plan.artifact_sha256.clone().ok_or_else(|| {
        GwlinkdReason::KnowledgePackage.err("中心未带制品摘要（artifact_sha256）")
    })?;
    let drop_dir = config
        .agent_package_drop_dir
        .as_deref()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            GwlinkdReason::KnowledgePackage.err("未配置 agent_package_drop_dir（投放目录）")
        })?;
    let container_dir = config
        .agent_package_container_dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| {
            GwlinkdReason::KnowledgePackage
                .err("未配置 agent_package_container_dir（网关容器路径前缀）")
        })?;
    // 网关只接受 `https://` 或**容器内绝对路径**；相对路径会被直接拒（且报错点在网关，不好定位）。
    if !container_dir.starts_with('/') {
        return Err(GwlinkdReason::KnowledgePackage.err(format!(
            "agent_package_container_dir 必须是网关容器内的绝对路径（当前：{container_dir}）"
        )));
    }

    // 取包与交付：失败也要走到下面的清旧（投放目录是**中转区**，不能因反复失败而无界增长）。
    let outcome: GwlinkdResult<()> = async {
        // 取包：gwlinkd 的制品客户端（带 CA-S / 客户端证书）。
        let bytes = wist_artifact::source::read_source_with_client(
            &client.artifact_http_client(),
            &artifact_url,
            wist_artifact::source::MAX_ARTIFACT_BYTES,
            wist_artifact::source::FETCH_TIMEOUT,
        )
        .await
        .map_err(|err| {
            GwlinkdReason::KnowledgePackage.err(format!("取包失败 {artifact_url}: {err}"))
        })?;

        // 摘要校验：不符即拒，绝不把错内容交付网关。
        let expected = wist_artifact::digest::parse_digest(&artifact_sha256).map_err(|err| {
            GwlinkdReason::KnowledgePackage
                .err(format!("中心给的摘要形态不对（{artifact_sha256}）：{err}"))
        })?;
        let actual = wist_artifact::digest::sha256_hex_bytes(&bytes);
        if actual != expected {
            return Err(GwlinkdReason::KnowledgePackage
                .err(format!("制品摘要不符：期望 {expected}，实得 {actual}")));
        }

        let filename = safe_artifact_filename_with(&artifact_url, GwlinkdReason::KnowledgePackage)?;
        write_drop_file(drop_dir, &filename, bytes).await?;

        // 签名旁文件：`<来源>.sig`（发布侧同名）。取不到不阻断，只记日志。
        let signature_url = format!("{artifact_url}.sig");
        match wist_artifact::source::read_source_with_client(
            &client.artifact_http_client(),
            &signature_url,
            wist_artifact::source::MAX_ARTIFACT_BYTES,
            wist_artifact::source::FETCH_TIMEOUT,
        )
        .await
        {
            Ok(signature) => {
                write_drop_file(drop_dir, &format!("{filename}.sig"), signature).await?;
            }
            // 已存在的旧 `.sig` 必须删掉：否则网关会拿上一版的签名去验这一版
            // （验签失败的报错会指向签名，而不是「这次没取到签名」）。
            Err(_) => {
                log::warn!("event=KnowledgePackageSignatureUnavailable url={signature_url}");
                let stale = drop_dir.join(format!("{filename}.sig"));
                let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(stale)).await;
            }
        }

        let container_path = format!("{}/{}", container_dir.trim_end_matches('/'), filename);
        pusher
            .push(&container_path, &artifact_sha256, &artifact_url, activate)
            .await?;
        Ok(())
    }
    .await;

    // 投放目录是交付中转区：按数量清旧（保留数不低于本批目标数；失败也清）。
    let keep = match config.agent_package_drop_keep {
        Some(0) => Some(0),
        Some(keep) => Some(keep.max(2)),
        None => Some(DEFAULT_AGENT_PACKAGE_DROP_KEEP.max(2)),
    };
    prune_old_drops(drop_dir, keep).await;
    outcome
}

/// 把一个交付文件落到投放目录：先写 `<name>.partial` 再改名，避免半截文件被网关读到。
async fn write_drop_file(dir: &Path, filename: &str, bytes: Vec<u8>) -> GwlinkdResult<()> {
    let write_dir = dir.to_path_buf();
    let target = dir.join(filename);
    tokio::task::spawn_blocking(move || -> GwlinkdResult<()> {
        std::fs::create_dir_all(&write_dir).source_err(
            GwlinkdReason::KnowledgePackage,
            format!("建投放目录失败 {}", write_dir.display()),
        )?;
        let partial = partial_path(&target);
        std::fs::write(&partial, &bytes).source_err(
            GwlinkdReason::KnowledgePackage,
            format!("写投放文件失败 {}", partial.display()),
        )?;
        std::fs::rename(&partial, &target).source_err(
            GwlinkdReason::KnowledgePackage,
            format!("落盘投放文件失败 {}", target.display()),
        )
    })
    .await
    .map_err(|err| GwlinkdReason::KnowledgePackage.err(format!("投放任务异常: {err}")))??;
    Ok(())
}

/// 目标平台集合（发布 ②）：优先契约的 `artifacts`（**多平台**，中心给该版本全部平台）；
/// 为空时回落到单值 `artifact_url` + 摘要 + **本机平台**（旧中心 / ① 语义，兼容）。
/// 三样缺一不可，缺则报出可读原因。
fn target_artifacts(
    plan: &wist_control::GatewayUpgradePlan,
) -> GwlinkdResult<Vec<(String, String, String)>> {
    if !plan.artifacts.is_empty() {
        return Ok(plan
            .artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact.platform.clone(),
                    artifact.artifact_url.clone(),
                    artifact.artifact_sha256.clone(),
                )
            })
            .collect());
    }
    let artifact_url = plan
        .artifact_url
        .clone()
        .ok_or_else(|| GwlinkdReason::AgentPackage.err("中心未派生制品地址（该版本未发布？）"))?;
    let artifact_sha256 = plan
        .artifact_sha256
        .clone()
        .ok_or_else(|| GwlinkdReason::AgentPackage.err("中心未带制品摘要（artifact_sha256）"))?;
    let platform = wist_gwlinkd::target::HostTarget::detect()
        .target_triple()
        .ok_or_else(|| GwlinkdReason::AgentPackage.err("认不出本机平台（target-triple）"))?;
    Ok(vec![(platform, artifact_url, artifact_sha256)])
}

/// 取包（gwlinkd 持中心信任）→ 落到投放目录 → **一次**环回交付网关托管（多平台）。
///
/// **取包由 gwlinkd 完成**：用 [`CenterClient::artifact_http_client`]（带 CA-S / 客户端证书，
/// 「自签中心也能拉」）逐个平台拉 `artifact_url` → 校验 `artifact_sha256` → 落成投放目录里的本机文件；
/// 再把**全部平台**一次 POST 给网关（网关侧整批一次提交，任一不合格整体拒绝落库）。分层见 `edge/center-content-delivery.md`。
///
/// **清理（无论成败）**：最后按数量清旧（best-effort）—— 失败也清，免得反复失败把投放目录撑爆；
/// 保留数**不低于本批目标数**，绝不误清刚落的文件。
async fn deliver_agent_package(
    config: &Config,
    client: &CenterClient,
    pusher: &AgentPackageClient,
    targets: &[(String, String, String)],
) -> GwlinkdResult<()> {
    let drop_dir = config
        .agent_package_drop_dir
        .as_deref()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            GwlinkdReason::AgentPackage.err("未配置 agent_package_drop_dir（② 投放目录）")
        })?;
    let container_dir = config
        .agent_package_container_dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| {
            GwlinkdReason::AgentPackage
                .err("未配置 agent_package_container_dir（网关容器路径前缀）")
        })?;
    // 网关只接受 `https://` 或**容器内绝对路径**；相对路径会被直接拒（且报错点在网关，不好定位）。
    if !container_dir.starts_with('/') {
        return Err(GwlinkdReason::AgentPackage.err(format!(
            "agent_package_container_dir 必须是网关容器内的绝对路径（当前：{container_dir}）"
        )));
    }

    let outcome = async {
        // 逐平台：取包 → 校验 → 落盘 → 记下（本机路径，容器可见）。
        let mut delivered: Vec<(String, String, String, String)> =
            Vec::with_capacity(targets.len());
        for (platform, artifact_url, artifact_sha256) in targets {
            // 取包：gwlinkd 的制品客户端（带 CA-S / 客户端证书）。
            let bytes = wist_artifact::source::read_source_with_client(
                &client.artifact_http_client(),
                artifact_url,
                wist_artifact::source::MAX_ARTIFACT_BYTES,
                wist_artifact::source::FETCH_TIMEOUT,
            )
            .await
            .map_err(|err| {
                GwlinkdReason::AgentPackage
                    .err(format!("取包失败 {platform} {artifact_url}: {err}"))
            })?;

            // 摘要校验：不符即拒，绝不把错内容交付网关。
            let expected = wist_artifact::digest::parse_digest(artifact_sha256).map_err(|err| {
                GwlinkdReason::AgentPackage.err(format!(
                    "中心给的摘要形态不对（{platform} {artifact_sha256}）：{err}"
                ))
            })?;
            let actual = wist_artifact::digest::sha256_hex_bytes(&bytes);
            if actual != expected {
                return Err(GwlinkdReason::AgentPackage.err(format!(
                    "制品摘要不符（{platform}）：期望 {expected}，实得 {actual}"
                )));
            }

            // 安全文件名（防 `..` / 控制字符把落点带出投放目录）+ **平台限定**（同版本多平台可能同名制品）。
            let filename = platform_drop_filename(platform, artifact_url)?;
            // 落到宿主投放目录（网关容器只读挂载同一份）：先写临时再改名，避免半截文件被读。
            let drop_path = drop_dir.join(&filename);
            let write_dir = drop_dir.to_path_buf();
            let write_target = drop_path.clone();
            tokio::task::spawn_blocking(move || -> GwlinkdResult<()> {
                std::fs::create_dir_all(&write_dir).source_err(
                    GwlinkdReason::AgentPackage,
                    format!("建投放目录失败 {}", write_dir.display()),
                )?;
                let partial = partial_path(&write_target);
                std::fs::write(&partial, &bytes).source_err(
                    GwlinkdReason::AgentPackage,
                    format!("写投放文件失败 {}", partial.display()),
                )?;
                std::fs::rename(&partial, &write_target).source_err(
                    GwlinkdReason::AgentPackage,
                    format!("落盘投放文件失败 {}", write_target.display()),
                )
            })
            .await
            .map_err(|err| GwlinkdReason::AgentPackage.err(format!("投放任务异常: {err}")))??;

            let container_path = format!("{}/{}", container_dir.trim_end_matches('/'), filename);
            delivered.push((
                platform.clone(),
                container_path,
                artifact_url.clone(),
                artifact_sha256.clone(),
            ));
        }

        // 一次 POST 带全部平台 —— 网关侧整批一次提交（任一不合格整体拒绝落库）。
        let items: Vec<AgentPackageItem<'_>> = delivered
            .iter()
            .map(
                |(platform, package_url, origin, package_sha256)| AgentPackageItem {
                    platform,
                    package_url,
                    origin,
                    package_sha256,
                },
            )
            .collect();
        pusher.push(&items).await?;
        Ok::<(), GwlinkdError>(())
    }
    .await;

    // 无论成败都按数量清旧（best-effort）：失败也清（免得反复失败把投放目录撑爆）；
    // 保留数**不低于本批目标数** —— 否则 keep 偏小时可能把刚落的文件清掉。
    let keep = match config.agent_package_drop_keep {
        Some(0) => Some(0), // 0 = 不清理
        Some(keep) => Some(keep.max(targets.len())),
        None => Some(DEFAULT_AGENT_PACKAGE_DROP_KEEP.max(targets.len())),
    };
    prune_old_drops(drop_dir, keep).await;
    outcome
}

/// 投放目录保留份数的缺省值（清理时按 mtime 保留最新 N 份）。
const DEFAULT_AGENT_PACKAGE_DROP_KEEP: usize = 12;

/// 按数量清旧：保留投放目录里**最新** `keep` 份，删其余。`0` = 不清理。
/// **best-effort**：失败只告警 —— 清理不该让一次交付变成失败。
async fn prune_old_drops(drop_dir: &Path, keep: Option<usize>) {
    let keep = keep.unwrap_or(DEFAULT_AGENT_PACKAGE_DROP_KEEP);
    if keep == 0 {
        return;
    }
    let dir = drop_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || prune_drop_dir(&dir, keep)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => log::warn!(
            "event=AgentPackageDropPruneFailed error={}",
            chain_one_line(&err)
        ),
        Err(err) => log::warn!(
            "event=AgentPackageDropPruneFailed error={}",
            chain_one_line(&err)
        ),
    }
}

/// 按 mtime 保留**最新** `keep` 个普通文件，删其余（子目录不动）。单一文件删除失败只告警。
fn prune_drop_dir(dir: &Path, keep: usize) -> GwlinkdResult<()> {
    let mut entries: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .source_err(
            GwlinkdReason::AgentPackage,
            format!("读投放目录失败 {}", dir.display()),
        )?
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push((modified, path));
    }
    if entries.len() <= keep {
        return Ok(());
    }
    entries.sort_by_key(|(modified, _)| *modified);
    let drop_count = entries.len() - keep;
    for (_, path) in entries.into_iter().take(drop_count) {
        if let Err(err) = std::fs::remove_file(&path) {
            log::warn!(
                "event=AgentPackageDropPruneSkip path={} error={}",
                path.display(),
                chain_one_line(&err)
            );
        }
    }
    Ok(())
}

/// 从制品地址取一个**安全的单段文件名**：去 query、取末段；拒空 / `.` / `..` / 含路径分隔或控制字符。
///
/// 该名字拼进投放目录后的**本机路径**会被交给网关取包，绝不能让 `..` 或分隔符把落点带出目录。
fn safe_artifact_filename(source: &str) -> GwlinkdResult<String> {
    safe_artifact_filename_with(source, GwlinkdReason::AgentPackage)
}

/// 同 [`safe_artifact_filename`]，但错误归到调用方的 reason（agent 包 / 知识库包各自的错误身份）。
fn safe_artifact_filename_with(source: &str, reason: GwlinkdReason) -> GwlinkdResult<String> {
    let name = source
        .split('?')
        .next()
        .unwrap_or(source)
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." {
        return Err(reason.err(format!("从制品地址取不出安全文件名：{source}")));
    }
    if name.chars().any(|ch| ch == '\\' || ch.is_control()) {
        return Err(reason.err(format!("制品文件名含非法字符：{name}")));
    }
    Ok(name.to_string())
}

/// 投放文件名 = `<平台>__<制品原名>`。
///
/// 同一版本的**多平台**制品，来源原名**可能相同**（用户上传时同名）—— 落在同一投放目录会互相覆盖：
/// 后一个平台把前一个的文件改名覆盖掉，交付里两个平台指向**同一份内容**，网关摘要校验必有一方不过，
/// 整批被拒（fail-closed，但原因难定位）。前缀平台即可保证「一平台一份」。
///
/// 网关取包只按**内容**认版本/架构、按报文里的 `platform` 认平台（不看文件名），故改名安全。
fn platform_drop_filename(platform: &str, artifact_url: &str) -> GwlinkdResult<String> {
    let platform = safe_path_segment(platform, "制品平台")?;
    let name = safe_artifact_filename(artifact_url)?;
    Ok(format!("{platform}__{name}"))
}

/// 校验一个可拼进本机路径的**安全单段**：非空、非 `.` / `..`、不含路径分隔符或控制字符。
fn safe_path_segment(value: &str, what: &str) -> GwlinkdResult<String> {
    if value.is_empty() || value == "." || value == ".." {
        return Err(GwlinkdReason::AgentPackage.err(format!("{what}为空或不安全：{value:?}")));
    }
    if value.contains('/') || value.chars().any(|ch| ch == '\\' || ch.is_control()) {
        return Err(GwlinkdReason::AgentPackage.err(format!("{what}含非法字符：{value}")));
    }
    Ok(value.to_string())
}

/// 落盘用的临时名：**追加** `.partial`（不用 `with_extension` —— 它会把 `.gz` 换成 `.partial`，
/// 不同扩展名的同名制品会撞到同一个临时文件）。
fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

/// 指数退避：0 → 初值，否则翻倍到上限。
fn back_off(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_secs(BACKOFF_BASE_SECS)
    } else {
        (current * 2).min(Duration::from_secs(BACKOFF_MAX_SECS))
    }
}

/// 配置里的接入券（`gwlinkd.toml` `link_token`）：**空白视为未配置**（`None`）。
fn link_token_from_config(config: &Config) -> Option<String> {
    config
        .link_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// 读取一次性接入券：优先 `WIST_GWLINKD_LINK_TOKEN`，回退旧名
/// `WIST_GWLINKD_BOOTSTRAP_TOKEN`（弃用告警，下一版移除）。
fn link_token_from_env() -> Option<String> {
    if let Ok(token) = std::env::var("WIST_GWLINKD_LINK_TOKEN") {
        return Some(token);
    }
    match std::env::var("WIST_GWLINKD_BOOTSTRAP_TOKEN") {
        Ok(token) => {
            log::warn!(
                "warning: WIST_GWLINKD_BOOTSTRAP_TOKEN 已更名为 WIST_GWLINKD_LINK_TOKEN，请更新（旧名下一版移除）"
            );
            Some(token)
        }
        Err(_) => None,
    }
}

/// 接入成功后把**已消费**的一次性接入券从现场态配置里删掉。
///
/// 券是一次性的（中心 `consume_link_token` 成功即清 hash，另有短 TTL）：留着它只是把一张
/// 废券明文搁在盘上（`diagnose` 会报「已接入却仍残留 link_token」）。失败**不阻断**接入结果
/// —— 只是留下一张废券，记一笔告警。
fn clear_consumed_link_token(pub_path: &Path) {
    match remove_link_token(pub_path) {
        Ok(true) => log::info!("event=LinkTokenCleared path={}", pub_path.display()),
        Ok(false) => {}
        Err(err) => log::warn!(
            "event=LinkTokenClearFailed path={} error={}",
            pub_path.display(),
            chain_one_line(&err)
        ),
    }
}

/// 首跑：确保拿到长期身份（可能等待页面提交的接入请求）。
async fn first_run(
    path: &Path,
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
) -> GwlinkdResult<()> {
    // 接入券来源（按序）：配置 `link_token`（gwlinkd.toml）→ 环境变量（兼容 dev / 旧路径）。
    // 有券即**直接接入**（不再经网关页面）。「券进配置」= 写好 gwlinkd.toml、起进程就自联上。
    let link_token = link_token_from_config(config).or_else(link_token_from_env);
    if let Some(token) = link_token.as_deref() {
        let link_client = CenterClient::with_client(
            config.control_center_endpoint.clone(),
            center::build_http_client(trust)?,
        );
        let outcome = onboard(&link_client, config, identity, Some(token)).await;
        if outcome.is_ok() {
            // 券（若写在场态配置里）已被消费 —— 删掉，别把废券留在盘上（§6）。
            clear_consumed_link_token(&pub_config_path(path));
        }
        return outcome;
    }
    // 无券：页面发起（环回接入请求）—— 拉取后在**这一步改写 gwlinkd.toml**。
    if let Some(base) = config.gateway_self_endpoint.as_deref() {
        let client = LinkRequestClient::with_trust(base, config.gateway_self_ca.as_deref())?;
        return wait_for_gateway_request(path, config, trust, identity, &client).await;
    }
    // 都没配：无券直跑 onboard，由其给出清晰错误。
    let link_client = CenterClient::with_client(
        config.control_center_endpoint.clone(),
        center::build_http_client(trust)?,
    );
    onboard(&link_client, config, identity, None).await
}

/// 一次「从环回接入请求接入」：有待办则接入并回报结果，成功返回 `Ok(true)`；无待办 `Ok(false)`。
async fn onboard_from_gateway(
    path: &Path,
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
    client: &LinkRequestClient,
) -> GwlinkdResult<bool> {
    let request = client.fetch(&config.gateway_id).await?;
    if !request.has_request {
        return Ok(false);
    }
    // 页面带的 CA 落盘（免得还要先手工预置信任锚）——落在**身份目录**：它是信任锚，现场事实。
    let trust_path = if request.trust_bundle_pem.trim().is_empty() {
        None
    } else {
        Some(state::save_trust_bundle(
            config.identity_dir(),
            &request.trust_bundle_pem,
        )?)
    };
    // **在接入这一步改写现场态配置**（`gwlinkd-pub.toml`）—— 链接关系的持久记录（不走 DB）。
    // 写现场态而非主配置：主配置是渲染物，重渲会把它抹掉；且这两类键本来就不该混在一个文件里。
    let pub_path = pub_config_path(path);
    upsert_link_settings(
        &pub_path,
        &request.center_endpoint,
        &request.gateway_id,
        &request.link_token,
        trust_path.as_deref(),
    )
    .conv_err()?;
    log::info!(
        "event=LinkRequestPersisted path={} center={}",
        pub_path.display(),
        request.center_endpoint
    );
    // 以改写后的配置为准（endpoint / gateway_id / trust 都从文件读回）。
    let updated = Config::load(path).conv_err()?;
    let effective_trust = trust_path.as_deref().or(trust);
    let link_client = CenterClient::with_client(
        updated.control_center_endpoint.clone(),
        center::build_http_client(effective_trust)?,
    );
    log::info!(
        "event=LinkRequestPicked gateway_id={} center={}",
        updated.gateway_id,
        updated.control_center_endpoint
    );
    match onboard(&link_client, &updated, identity, Some(&request.link_token)).await {
        Ok(()) => {
            // 券已被中心消费（一次性）——从现场态配置里删掉，别把废券留在盘上（§6）。
            clear_consumed_link_token(&pub_path);
            let _ = client
                .report_result(&updated.gateway_id, "Connected", "")
                .await;
            Ok(true)
        }
        Err(err) => {
            let _ = client
                .report_result(&updated.gateway_id, "Failed", &err.to_string())
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
    path: &Path,
    config: &Config,
    trust: Option<&Path>,
    identity: &str,
    client: &LinkRequestClient,
) -> GwlinkdResult<()> {
    log::info!(
        "event=WaitingLinkRequest gateway_id={}（等待页面「链接上级」提交接入请求）",
        config.gateway_id
    );
    // 接入前也把依赖工具版本带上：页面在「等待接入」状态下同样能看到宿主工具是否就绪。
    let mut tool_versions = ToolVersions::detect(config.upgrader_program.as_deref()).await;
    let mut next_tool_probe_at = Instant::now() + Duration::from_secs(TOOL_VERSION_REFRESH_SECS);
    loop {
        // 等待接入可能持续很久（人去装 gops / 点页面）：定期刷新，不把启动时的快照一直挂到接入完成。
        if Instant::now() >= next_tool_probe_at {
            tool_versions = ToolVersions::detect(config.upgrader_program.as_deref()).await;
            next_tool_probe_at = Instant::now() + Duration::from_secs(TOOL_VERSION_REFRESH_SECS);
        }
        match retry_leftover_registration(config, trust, identity).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => log::warn!("event=RegisterRetryFailed error={}", chain_one_line(&err)),
        }
        match onboard_from_gateway(path, config, trust, identity, client).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(err) => log::warn!("event=LinkRequestFailed error={}", chain_one_line(&err)),
        }
        // 心率：接入前也推一条「在跑、等待接入」——免得页面把「还没接」当成「gwlinkd 没跑」。
        let waiting = GwlinkdStatus {
            gateway_id: config.gateway_id.clone(),
            instance_id: state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)
                .unwrap_or_default(),
            version: wist_gwlinkd::VERSION.to_string(),
            gops_version: tool_versions.gops.clone(),
            gx_version: tool_versions.gx.clone(),
            center_endpoint: config.control_center_endpoint.clone(),
            state: STATE_WAITING_LINK_REQUEST.to_string(),
            credential_expires_at: None,
            last_center_report_at: None,
            last_error: None,
            reported_at: DateTime::now(),
        };
        if let Err(err) = client.report_linkd_status(&waiting).await {
            log::warn!("event=LinkdStatusPushFailed error={}", chain_one_line(&err));
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
) -> GwlinkdResult<bool> {
    if state::load_regist_token(&config.state_dir).is_none() {
        return Ok(false);
    }
    let endpoint = state::load_link_config(&config.state_dir)
        .map(|saved| saved.control_center_endpoint)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.control_center_endpoint.clone());
    let saved_ca = config.identity_dir().join(state::TRUST_BUNDLE_FILE);
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
) -> GwlinkdResult<()> {
    let instance_id = state::load_or_create_instance_id(&config.state_dir, &config.gateway_id)?;
    // 网关**对外域名**：注册时尽力从自述面取（取不到就 None，不阻断注册）。
    let public_base_url = fetch_gateway_public_base_url(config).await;

    // 复用上次未消费的 RegistToken（link-upstream 已成功、register 未成的遗留）：直接重试注册。
    if let Some(regist_token) = state::load_regist_token(&config.state_dir) {
        log::info!("event=RegisterRetry gateway_id={}", config.gateway_id);
        match register_once(
            client,
            config,
            &regist_token,
            &instance_id,
            public_base_url.as_deref(),
        )
        .await
        {
            Ok(()) => {
                state::clear_regist_token(&config.state_dir);
                return Ok(());
            }
            Err(err) if err.is_unauthorized() => {
                // token 已失效/已被消费：丢弃，走完整首跑（需接入券）。
                log::warn!("event=RegistTokenStale 清掉遗留 token，重走首跑");
                state::clear_regist_token(&config.state_dir);
            }
            // 网络类错误：保留 token，下次再试。
            Err(err) => return Err(err.into()),
        }
    }

    let link = link.ok_or_else(|| {
        GwlinkdReason::Enrollment.err(
            "首跑需要接入券：在 gwlinkd.toml 配 `link_token`（或设 WIST_GWLINKD_LINK_TOKEN）\
             —— 中心 admin 创建实例时签发的接入 token；\
             若本机曾有身份，请检查 state/credential.json 是否损坏",
        )
    })?;

    log::info!("event=LinkUpstream gateway_id={}", config.gateway_id);
    let returned = logged_op(
        module_path!(),
        "center link-upstream",
        &[("gateway_id", config.gateway_id.clone())],
        client
            .link_upstream(&config.gateway_id, link, Some(identity))
            .await,
    )
    .map_err(|err| {
            // 401 `certificate_required`：link-upstream 对**已初始化**的 gateway_id 只认 mTLS（不再置备），
            // 而本地已无客户端证书（典型：刚 unlink）—— 这不是「券不对」，是「这台已不能再置备」。
            // **不给「重置」的口子**（有意）：唯一可行是换**新 gateway_id**。别把裸 code 抛给运维。
            let detail = if err.is_unauthorized() && err.detail_contains("certificate_required") {
                format!(
                    "中心认为该 gateway_id 已初始化（link-upstream 走 mTLS、不再置备），但本地无客户端证书 \
                     —— 无法自动恢复。同一 gateway_id 不能重接（本仓无「重置实例」，且是有意的安全边界）：\
                     请在中心**新建一个实例**、用它的**新 gateway_id** 再接入。原始错误：{err}"
                )
            } else {
                err.to_string()
            };
            GwlinkdReason::Enrollment.err(detail)
        })?;
    // 链接配置（信任锚 / 协议版本 / 注册 token 引用）落盘留痕 —— 不再丢弃。
    state::save_link_config(&config.state_dir, &returned.config)?;
    let regist_token = returned.regist_token.ok_or_else(|| {
        GwlinkdReason::Enrollment.err(
            "中心认为该网关**已初始化**，但本地无客户端证书 —— 无法自动恢复（CR-003 尚缺「身份重置」路径）。\
             请在中心重置该实例后重跑，或把既有的客户端证书/私钥写入 state/credential.json",
        )
    })?;
    // **先落盘再注册**：接入券已消费，注册失败也要能靠这个 token 重试。
    state::save_regist_token(&config.state_dir, &regist_token)?;

    register_once(
        client,
        config,
        &regist_token,
        &instance_id,
        public_base_url.as_deref(),
    )
    .await?;
    state::clear_regist_token(&config.state_dir);
    Ok(())
}

/// 尽力取网关**对外域名**（自述面 `public_base_url`）：注册时网关可能还没起来 / 未配
/// `gateway_self_endpoint` —— 取不到就 `None`，不阻断注册（中心容忍缺该字段）；随后的周期
/// 状态上报会自然补上。
async fn fetch_gateway_public_base_url(config: &Config) -> Option<String> {
    let base = config.gateway_self_endpoint.as_deref()?;
    let client = SelfReportClient::with_trust(base, config.gateway_self_ca.as_deref()).ok()?;
    client
        .fetch(&config.gateway_id)
        .await
        .ok()
        .and_then(|state| state.public_base_url)
}

/// 用 RegistToken 完成一次注册：当场生成密钥对（私钥不上送，只交 CSR）、落长期身份。
async fn register_once(
    client: &CenterClient,
    config: &Config,
    regist_token: &str,
    instance_id: &str,
    public_base_url: Option<&str>,
) -> Result<(), CenterError> {
    let keypair = identity::generate_client_keypair(&config.gateway_id).map_err(|err| {
        CenterReason::Local.err(format!("生成客户端密钥对失败: {}", err.op_display_chain()))
    })?;
    log::info!("event=Register gateway_id={}", config.gateway_id);
    let result = logged_op(
        module_path!(),
        "center register",
        &[("gateway_id", config.gateway_id.clone())],
        client
            .register(regist_token, instance_id, &keypair.csr_pem, public_base_url)
            .await,
    )?;
    let credential = state::StoredCredential {
        bundle: result.credential_bundle,
        private_key_pem: keypair.private_key_pem,
    };
    state::save_credential(config.identity_dir(), &credential).map_err(|err| {
        CenterReason::Local.err(format!("落长期身份失败: {}", err.op_display_chain()))
    })?;
    log::info!(
        "event=Registered gateway_id={} credential_id={}",
        result.gateway_id,
        result.credential_id
    );
    Ok(())
}

/// 以当前长期身份建 mTLS 客户端（网关面调用用它认人）。
fn mtls_client(
    config: &Config,
    trust: Option<&Path>,
    credential: &state::StoredCredential,
) -> CenterResult<CenterClient> {
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

// ─────────────────────────── unlink（断开与 Center 的连接） ───────────────────

/// `unlink`：断开本机网关与 Center 的连接（见 [`wist_gwlinkd::unlink`]）。
///
/// 需先停掉 gwlinkd（本命令不与运行中的常驻并存）；`--forget-center` 连中心信任锚也删；
/// `--dry-run` 只算不落盘。删完**尽最大努力**把「已断开接入」这一拍告知网关（环回 linkd-status），
/// 让页面立刻别再显示「已接入」。
async fn run_unlink(args: &[String]) -> ExitCode {
    let mut forget_center = false;
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--forget-center" => forget_center = true,
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                print_unlink_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("未知参数：{other}（可用：--forget-center / --dry-run）");
                return ExitCode::from(2);
            }
        }
    }
    let path = config_path();
    let config = match Config::load(&path) {
        Ok(config) => {
            // 已读到配置：日志按它的 `[log]` 段初始化（与 `run` 同口径）。
            log_init(&config.log);
            config
        }
        Err(err) => {
            eprintln!("[FAIL] 配置不可读：{}", err.display_chain());
            return ExitCode::FAILURE;
        }
    };
    match unlink::unlink(&path, &config, forget_center, dry_run) {
        Ok(report) => {
            report_unlink(&report);
            if !dry_run {
                announce_unlinked_to_gateway(&config).await;
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[FAIL] {}", err.op_display_chain());
            ExitCode::FAILURE
        }
    }
}

/// 把「已断开接入」推给网关（环回 linkd-status），让「链接上级」页的**接入状态卡**立刻
/// 从「已接入（降级）」变成「未接入」—— 不再等 90s 失联窗口，也不靠页面自己猜。
///
/// **尽力而为**：没配 `gateway_self_endpoint`、网关没起、或推失败，都只记一行，**不影响 unlink 成功**
/// （本机已断开的事实已经落盘）。报文状态用 `WaitingLinkRequest`（与重启后 gwlinkd 的自然状态一致：
/// 无客户端证书、等页面重新提交接入物），且**不带** `credential_expires_at`（凭据已删）。
async fn announce_unlinked_to_gateway(config: &Config) {
    let Some(base) = config.gateway_self_endpoint.as_deref() else {
        return;
    };
    let client = match LinkRequestClient::with_trust(base, config.gateway_self_ca.as_deref()) {
        Ok(client) => client,
        Err(err) => {
            log::warn!("event=UnlinkAnnounceSkipped error={}", chain_one_line(&err));
            return;
        }
    };
    let tool_versions = ToolVersions::detect(config.upgrader_program.as_deref()).await;
    let status = GwlinkdStatus {
        gateway_id: config.gateway_id.clone(),
        instance_id: String::new(),
        version: wist_gwlinkd::VERSION.to_string(),
        gops_version: tool_versions.gops.clone(),
        gx_version: tool_versions.gx.clone(),
        center_endpoint: config.control_center_endpoint.clone(),
        state: STATE_WAITING_LINK_REQUEST.to_string(),
        credential_expires_at: None,
        last_center_report_at: None,
        last_error: None,
        reported_at: DateTime::now(),
    };
    match client.report_linkd_status(&status).await {
        Ok(()) => println!("已告知网关：本机已断开接入（页面将显示「未接入」）。"),
        Err(err) => log::warn!("event=UnlinkAnnounceFailed error={}", chain_one_line(&err)),
    }
}

fn print_unlink_usage() {
    println!(
        "用法：wist-gwlinkd unlink [--forget-center] [--dry-run]\n\
         断开本机网关与 Center 的连接：删注册态（客户端证书 / 链接配置 / 注册券 / 身份 / 实例 / 升级游标），\n\
         并去掉 gwlinkd.toml 里的 link_token；--forget-center 连中心信任锚（trust_bundle）也删；\n\
         --dry-run 只看将做什么，不落盘。\n\
         需先停掉 gwlinkd（本命令不与运行中的常驻并存）。\n\
         配置路径：WIST_GWLINKD_CONFIG（缺省 {}）。",
        wist_gwlinkd::DEFAULT_CONFIG_PATH
    );
}

fn report_unlink(report: &UnlinkReport) {
    let head = if report.dry_run {
        "【DRY_RUN】将断开"
    } else {
        "已断开"
    };
    let done = if report.dry_run { "将删" } else { "已删" };
    println!(
        "{head}本机网关与 Center 的连接（state={}）：",
        report.state_dir.display()
    );
    if report.removed_state_files.is_empty() {
        println!("  注册态        本就没有");
    } else {
        println!(
            "  注册态        {done} {} 项：{}",
            report.removed_state_files.len(),
            report.removed_state_files.join(", ")
        );
    }
    println!(
        "  link_token    {}",
        match (report.link_token_removed, report.dry_run) {
            (true, true) => "将从配置去掉",
            (true, false) => "已从配置去掉",
            (false, _) => "本就为空",
        }
    );
    if !report.removed_anchors.is_empty() {
        let paths = report
            .removed_anchors
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("、");
        println!("  信任锚        {done} {paths}");
    } else if report.trust_bundle.exists() {
        println!("  信任锚        保留 {}", report.trust_bundle.display());
    } else {
        println!(
            "  信任锚        本就没有（{}）",
            report.trust_bundle.display()
        );
    }
    if report.dry_run {
        println!("（DRY_RUN：以上改动都没有落盘）");
    }
}

// ─────────────────────────── service（OS 服务管理器托管） ───────────────────────

fn run_service(args: &[String]) -> ExitCode {
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!(
            "用法：wist-gwlinkd service <print|install|uninstall|status> [--system|--user] [--bin PATH] [--config PATH] [--pub-config PATH] [--run-as USER] [--run-as-group GROUP] [--force] [--no-activate]"
        );
        return ExitCode::from(2);
    };
    match service_command(action, &args[1..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[FAIL] {}", err.op_display_chain());
            ExitCode::FAILURE
        }
    }
}

fn service_command(action: &str, rest: &[String]) -> GwlinkdResult<()> {
    let platform = service::ServicePlatform::current()
        .ok_or_else(|| GwlinkdReason::Service.err("当前平台非 Linux/macOS，不支持 service 托管"))?;

    let mut scope: Option<service::ServiceScope> = None;
    let mut bin: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut pub_config: Option<PathBuf> = None;
    let mut run_as: Option<String> = None;
    let mut run_as_group: Option<String> = None;
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
            "--run-as" => {
                run_as = Some(need_arg(rest, i + 1, "--run-as")?);
                i += 2;
            }
            "--run-as-group" => {
                run_as_group = Some(need_arg(rest, i + 1, "--run-as-group")?);
                i += 2;
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
            "--pub-config" => {
                pub_config = Some(PathBuf::from(need_arg(rest, i + 1, "--pub-config")?));
                i += 2;
            }
            other => return Err(GwlinkdReason::InvalidArgs.err(format!("未知参数：{other}"))),
        }
    }

    // 默认 system（正式运行即系统级常驻）；要用户级就显式 --user。
    let scope = scope.unwrap_or(service::ServiceScope::System);
    // --run-as 只在 system 作用域有意义：systemd `User=` / launchd `UserName` 让系统服务**以非 root 运行**。
    if run_as.is_some() && scope != service::ServiceScope::System {
        return Err(GwlinkdReason::InvalidArgs
            .err("--run-as 只对 --system 作用域有效（--user 本就以本人运行）"));
    }
    if run_as_group.is_some() && run_as.is_none() {
        return Err(GwlinkdReason::InvalidArgs.err("--run-as-group 需要同时给 --run-as"));
    }
    // `service status` 是只读的：`--bin` / `--config` / `--pub-config` 没给时，回落到**已装定义**里
    // 那份，而不是 `/etc/wist-gwlinkd/gwlinkd.toml` 这类作用域默认值 —— 它报的才是「这个服务」的真相。
    // 只落到 `status`：`install` 是写操作，不能拿旧定义当缺省去重写它自己。
    if action == "status"
        && let Some(installed) = service::installed_definition(scope)
    {
        if bin.is_none() {
            bin = installed.paths.bin;
        }
        if config.is_none() {
            config = installed.paths.config_path;
        }
        if pub_config.is_none() {
            pub_config = installed.paths.pub_config;
        }
    }
    let bin = match bin {
        Some(bin) => bin,
        None => service::default_bin()?,
    };
    let config = match config {
        Some(config) => config,
        None => service::default_config_path(scope)?,
    };
    let layout = service::ServiceLayout::resolve(platform, scope)?;
    let spec = service::ServiceSpec::new(scope, bin, config)
        .with_run_as(run_as, run_as_group)
        .with_pub_config(pub_config);

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
                if removed {
                    "已删除"
                } else {
                    "本就不存在"
                }
            );
            Ok(())
        }
        "status" => {
            let status = service::status(&layout, &spec)?;
            println!("platform={}", status.platform.as_str());
            println!("scope={}", status.scope.as_str());
            println!(
                "definition={}",
                service::path_state(&status.definition_path)
            );
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
        other => Err(GwlinkdReason::InvalidArgs.err(format!(
            "未知 service 动作：{other}（可用：print | install | uninstall | status）"
        ))),
    }
}

fn need_arg(args: &[String], index: usize, flag: &str) -> GwlinkdResult<String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| GwlinkdReason::InvalidArgs.err(format!("{flag} 需要一个值")))
}

fn run_service_commands(commands: Vec<service::ServiceCommand>) -> GwlinkdResult<()> {
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
        Err(GwlinkdReason::Service.err(failures.join("\n")))
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

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parse_global_paths_takes_conf_and_pub_and_leaves_the_rest() {
        let (conf, pub_path, rest) = parse_global_paths(&args(&[
            "diagnose",
            "--conf",
            "a/gwlinkd.toml",
            "--pub",
            "b/gwlinkd-pub.toml",
        ]));
        assert_eq!(conf, Some(PathBuf::from("a/gwlinkd.toml")));
        assert_eq!(pub_path, Some(PathBuf::from("b/gwlinkd-pub.toml")));
        assert_eq!(rest, args(&["diagnose"]));
    }

    #[test]
    fn parse_global_paths_accepts_the_inline_equals_form() {
        let (conf, pub_path, rest) =
            parse_global_paths(&args(&["run", "--conf=a.toml", "--pub=b.toml"]));
        assert_eq!(conf, Some(PathBuf::from("a.toml")));
        assert_eq!(pub_path, Some(PathBuf::from("b.toml")));
        assert_eq!(rest, args(&["run"]));
    }

    #[test]
    fn parse_global_paths_is_order_independent_and_keeps_unknown_args() {
        // 全局选项在子命令前后都能被摘出，其余参数（如 `service install --config X`）原样保留。
        let (conf, pub_path, rest) = parse_global_paths(&args(&[
            "service",
            "install",
            "--conf",
            "c.toml",
            "--config",
            "unit-conf.toml",
            "--pub",
            "p.toml",
        ]));
        assert_eq!(conf, Some(PathBuf::from("c.toml")));
        assert_eq!(pub_path, Some(PathBuf::from("p.toml")));
        // `service` 自己的 `--config` 与全局 `--conf` 不同名，不被摘除。
        assert_eq!(
            rest,
            args(&["service", "install", "--config", "unit-conf.toml"])
        );
    }

    #[test]
    fn parse_global_paths_accepts_short_aliases() {
        // `-c` / `-p` 与 `--conf` / `--pub` 同义（写错了会被静默丢掉 → 回落默认配置，很难查）。
        let (conf, pub_path, rest) =
            parse_global_paths(&args(&["diagnose", "-c", "a.toml", "-p", "b.toml"]));
        assert_eq!(conf, Some(PathBuf::from("a.toml")));
        assert_eq!(pub_path, Some(PathBuf::from("b.toml")));
        assert_eq!(rest, args(&["diagnose"]));

        let (conf, pub_path, rest) =
            parse_global_paths(&args(&["diagnose", "-c=a.toml", "-p=b.toml"]));
        assert_eq!(conf, Some(PathBuf::from("a.toml")));
        assert_eq!(pub_path, Some(PathBuf::from("b.toml")));
        assert_eq!(rest, args(&["diagnose"]));

        // 短名放在子命令**前**（与长名同一口径：前后都行）。
        let (conf, pub_path, rest) = parse_global_paths(&args(&["-c", "a.toml", "version"]));
        assert_eq!(conf, Some(PathBuf::from("a.toml")));
        assert_eq!(pub_path, None);
        assert_eq!(rest, args(&["version"]));
    }

    #[test]
    fn parse_global_paths_without_flags_keeps_everything() {
        let (conf, pub_path, rest) = parse_global_paths(&args(&["diagnose"]));
        assert_eq!(conf, None);
        assert_eq!(pub_path, None);
        assert_eq!(rest, args(&["diagnose"]));
    }

    #[test]
    fn absolutize_from_cwd_keeps_absolute_and_prefixes_relative() {
        let abs = PathBuf::from("/tmp/abs/gwlinkd.toml");
        assert_eq!(absolutize_from_cwd(abs.clone()), abs);

        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(
            absolutize_from_cwd(PathBuf::from("rel/gwlinkd.toml")),
            cwd.join("rel/gwlinkd.toml")
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
        /// 收到的 `upgrade-result` body（按到达顺序）。
        reports: Vec<String>,
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
            identity_dir: None,
            gateway_id: "gw-1".into(),
            link_token: None,
            gateway_self_endpoint: None,
            gateway_self_ca: None,
            agent_package_drop_dir: None,
            agent_package_container_dir: None,
            agent_package_drop_keep: None,
            renew_lead_seconds: None,
            upgrader_program: None,
            upgrade_on_failure: None,
            upgrade_health_cmd: None,
            upgrade_health_timeout_seconds: None,
            upgrade_verify_timeout_seconds: None,
            upgrade_project_dir: None,
            upgrade_project_name: None,
            upgrade_retry_on_dead: None,
            upgrade_tool_require_arch: None,
            upgrade: Default::default(),
            wparse_relay: Default::default(),
            log: Default::default(),
            pub_config_path: None,
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
                    let body_start = head.find("\r\n\r\n").map(|i| i + 4).unwrap_or(head.len());
                    let req_body = head[body_start..].to_string();
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
                        } else if route == "/api/v1/gateway/upgrade-result" {
                            guard.reports.push(req_body.clone());
                            (
                                200,
                                r#"{"gateway_id":"gw-1","work_id":"w","accepted_at":"2026-10-09T00:00:00Z"}"#
                                    .into(),
                            )
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

    #[test]
    fn init_config_writes_a_loadable_config() {
        let dir = temp_dir("init-config");
        let path = dir.join("gwlinkd.toml");
        init_config_command(Some(path.display().to_string())).expect("init-config");
        // 开箱即可加载：声明里的 `${VAR}` 由一并生成的 `.env` 骶架提供（值可为空）。
        let config = Config::load(&path).expect("生成的配置必须可加载");
        // `.env` 骶架里 GWLINKD_GATEWAY_ID=gw-local（声明里的 `${GWLINKD_GATEWAY_ID}` 取到它）。
        assert_eq!(config.gateway_id, "gw-local");
        assert!(config.control_center_endpoint.is_empty(), "空 = 尚未接入");
        // `:-` 默认值：骶架里没有 GWLINKD_LINK_TOKEN 行 → 空（= 无券）。
        assert!(config.link_token.as_deref().unwrap_or("").is_empty());
        // 声明 + 值两份都在，且声明**不含现场值**。
        let decl = std::fs::read_to_string(wist_gwlinkd::config::pub_config_path(&path))
            .expect("read declaration");
        assert!(decl.contains("${GWLINKD_CENTER_ENDPOINT}"), "{decl}");
        assert!(!decl.contains("https://"), "声明不应含现场值：{decl}");
        assert!(
            wist_gwlinkd::config::pub_env_path(&wist_gwlinkd::config::pub_config_path(&path))
                .is_file(),
            ".env 骶架应已生成"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_link_token_ignores_blank_values() {
        let dir = temp_dir("cfg-token");
        let mut config = test_config(&dir, "https://c".to_string());
        assert_eq!(link_token_from_config(&config), None, "未配置 → None");
        config.link_token = Some("   ".to_string());
        assert_eq!(link_token_from_config(&config), None, "空白不算配置");
        config.link_token = Some(" link_x ".to_string());
        assert_eq!(link_token_from_config(&config).as_deref(), Some("link_x"));
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
        let config = test_config(&dir, url.clone());
        // 配置路径：接入那一步会把**现场值**写进声明旁边的 `.env`（声明本身不动）。
        // 主配置只放**交付物类**键。
        let config_path = dir.join("gwlinkd.toml");
        std::fs::write(
            &config_path,
            format!("state_dir = \"{state}\"\n", state = dir.display()),
        )
        .expect("write config");
        // 现场态声明：只写引用（与内置骶架同形）。
        std::fs::write(
            wist_gwlinkd::config::pub_config_path(&config_path),
            "control_center_endpoint = \"${GWLINKD_CENTER_ENDPOINT}\"\n\
             gateway_id = \"${GWLINKD_GATEWAY_ID}\"\n\
             trust_bundle = \"${GWLINKD_TRUST_BUNDLE:-}\"\n\
             link_token = \"${GWLINKD_LINK_TOKEN:-}\"\n",
        )
        .expect("write declaration");

        let registered = onboard_from_gateway(&config_path, &config, None, "ident-1", &client)
            .await
            .expect("onboard");
        assert!(registered, "有待办应完成接入");
        // 现场接线**不写回主配置**（主配置是渲染物，重渲会抹掉它）。
        let written = std::fs::read_to_string(&config_path).expect("read config");
        assert!(
            !written.contains("control_center_endpoint") && !written.contains("gateway_id"),
            "现场接线不应写回主配置：{written}"
        );
        // 现场接线落进声明旁边的 `.env`（= 链接关系的持久记录）；一次性券在成功接入后**已清掉**。
        let env_path = wist_gwlinkd::config::pub_env_path(&wist_gwlinkd::config::pub_config_path(
            &config_path,
        ));
        let env_text = std::fs::read_to_string(&env_path).expect("read .env");
        assert!(env_text.contains("GWLINKD_GATEWAY_ID=gw-1"), "{env_text}");
        assert!(env_text.contains("GWLINKD_CENTER_ENDPOINT="), "{env_text}");
        assert!(
            !env_text.contains("GWLINKD_LINK_TOKEN="),
            "一次性券应在接入成功后清掉：{env_text}"
        );
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
        let config = test_config(&dir, url.clone());
        let config_path = dir.join("gwlinkd.toml");
        std::fs::write(
            &config_path,
            format!("state_dir = \"{state}\"\n", state = dir.display()),
        )
        .expect("write config");

        let registered = onboard_from_gateway(&config_path, &config, None, "ident-1", &client)
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
    async fn onboard_explains_when_link_upstream_requires_a_certificate() {
        // 已初始化的 gateway_id：link-upstream 改走 mTLS，本地无证书 → 401 `certificate_required`。
        // 必须报成**可处置**的话（重置实例 / 换新 gateway_id），而不是把裸 code 抛给运维。
        let dir = temp_dir("onboard-cert-required");
        let stub = Arc::new(Mutex::new(Stub {
            link: Some((
                401,
                "gateway identity rejected: certificate_required".to_string(),
            )),
            ..Default::default()
        }));
        let url = serve_center(Arc::clone(&stub)).await;
        let client = CenterClient::new(url);
        let config = test_config(&dir, client.endpoint().to_string());

        let err = onboard(&client, &config, "ident-1", Some("link-1"))
            .await
            .expect_err("应报错");
        assert!(err.to_string().contains("新建一个实例"), "{err}");
        assert!(err.to_string().contains("新 gateway_id"), "{err}");
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
        assert!(err.to_string().contains("身份重置"), "{err}");
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
        assert!(err.to_string().contains("WIST_GWLINKD_LINK_TOKEN"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 发布 ②：`push-agent-package` 计划 → **gwlinkd 取包**（带 CA-S 客户端）→ 校验摘要 → 落到投放目录 → 环回**交付**网关 + 回报中心 `done` + 落游标。
    #[tokio::test]
    async fn drive_agent_package_push_fetches_delivers_and_reports() {
        // 本机平台：认不出就跳过（罕见主机）。
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };

        let dir = temp_dir("agent-package-push");

        // 制品桩：gwlinkd 用自己的客户端取它（http 明文即可）。任意路径都回同一段字节。
        let payload = b"fake-agentd-package-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload.clone()).await;

        // **多平台**：契约 `artifacts` 带两个平台（本机 + 另一个 —— 机队平台可能 ≠ 网关本机平台）。
        let other_platform = if platform == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-apple-darwin".to_string()
        };
        let host_url = format!("{artifact_base}/wist-agentd-0.1.9-{platform}.tar.gz");
        let other_url = format!("{artifact_base}/wist-agentd-0.1.9-{other_platform}.tar.gz");

        // 网关桩：捕获环回交付 POST，回 200。
        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;

        // 中心桩（只用于回执）。
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.gateway_self_endpoint = Some(gw_base.clone());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let mut plan = push_plan(None, None); // 走 `artifacts` 分支，单值留空
        plan.artifacts = vec![
            multi_artifact(&platform, &host_url, &digest),
            multi_artifact(&other_platform, &other_url, &digest),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 落盘：**两个平台**各自文件都在（文件名前缀平台），内容一致。
        for platform in [&platform, &other_platform] {
            let dropped = drop_dir.join(format!("{platform}__wist-agentd-0.1.9-{platform}.tar.gz"));
            assert_eq!(
                std::fs::read(&dropped)
                    .unwrap_or_else(|err| panic!("{}: {err}", dropped.display())),
                payload,
                "取到的字节落到投放目录（{platform}）"
            );
        }

        // 网关收到**一次**环回交付，body 带**两个**平台（整批一次提交）。
        let request = rx.await.expect("gateway captured");
        assert!(
            request.starts_with("POST /api/v1/gateway/agent-package "),
            "{request}"
        );
        for platform in [&platform, &other_platform] {
            assert!(
                request.contains(&format!("\"platform\":\"{platform}\"")),
                "多平台：{platform} 要在同一次交付里：{request}"
            );
            assert!(
                request.contains(&format!(
                    "\"package_url\":\"/packages/{platform}__wist-agentd-0.1.9-{platform}.tar.gz\""
                )),
                "{request}"
            );
        }
        assert!(
            request.contains(&format!("\"origin\":\"{host_url}\"")),
            "{request}"
        );
        assert!(
            request.contains(&format!("\"package_sha256\":\"sha256:{digest}\"")),
            "{request}"
        );

        // 中心收到 `done` 回执（回填发布计划条目）。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1, "one upgrade-result report");
        assert!(reports[0].contains("\"status\":\"done\""), "{}", reports[0]);
        assert!(
            reports[0].contains("\"work_id\":\"plan-push-1\""),
            "{}",
            reports[0]
        );

        // 任务日志（P0）：回执 detail 带目录，目录里有 task.json / events.jsonl。
        assert!(
            reports[0].contains("日志: "),
            "回执要带任务日志目录：{}",
            reports[0]
        );
        let task_dirs: Vec<_> = std::fs::read_dir(dir.join("tasks"))
            .expect("tasks dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        assert_eq!(task_dirs.len(), 1, "{task_dirs:?}");
        let task_dir = &task_dirs[0];
        assert!(task_dir.join("task.json").exists());
        let events = std::fs::read_to_string(task_dir.join("events.jsonl")).unwrap();
        assert!(events.contains("run.start"), "{events}");
        assert!(events.contains("\"phase\":\"done\""), "{events}");

        // 落游标：同一计划不再重复驱动。
        let cursor = wist_gwlinkd::state::load_upgrade_cursor(&dir);
        assert_eq!(cursor.last_plan_id.as_deref(), Some("plan-push-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 「知识库包下发」计划：单制品（无平台）+ 可选 `activate`。
    fn knowledge_plan(
        url: Option<&str>,
        sha: Option<&str>,
        activate: bool,
    ) -> wist_control::GatewayUpgradePlan {
        wist_control::GatewayUpgradePlan {
            gateway_id: "gw-1".into(),
            has_plan: true,
            plan_id: Some("plan-knowledge-1".into()),
            component: Some("wist-knowledge".into()),
            to_version: Some("0.3.0".into()),
            artifact_url: url.map(str::to_string),
            action: Some(wist_control::ACTION_PUSH_KNOWLEDGE_PACKAGE.into()),
            artifact_sha256: sha.map(str::to_string),
            artifacts: Vec::new(),
            activate: Some(activate),
        }
    }

    /// 「知识库包下发」：gwlinkd 取包（含 `.sig` 旁文件）→ 校验摘要 → 落到投放目录 →
    /// 环回交付网关（`source` 是容器内路径、`origin` 是中心地址、`activate` 原样带过）→
    /// 回报中心 `done` + 落游标。
    #[tokio::test]
    async fn drive_knowledge_package_push_delivers_signature_and_reports() {
        let dir = temp_dir("knowledge-package-push");

        // 制品桩：`<base>/wist-knowledge-0.3.0.tar.gz` 与它的 `.sig` 都回同一段字节
        // （内容不重要，只验证「取到并落地」）。
        let payload = b"fake-knowledge-package-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload.clone()).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");

        // 网关桩：捕获环回交付 POST，回 200。
        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"package_id":"kbp-a"}"#).await;

        // 中心桩（只用于回执）。
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.gateway_self_endpoint = Some(gw_base.clone());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let plan = knowledge_plan(Some(&artifact_url), Some(&digest), true);
        let pusher = KnowledgePackageClient::new(gw_base);
        drive_knowledge_package_push(&config, &client, &pusher, &plan).await;

        // 落盘：包本体 + 签名旁文件（配了验签公钥的网关读 `<source>.sig`）。
        let dropped = drop_dir.join("wist-knowledge-0.3.0.tar.gz");
        assert_eq!(
            std::fs::read(&dropped).unwrap_or_else(|err| panic!("{}: {err}", dropped.display())),
            payload,
            "取到的字节落到投放目录"
        );
        assert!(
            drop_dir.join("wist-knowledge-0.3.0.tar.gz.sig").is_file(),
            "签名旁文件要一起落地（否则配了公钥的网关会拒收）"
        );

        // 网关收到交付：来源是容器内路径、origin 是中心地址、activate 原样带过。
        let request = rx.await.expect("gateway captured");
        assert!(
            request.starts_with("POST /api/v1/gateway/knowledge-package "),
            "{request}"
        );
        assert!(
            request.contains("\"source\":\"/packages/wist-knowledge-0.3.0.tar.gz\""),
            "{request}"
        );
        assert!(
            request.contains(&format!("\"sha256\":\"{digest}\"")),
            "{request}"
        );
        assert!(
            request.contains(&format!("\"origin\":\"{artifact_url}\"")),
            "{request}"
        );
        assert!(request.contains("\"activate\":true"), "{request}");

        // 中心收到 `done` 回执 + 游标。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1, "one upgrade-result report");
        assert!(reports[0].contains("\"status\":\"done\""), "{}", reports[0]);
        assert!(
            reports[0].contains("\"work_id\":\"plan-knowledge-1\""),
            "{}",
            reports[0]
        );
        let cursor = wist_gwlinkd::state::load_upgrade_cursor(&dir);
        assert_eq!(cursor.last_plan_id.as_deref(), Some("plan-knowledge-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 动作分派：**未知动作绝不回落成 `upgrade`**。
    ///
    /// 防的是 2026-10-18 的事故形态：旧版本把 `push-knowledge-package` 当成升级，拿知识库包跑 gops。
    /// 新增动作忘了加分支时，期望结果是「拒绝并留痕」，而不是「拿错包去装网关」。
    #[test]
    fn unknown_plan_actions_never_fall_back_to_upgrade() {
        assert_eq!(
            plan_intent(wist_control::ACTION_UPGRADE),
            PlanIntent::Upgrade
        );
        assert_eq!(
            plan_intent(wist_control::ACTION_PUSH_AGENT_PACKAGE),
            PlanIntent::PushAgentPackage
        );
        assert_eq!(
            plan_intent(wist_control::ACTION_PUSH_KNOWLEDGE_PACKAGE),
            PlanIntent::PushKnowledgePackage
        );
        for unknown in ["", "push-strategy-package", "upgrade ", "UPGRADE"] {
            assert_eq!(
                plan_intent(unknown),
                PlanIntent::Unsupported,
                "未知动作 {unknown:?} 必须落到 Unsupported（绝不升级）"
            );
        }
    }

    /// 「知识库包下发」缺地址 / 切书 → 报可读原因，不落盘、不交付。
    #[tokio::test]
    async fn deliver_knowledge_package_requires_url_and_digest() {
        let dir = temp_dir("knowledge-package-missing");
        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = Some(dir.join("packages"));
        config.agent_package_container_dir = Some("/packages".into());
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new("http://127.0.0.1:9");

        let err = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(None, None, false),
            false,
        )
        .await
        .expect_err("no url must fail");
        assert!(err.to_string().contains("制品地址"), "{err}");

        let err = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(Some("http://127.0.0.1:9/k.tar.gz"), None, false),
            false,
        )
        .await
        .expect_err("no digest must fail");
        assert!(err.to_string().contains("摘要"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ② 计划的 `GatewayUpgradePlan`（**单值回落**）：`artifact_url` / `artifact_sha256` 可省（造失败分支）。
    /// `artifacts` 留空 —— 走「旧中心 / 单值 + 本机平台」回落；多平台用例另自行置 `plan.artifacts`。
    fn push_plan(url: Option<&str>, sha: Option<&str>) -> wist_control::GatewayUpgradePlan {
        wist_control::GatewayUpgradePlan {
            gateway_id: "gw-1".into(),
            has_plan: true,
            plan_id: Some("plan-push-1".into()),
            component: Some("wist-agentd".into()),
            to_version: Some("0.1.9".into()),
            artifact_url: url.map(str::to_string),
            action: Some(wist_control::ACTION_PUSH_AGENT_PACKAGE.into()),
            artifact_sha256: sha.map(str::to_string),
            artifacts: Vec::new(),
            activate: None,
        }
    }

    /// ② 多平台制品项（造 `plan.artifacts` 用）。
    fn multi_artifact(
        platform: &str,
        url: &str,
        sha_without_prefix: &str,
    ) -> wist_control::GatewayUpgradeArtifact {
        wist_control::GatewayUpgradeArtifact {
            platform: platform.to_string(),
            artifact_url: url.to_string(),
            artifact_sha256: format!("sha256:{sha_without_prefix}"),
        }
    }

    /// `target_artifacts`：`artifacts` 非空（**多平台**，中心给该版本全部平台）优先；为空才回落
    /// 单值 `artifact_url` + 摘要 + **本机平台**（旧中心 / ① 兼容）。
    #[test]
    fn target_artifacts_prefers_the_multi_platform_list() {
        // 多平台清单优先：单值即使也在，也被忽略。
        let mut plan = push_plan(Some("https://c/single.tar.gz"), Some("sha256:single"));
        plan.artifacts = vec![
            multi_artifact("x86_64-unknown-linux-musl", "https://c/a.tar.gz", "aa"),
            multi_artifact("aarch64-apple-darwin", "https://c/b.tar.gz", "bb"),
        ];
        assert_eq!(
            target_artifacts(&plan).expect("targets"),
            vec![
                (
                    "x86_64-unknown-linux-musl".to_string(),
                    "https://c/a.tar.gz".to_string(),
                    "sha256:aa".to_string(),
                ),
                (
                    "aarch64-apple-darwin".to_string(),
                    "https://c/b.tar.gz".to_string(),
                    "sha256:bb".to_string(),
                ),
            ],
            "多平台清单优先，避开单值"
        );

        // 清单为空 → 回落单值 + **本机平台**（认不出本机平台则报可读原因）。
        let single = push_plan(Some("https://c/single.tar.gz"), Some("sha256:single"));
        match wist_gwlinkd::target::HostTarget::detect().target_triple() {
            Some(host) => assert_eq!(
                target_artifacts(&single).expect("targets"),
                vec![(
                    host.to_string(),
                    "https://c/single.tar.gz".to_string(),
                    "sha256:single".to_string(),
                )]
            ),
            None => assert!(target_artifacts(&single).is_err(), "认不出本机平台应报错"),
        }

        // 单值缺地址 → 报可读原因（清单也空）。
        let missing = push_plan(None, Some("sha256:x"));
        assert!(
            target_artifacts(&missing)
                .unwrap_err()
                .to_string()
                .contains("未派生制品地址")
        );
    }

    /// **同名制品**（同版本多平台的来源原名相同）不得互相覆盖：投放文件名前缀平台 → 一平台一份，
    /// 交付里两个平台各指各的文件（否则后者覆盖前者，网关摘要校验必有一方不过、整批被拒）。
    #[tokio::test]
    async fn drive_agent_package_push_keeps_same_named_platforms_separate() {
        let dir = temp_dir("agent-package-samename");
        let payload = b"same-named-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload.clone()).await;
        // **相同的末段文件名**给两个不同平台。
        let shared_url = format!("{artifact_base}/wist-agentd-0.2.1.tar.gz");

        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let mut plan = push_plan(None, None);
        plan.artifacts = vec![
            multi_artifact("aarch64-apple-darwin", &shared_url, &digest),
            multi_artifact("x86_64-unknown-linux-musl", &shared_url, &digest),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        for platform in ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"] {
            let dropped = drop_dir.join(format!("{platform}__wist-agentd-0.2.1.tar.gz"));
            assert_eq!(
                std::fs::read(&dropped)
                    .unwrap_or_else(|err| panic!("{}: {err}", dropped.display())),
                payload,
                "同名制品不得互相覆盖（{platform}）"
            );
        }

        let request = rx.await.expect("gateway captured");
        assert!(
            request.contains(
                "\"package_url\":\"/packages/aarch64-apple-darwin__wist-agentd-0.2.1.tar.gz\""
            ),
            "{request}"
        );
        assert!(
            request.contains(
                "\"package_url\":\"/packages/x86_64-unknown-linux-musl__wist-agentd-0.2.1.tar.gz\""
            ),
            "{request}"
        );
        let reports = stub.lock().unwrap().reports.clone();
        assert!(reports[0].contains("\"status\":\"done\""), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 一次性网关桩：捕获收到的原始请求，并按给定状态码/体回一次；返回 `http://addr`。
    async fn one_shot_gateway(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gw");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0_u8; 4096];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// `unlink` 后会把「已断开接入」这一拍推给网关（state=`WaitingLinkRequest`，**不带**凭据），
    /// 让页面立刻别再显示「已接入」。
    #[tokio::test]
    async fn announce_unlinked_reports_waiting_state_without_credential() {
        let dir = temp_dir("announce-unlink");
        let (gateway, captured) = one_shot_gateway("200 OK", "{}").await;
        let mut config = test_config(&dir, "https://c".into());
        config.gateway_self_endpoint = Some(gateway);

        announce_unlinked_to_gateway(&config).await;

        let request = captured.await.expect("request captured");
        assert!(
            request
                .to_lowercase()
                .starts_with("post /api/v1/gateway/linkd-status"),
            "{request}"
        );
        assert!(
            request.contains("\"state\":\"WaitingLinkRequest\""),
            "{request}"
        );
        assert!(
            !request.contains("credential_expires_at"),
            "凭据已删，不该带 credential_expires_at：{request}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没配 `gateway_self_endpoint` 时，告知是**静默跳过**（不 panic、不阻塞）。
    #[tokio::test]
    async fn announce_unlinked_is_a_noop_without_a_self_endpoint() {
        let dir = temp_dir("announce-skip");
        let config = test_config(&dir, "https://c".into());
        announce_unlinked_to_gateway(&config).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 交付失败也要**回报 `failed` + 落游标**：否则同一份计划每轮重试（0.12.x 的行为）。
    #[tokio::test]
    async fn drive_knowledge_package_push_reports_failure_and_saves_the_cursor() {
        let dir = temp_dir("knowledge-package-failure");
        let payload = b"knowledge-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");

        // 网关拒收（如旧网关没有 /knowledge-package 端点 → 404）。
        let (gw_base, _rx) =
            one_shot_gateway("404 Not Found", "knowledge-package is loopback-only").await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.gateway_self_endpoint = Some(gw_base.clone());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let pusher = KnowledgePackageClient::new(gw_base);
        let plan = knowledge_plan(Some(&artifact_url), Some(&digest), false);
        drive_knowledge_package_push(&config, &client, &pusher, &plan).await;

        // 中心收到 failed（而不是静默）。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1, "one upgrade-result report");
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(
            reports[0].contains("\"work_id\":\"plan-knowledge-1\""),
            "{}",
            reports[0]
        );
        assert!(
            reports[0].contains("404") || reports[0].contains("knowledge-package"),
            "失败回执要带上可诊断的原因：{}",
            reports[0]
        );

        // 游标已落：同一计划不再重试。
        let cursor = wist_gwlinkd::state::load_upgrade_cursor(&dir);
        assert_eq!(cursor.last_plan_id.as_deref(), Some("plan-knowledge-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 容器路径前缀必须是**绝对路径**：相对路径在网关侧会被当「不支持的来源」拒，
    /// 但在 gwlinkd 侧就报清楚更好定位。
    #[tokio::test]
    async fn deliver_knowledge_package_rejects_a_relative_container_dir() {
        let dir = temp_dir("knowledge-package-relative-container");
        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = Some(dir.join("packages"));
        config.agent_package_container_dir = Some("packages".into());
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new("http://127.0.0.1:9");

        let err = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(
                Some("http://127.0.0.1:9/k.tar.gz"),
                Some(&"0".repeat(64)),
                false,
            ),
            false,
        )
        .await
        .expect_err("relative container dir must fail");
        assert!(err.to_string().contains("绝对路径"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没配投放目录 → 报出**配置键名**（而不是等到交付环节才发现）。
    #[tokio::test]
    async fn deliver_knowledge_package_requires_a_drop_dir() {
        let dir = temp_dir("knowledge-package-no-drop");
        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = None;
        config.agent_package_container_dir = Some("/packages".into());
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new("http://127.0.0.1:9");

        let err = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(
                Some("http://127.0.0.1:9/k.tar.gz"),
                Some(&"0".repeat(64)),
                false,
            ),
            false,
        )
        .await
        .expect_err("missing drop dir must fail");
        assert!(err.to_string().contains("agent_package_drop_dir"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 交付**失败也清旧**：投放目录是中转区，不能因反复失败而无界增长。
    #[tokio::test]
    async fn deliver_knowledge_package_prunes_after_a_failed_delivery() {
        use std::time::{Duration, UNIX_EPOCH};

        let dir = temp_dir("knowledge-package-prune");
        let payload = b"knowledge-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");
        let (gw_base, _rx) = one_shot_gateway("500 Internal Server Error", "boom").await;
        let client = CenterClient::new("http://127.0.0.1:9".to_string());

        let drop_dir = dir.join("packages");
        std::fs::create_dir_all(&drop_dir).expect("mkdir");
        // 旧交付：mtime 一律固定在很久以前，保证本次落盘的两份是「最新」。
        for index in 0..20 {
            let path = drop_dir.join(format!("old-{index}.tar.gz"));
            std::fs::write(&path, b"old").expect("write old");
            let file = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open old");
            file.set_modified(UNIX_EPOCH + Duration::from_secs(index as u64 * 10))
                .expect("set mtime");
        }

        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        config.agent_package_drop_keep = Some(2);
        let pusher = KnowledgePackageClient::new(gw_base);

        let _ = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(Some(&artifact_url), Some(&digest), false),
            false,
        )
        .await;

        // keep=2：本次的包 + 签名（最新）保留，旧交付被清。
        let remaining = std::fs::read_dir(&drop_dir)
            .expect("read drop dir")
            .filter_map(Result::ok)
            .count();
        assert!(
            remaining <= 2,
            "交付失败也要清旧（keep=2），实际剩 {remaining} 个文件"
        );
        assert!(
            drop_dir.join("wist-knowledge-0.3.0.tar.gz").is_file(),
            "本次的包不该被自己的清理误删"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `keep = 1` 也不能把本次交付的签名清掉：知识库一次交付是**两份文件**（包 + `.sig`），
    /// 所以保留数会抬到 2 —— 否则网关验签时发现 `.sig` 自己没了。
    #[tokio::test]
    async fn knowledge_delivery_keeps_its_own_signature_even_with_keep_1() {
        let dir = temp_dir("knowledge-package-keep1");
        let payload = b"knowledge-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");
        let (gw_base, _rx) = one_shot_gateway("200 OK", "{}").await;

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        config.agent_package_drop_keep = Some(1);
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new(gw_base);

        deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(Some(&artifact_url), Some(&digest), false),
            false,
        )
        .await
        .expect("delivery ok");

        assert!(
            drop_dir.join("wist-knowledge-0.3.0.tar.gz").is_file(),
            "包本体不能被自己清掉"
        );
        assert!(
            drop_dir.join("wist-knowledge-0.3.0.tar.gz.sig").is_file(),
            "签名不能被自己清掉（keep=1 也要抬到 2）"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 摘要不符 → 拒绝交付：不落盘、不推送（网关收到的字节必须与中心声明的一致）。
    #[tokio::test]
    async fn deliver_knowledge_package_refuses_a_digest_mismatch() {
        let dir = temp_dir("knowledge-package-digest");
        let payload = b"actual-bytes".to_vec();
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");
        let (gw_base, _rx) = one_shot_gateway("200 OK", r#"{}"#).await;

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new(gw_base);

        // 中心声明的摘要与制品字节不一致。
        let wrong = "0".repeat(64);
        let err = deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(Some(&artifact_url), Some(&wrong), false),
            false,
        )
        .await
        .expect_err("digest mismatch must fail");
        assert!(err.to_string().contains("摘要不符"), "{err}");
        assert!(
            !drop_dir.join("wist-knowledge-0.3.0.tar.gz").exists(),
            "摘要不符时不得落盘"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 取不到 `.sig` 不阻断交付，但要把**除旧的同名 `.sig`** 删掉
    /// （否则网关会拿上一版的签名去验这一版）。
    #[tokio::test]
    async fn deliver_knowledge_package_without_a_signature_drops_the_stale_one() {
        let dir = temp_dir("knowledge-package-no-sig");
        let payload = b"knowledge-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact_only("/wist-knowledge-0.3.0.tar.gz", payload).await;
        let artifact_url = format!("{artifact_base}/wist-knowledge-0.3.0.tar.gz");
        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"package_id":"kbp-a"}"#).await;

        let drop_dir = dir.join("packages");
        std::fs::create_dir_all(&drop_dir).expect("mkdir");
        // 上一版遗留的签名：取不到新签名时必须被清掉。
        let stale = drop_dir.join("wist-knowledge-0.3.0.tar.gz.sig");
        std::fs::write(&stale, b"stale-signature").expect("write stale sig");

        let mut config = test_config(&dir, "http://127.0.0.1:9".to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        let client = CenterClient::new("http://127.0.0.1:9".to_string());
        let pusher = KnowledgePackageClient::new(gw_base);

        deliver_knowledge_package(
            &config,
            &client,
            &pusher,
            &knowledge_plan(Some(&artifact_url), Some(&digest), false),
            false,
        )
        .await
        .expect("missing sig must not block delivery");

        assert!(
            drop_dir.join("wist-knowledge-0.3.0.tar.gz").is_file(),
            "包本体要落盘"
        );
        assert!(!stale.exists(), "旧签名必须清掉（否则会拿旧签名验新包）");
        let request = rx.await.expect("gateway captured");
        assert!(request.contains("\"activate\":false"), "{request}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 只服务**一个**路径的制品桩：其余路径回 404。
    ///
    /// 用于「签名旁文件不存在」这类用例 —— 通用的 [`serve_artifact`] 对任何路径都回字节，
    /// 无法模拟 404。
    async fn serve_artifact_only(path: &'static str, bytes: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind artifact");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let bytes = bytes.clone();
                tokio::spawn(async move {
                    let mut buf = [0_u8; 4096];
                    let read = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]);
                    if request.starts_with(&format!("GET {path} ")) {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            bytes.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&bytes).await;
                    } else {
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            )
                            .await;
                    }
                    let _ = sock.flush().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// 极简制品桩：任意 GET 都回这段字节（`application/octet-stream`），可反复取。
    async fn serve_artifact(bytes: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind artifact");
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

    /// 多平台：**每个平台取各自的字节、各自校验各自的摘要**（防跳平台摘要串味），
    /// 交付报文里每个平台带自己的 `package_url` / `package_sha256`。
    #[tokio::test]
    async fn drive_agent_package_push_keeps_each_platforms_own_bytes_and_digest() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let other = if platform == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-apple-darwin".to_string()
        };
        let dir = temp_dir("agent-package-perplatform");

        // 两个平台各自的制品桩：**不同字节、不同摘要**。
        let bytes_host = b"host-platform-bytes".to_vec();
        let bytes_other = b"other-platform-bytes".to_vec();
        let digest_host = wist_artifact::digest::sha256_hex_bytes(&bytes_host);
        let digest_other = wist_artifact::digest::sha256_hex_bytes(&bytes_other);
        let base_host = serve_artifact(bytes_host.clone()).await;
        let base_other = serve_artifact(bytes_other.clone()).await;
        let url_host = format!("{base_host}/wist-agentd-0.2.1-{platform}.tar.gz");
        let url_other = format!("{base_other}/wist-agentd-0.2.1-{other}.tar.gz");

        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let mut plan = push_plan(None, None);
        plan.artifacts = vec![
            multi_artifact(&platform, &url_host, &digest_host),
            multi_artifact(&other, &url_other, &digest_other),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 各平台落到自己的文件、写的是**自己那份**字节。
        assert_eq!(
            std::fs::read(
                drop_dir.join(format!("{platform}__wist-agentd-0.2.1-{platform}.tar.gz"))
            )
            .expect("host dropped"),
            bytes_host,
            "本机平台文件内容"
        );
        assert_eq!(
            std::fs::read(drop_dir.join(format!("{other}__wist-agentd-0.2.1-{other}.tar.gz")))
                .expect("other dropped"),
            bytes_other,
            "另一平台文件内容"
        );

        // 交付报文里每个平台带**自己的**摘要（不是同一份串给所有平台）。
        let request = rx.await.expect("gateway captured");
        for (platform, digest) in [(&platform, &digest_host), (&other, &digest_other)] {
            assert!(
                request.contains(&format!("\"platform\":\"{platform}\"")),
                "{request}"
            );
            assert!(
                request.contains(&format!("\"package_sha256\":\"sha256:{digest}\"")),
                "{platform} 的摘要在报文里：{request}"
            );
        }
        let reports = stub.lock().unwrap().reports.clone();
        assert!(reports[0].contains("\"status\":\"done\""), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 一批里**任一平台**取包/摘要失败 → **不**交付网关（不碰环回面）、回报 `failed`。
    /// 前置平台已落盘的文件会留下（见设计 `edge/agent-package-push-to-gateways.md` §10 遗留；网关侧未被触碰）。
    #[tokio::test]
    async fn drive_agent_package_push_stops_before_delivery_when_one_platform_fails() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let other = if platform == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-apple-darwin".to_string()
        };
        let dir = temp_dir("agent-package-partial");

        let bytes_host = b"host-ok-bytes".to_vec();
        let digest_host = wist_artifact::digest::sha256_hex_bytes(&bytes_host);
        let base_host = serve_artifact(bytes_host.clone()).await;
        let url_host = format!("{base_host}/wist-agentd-0.2.1-{platform}.tar.gz");

        // 另一平台：摘要**故意写错**（取到的字节对不上），驱动应在第二个平台拒绝。
        let base_other = serve_artifact(b"other-bytes".to_vec()).await;
        let url_other = format!("{base_other}/wist-agentd-0.2.1-{other}.tar.gz");

        let (gw_base, mut rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        let mut plan = push_plan(None, None);
        plan.artifacts = vec![
            multi_artifact(&platform, &url_host, &digest_host),
            multi_artifact(
                &other,
                &url_other,
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 不交付：网关的环回面**从未**被联系到。
        assert!(
            rx.try_recv().is_err(),
            "一批里任一平台失败 → 不交付网关（不碰环回面）"
        );
        // 前置平台已落盘的文件留下（best-effort；不影响网关）。
        assert!(
            drop_dir
                .join(format!("{platform}__wist-agentd-0.2.1-{platform}.tar.gz"))
                .exists(),
            "前置平台文件已落盘（遗留，无害）"
        );
        // 回报 failed，detail 带「摘要不符」。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("摘要不符"), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 旧中心兼容（**端到端**）：计划**不带** `artifacts` 时，回落单值 `artifact_url` + 摘要 + **本机平台**，
    /// 照样取包 / 校验 / 落盘 / 交付（不只是 `target_artifacts` 单测）。
    #[tokio::test]
    async fn drive_agent_package_push_falls_back_to_single_value_for_old_centers() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let dir = temp_dir("agent-package-fallback");
        let payload = b"fallback-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let base = serve_artifact(payload.clone()).await;
        let url = format!("{base}/wist-agentd-0.1.9-{platform}.tar.gz");

        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());

        // 旧中心：只有单值（`artifacts` 为空）。
        let plan = push_plan(Some(&url), Some(&format!("sha256:{digest}")));
        assert!(plan.artifacts.is_empty(), "旧中心不带清单");
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 回落用**本机平台**，落到带平台前缀的文件。
        assert_eq!(
            std::fs::read(
                drop_dir.join(format!("{platform}__wist-agentd-0.1.9-{platform}.tar.gz"))
            )
            .expect("dropped"),
            payload
        );
        let request = rx.await.expect("gateway captured");
        assert!(
            request.contains(&format!("\"platform\":\"{platform}\"")),
            "{request}"
        );
        assert!(
            request.contains(&format!(
                "\"package_url\":\"/packages/{platform}__wist-agentd-0.1.9-{platform}.tar.gz\""
            )),
            "{request}"
        );
        assert!(
            request.contains(&format!("\"package_sha256\":\"sha256:{digest}\"")),
            "{request}"
        );
        let reports = stub.lock().unwrap().reports.clone();
        assert!(reports[0].contains("\"status\":\"done\""), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 交付成功后按 `agent_package_drop_keep` 清旧：`keep` 恰好够时**不**丢本次交付，只清更早的旧份。
    #[tokio::test]
    async fn drive_agent_package_push_prunes_old_drops_without_losing_this_delivery() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let other = if platform == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-apple-darwin".to_string()
        };
        let dir = temp_dir("agent-package-prune-delivery");
        let drop_dir = dir.join("packages");
        std::fs::create_dir_all(&drop_dir).expect("mk drop dir");
        // 两个**陈旧**文件（mtime 显式设老，不依赖调度）：交付后应被清掉。
        use std::time::{Duration, UNIX_EPOCH};
        for name in ["stale-a.bin", "stale-b.bin"] {
            let path = drop_dir.join(name);
            std::fs::write(&path, b"old").expect("write stale");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open stale")
                .set_modified(UNIX_EPOCH + Duration::from_secs(1))
                .expect("set stale mtime");
        }

        let payload = b"prune-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let base = serve_artifact(payload.clone()).await;
        let url_host = format!("{base}/wist-agentd-0.2.1-{platform}.tar.gz");
        let url_other = format!("{base}/wist-agentd-0.2.1-{other}.tar.gz");

        let (gw_base, rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        // 故意配小（1 < 本批 2 份）：保留数会被抬到本批目标数，故**不**会误清本次交付。
        config.agent_package_drop_keep = Some(1);

        let mut plan = push_plan(None, None);
        plan.artifacts = vec![
            multi_artifact(&platform, &url_host, &digest),
            multi_artifact(&other, &url_other, &digest),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;
        let _ = rx.await.expect("gateway captured");

        // keep=1 但被抬到本批 2 → 只保最新两份 = 本次交付的两份；陈旧的两份被清。
        assert!(
            drop_dir
                .join(format!("{platform}__wist-agentd-0.2.1-{platform}.tar.gz"))
                .exists()
                && drop_dir
                    .join(format!("{other}__wist-agentd-0.2.1-{other}.tar.gz"))
                    .exists(),
            "本次交付两份都在（keep 不低于本批目标数）"
        );
        assert!(
            !drop_dir.join("stale-a.bin").exists() && !drop_dir.join("stale-b.bin").exists(),
            "陈旧两份被清（keep 抬到 2）"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// P7：交付**失败也会**按数量清旧（best-effort）—— 反复失败不致把投放目录撑爆；
    /// 同时保留数不低于本批目标数，不会误清刚落的文件。
    #[tokio::test]
    async fn drive_agent_package_push_prunes_old_drops_even_when_delivery_fails() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let other = if platform == "aarch64-apple-darwin" {
            "x86_64-unknown-linux-musl".to_string()
        } else {
            "aarch64-apple-darwin".to_string()
        };
        let dir = temp_dir("agent-package-prune-on-failure");
        let drop_dir = dir.join("packages");
        std::fs::create_dir_all(&drop_dir).expect("mk drop dir");
        // 三个陈旧文件，mtime 递增（不依赖调度）。
        use std::time::{Duration, UNIX_EPOCH};
        for (index, name) in ["old-1.bin", "old-2.bin", "old-3.bin"].iter().enumerate() {
            let path = drop_dir.join(name);
            std::fs::write(&path, b"old").expect("write stale");
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open stale")
                .set_modified(UNIX_EPOCH + Duration::from_secs(index as u64 + 1))
                .expect("set stale mtime");
        }

        // 本机平台好、另一平台摘要写错 → 交付失败（在写好本机平台文件后）。
        let bytes = b"ok-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&bytes);
        let base = serve_artifact(bytes).await;
        let url_a = format!("{base}/wist-agentd-0.2.1-{platform}.tar.gz");
        let url_b = format!("{base}/wist-agentd-0.2.1-{other}.tar.gz");

        let (gw_base, mut rx) = one_shot_gateway("200 OK", r#"{"packages":[]}"#).await;
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);

        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        config.agent_package_drop_keep = Some(1); // 抬到本批 2

        let mut plan = push_plan(None, None);
        plan.artifacts = vec![
            multi_artifact(&platform, &url_a, &digest),
            multi_artifact(
                &other,
                &url_b,
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ];
        let pusher = AgentPackageClient::new(gw_base);
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 失败：不交付。
        assert!(rx.try_recv().is_err(), "失败不交付网关");
        // 但清旧跑了：4 份（3 陈旧 + 1 新落）→ keep 抬到 2 → 清最旧的 2 份；
        // 剩 old-3 + 本机平台文件。
        assert!(
            !drop_dir.join("old-1.bin").exists() && !drop_dir.join("old-2.bin").exists(),
            "陈旧两份被清（失败也清）"
        );
        assert!(
            drop_dir.join("old-3.bin").exists(),
            "较新陈旧一份保留（keep=2）"
        );
        assert!(
            drop_dir
                .join(format!("{platform}__wist-agentd-0.2.1-{platform}.tar.gz"))
                .exists(),
            "本批已落的一份不被误清"
        );
        // 回报 failed，detail 带「摘要不符」。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("摘要不符"), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：中心**没派生地址** → **不**碰网关，回报 `failed`，仍落游标（一份计划只驱一次）。
    #[tokio::test]
    async fn drive_agent_package_push_reports_failure_without_a_url() {
        let dir = temp_dir("agent-package-no-url");
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        let config = test_config(&dir, client.endpoint().to_string());
        // 指向一个不会被联系到的 endpoint（缺地址时不该推）。
        let pusher = AgentPackageClient::new("http://127.0.0.1:1");

        let plan = push_plan(None, Some("sha256:deadbeef"));
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("未派生制品地址"), "{}", reports[0]);
        assert_eq!(
            wist_gwlinkd::state::load_upgrade_cursor(&dir)
                .last_plan_id
                .as_deref(),
            Some("plan-push-1")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：中心**没带摘要** → 回报 `failed`，仍落游标。
    #[tokio::test]
    async fn drive_agent_package_push_reports_failure_without_a_digest() {
        let dir = temp_dir("agent-package-no-sha");
        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        let config = test_config(&dir, client.endpoint().to_string());
        let pusher = AgentPackageClient::new("http://127.0.0.1:1");

        let plan = push_plan(
            Some("https://center.example/wist-agentd-0.1.9.tar.gz"),
            None,
        );
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("未带制品摘要"), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：网关**拒收**（如平台不符）→ 回报 `failed`，detail 带网关响应体，仍落游标。
    #[tokio::test]
    async fn drive_agent_package_push_reports_a_gateway_rejection() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let dir = temp_dir("agent-package-reject");
        let payload = b"reject-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-agentd-0.1.9-{platform}.tar.gz");

        let (gw_base, rx) = one_shot_gateway(
            "400 Bad Request",
            "artifact platform `x86_64-unknown-linux-musl` does not match the package triple `aarch64-apple-darwin`",
        )
        .await;

        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(dir.join("packages"));
        config.agent_package_container_dir = Some("/packages".into());
        let pusher = AgentPackageClient::new(gw_base);

        let plan = push_plan(Some(&artifact_url), Some(&format!("sha256:{digest}")));
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 网关确实被联系到。
        let _ = rx.await.expect("gateway captured");
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("400"), "{}", reports[0]);
        assert_eq!(
            wist_gwlinkd::state::load_upgrade_cursor(&dir)
                .last_plan_id
                .as_deref(),
            Some("plan-push-1")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：**未配置投放目录** → fail-closed（不碰网关、不落盘），回报 `failed`。
    #[tokio::test]
    async fn drive_agent_package_push_fails_closed_without_a_drop_dir() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let dir = temp_dir("agent-package-nodrop");
        let payload = b"nodrop-bytes".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-agentd-0.1.9-{platform}.tar.gz");

        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        // 故意**不**配投放目录。
        let config = test_config(&dir, client.endpoint().to_string());
        let pusher = AgentPackageClient::new("http://127.0.0.1:1");

        let plan = push_plan(Some(&artifact_url), Some(&format!("sha256:{digest}")));
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(
            reports[0].contains("agent_package_drop_dir"),
            "{}",
            reports[0]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：**投放目录配了、容器前缀空白** → 同样 fail-closed（空白视为未配置）。
    #[tokio::test]
    async fn drive_agent_package_push_fails_closed_with_a_blank_container_dir() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let dir = temp_dir("agent-package-blankprefix");
        let payload = b"blank-prefix".to_vec();
        let digest = wist_artifact::digest::sha256_hex_bytes(&payload);
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-agentd-0.1.9-{platform}.tar.gz");

        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(dir.join("packages"));
        config.agent_package_container_dir = Some("   ".into());
        let pusher = AgentPackageClient::new("http://127.0.0.1:1");

        let plan = push_plan(Some(&artifact_url), Some(&format!("sha256:{digest}")));
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(
            reports[0].contains("agent_package_container_dir"),
            "{}",
            reports[0]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 失败分支：取到的包**摘要不符**（与中心给的 `artifact_sha256` 对不上）→
    /// **不落盘、不碰网关**，回报 `failed`。
    #[tokio::test]
    async fn drive_agent_package_push_rejects_a_digest_mismatch() {
        let Some(platform) = wist_gwlinkd::target::HostTarget::detect().target_triple() else {
            eprintln!("skip: 测试机平台不可识别");
            return;
        };
        let dir = temp_dir("agent-package-badsha");
        // 桩回的字节是真字节，但计划里的期望摘要故意对不上。
        let payload = b"tampered-bytes".to_vec();
        let artifact_base = serve_artifact(payload).await;
        let artifact_url = format!("{artifact_base}/wist-agentd-0.1.9-{platform}.tar.gz");

        let stub = Arc::new(Mutex::new(Stub::default()));
        let client = CenterClient::new(serve_center(Arc::clone(&stub)).await);
        let drop_dir = dir.join("packages");
        let mut config = test_config(&dir, client.endpoint().to_string());
        config.agent_package_drop_dir = Some(drop_dir.clone());
        config.agent_package_container_dir = Some("/packages".into());
        // 指向不会被联系到的 endpoint（摘要不符时不该推）。
        let pusher = AgentPackageClient::new("http://127.0.0.1:1");

        let plan = push_plan(
            Some(&artifact_url),
            Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
        );
        drive_agent_package_push(&config, &client, &pusher, &plan).await;

        // 不落盘。
        assert!(
            !drop_dir
                .join(format!("{platform}__wist-agentd-0.1.9-{platform}.tar.gz"))
                .exists(),
            "摘要不符不得落盘"
        );
        // 回报 failed，detail 带「摘要不符」（而不是连接错）。
        let reports = stub.lock().unwrap().reports.clone();
        assert_eq!(reports.len(), 1);
        assert!(
            reports[0].contains("\"status\":\"failed\""),
            "{}",
            reports[0]
        );
        assert!(reports[0].contains("摘要不符"), "{}", reports[0]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 制品文件名必须安全：拒空 / `.` / `..` / 控制字符；正常末段原样保留（去 query）。
    #[test]
    fn safe_artifact_filename_rejects_unsafe_names() {
        assert_eq!(
            safe_artifact_filename("https://c/a/b/wist-agentd-0.1.9.tar.gz").unwrap(),
            "wist-agentd-0.1.9.tar.gz"
        );
        assert_eq!(
            safe_artifact_filename("https://c/pkg.tar.gz?sig=1").unwrap(),
            "pkg.tar.gz"
        );
        assert!(safe_artifact_filename("https://c/").is_err());
        assert!(safe_artifact_filename("https://c/..").is_err());
        assert!(safe_artifact_filename("https://c/.").is_err());
        assert!(safe_artifact_filename("https://c/a\u{7}b").is_err());
    }

    /// 投放文件名**平台限定**：`<平台>__<原名>`；空 / `.` / `..` / 含分隔符 / 控制字符的平台报错。
    #[test]
    fn platform_drop_filename_qualifies_and_rejects_unsafe_platforms() {
        assert_eq!(
            platform_drop_filename(
                "aarch64-apple-darwin",
                "https://c/x/wist-agentd-0.2.1.tar.gz"
            )
            .unwrap(),
            "aarch64-apple-darwin__wist-agentd-0.2.1.tar.gz"
        );
        assert!(platform_drop_filename("", "https://c/x/p.tar.gz").is_err());
        assert!(platform_drop_filename("..", "https://c/x/p.tar.gz").is_err());
        assert!(platform_drop_filename(".", "https://c/x/p.tar.gz").is_err());
        assert!(platform_drop_filename("a/b", "https://c/x/p.tar.gz").is_err());
        assert!(platform_drop_filename("a\\b", "https://c/x/p.tar.gz").is_err());
        assert!(platform_drop_filename("a\nb", "https://c/x/p.tar.gz").is_err());
    }

    /// 落盘临时名**追加** `.partial`，不替换扩展名（否则 `x.tar.gz` / `x.tar.zst` 会撞同一临时文件）。
    #[test]
    fn partial_path_appends_instead_of_replacing_the_extension() {
        assert_eq!(
            partial_path(std::path::Path::new("/drop/pkg.tar.gz")),
            std::path::PathBuf::from("/drop/pkg.tar.gz.partial")
        );
    }

    /// 交付后按 mtime 保留**最新** N 份，删其余；子目录不动。
    #[test]
    fn prune_drop_dir_keeps_the_newest_files() {
        use std::time::{Duration, UNIX_EPOCH};
        let dir = temp_dir("agent-package-prune");
        for (index, name) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, b"x").expect("write");
            let file = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open");
            file.set_modified(UNIX_EPOCH + Duration::from_secs(index as u64 * 10))
                .expect("set mtime");
        }
        std::fs::create_dir(dir.join("sub")).expect("mkdir");

        prune_drop_dir(&dir, 2).expect("prune");

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "sub")
            .collect();
        left.sort();
        assert_eq!(left, vec!["d", "e"], "保留最新两份");
        assert!(dir.join("sub").is_dir(), "子目录不动");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `agent_package_drop_keep = 0` = 不清理。
    #[tokio::test]
    async fn prune_old_drops_is_a_noop_when_disabled() {
        let dir = temp_dir("agent-package-prune-off");
        for name in ["a", "b", "c"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        prune_old_drops(&dir, Some(0)).await;
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3, "0 = 不清理");
        let _ = std::fs::remove_dir_all(dir);
    }
}
