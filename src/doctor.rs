//! `wist-gwlinkd diagnose`：分项 OK/WARN/FAIL + 证据 + 下一步（照 `wist-agentd doctor` 范式）。
//!
//! **复用 [`crate::state`] 的判据**，不另写一套 —— 诊断与判定必须同源。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use orion_error::prelude::*;

use crate::config::Config;
use crate::error::{ConfigReason, ConfigResult};
use crate::state::{self, CredentialStatus};

/// 中心可达性探测超时（诊断是人手跑的，短超时即可）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// 单条检查的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

/// 一条诊断检查。
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub title: String,
    pub detail: String,
    pub hint: Option<String>,
}

impl Check {
    fn ok(name: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            title: title.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    fn warn(name: &str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            title: title.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    fn fail(
        name: &str,
        title: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            title: title.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// 一次诊断的报告。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// 最严重的一档（有 Fail 即 Fail，否则有 Warn 即 Warn，否则 Ok）。
    pub fn worst(&self) -> Status {
        if self.checks.iter().any(|c| c.status == Status::Fail) {
            Status::Fail
        } else if self.checks.iter().any(|c| c.status == Status::Warn) {
            Status::Warn
        } else {
            Status::Ok
        }
    }

    /// 按名字取一条检查（测试与外部消费用）。
    pub fn find(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.name == name)
    }
}

/// 运行全部检查：配置 → 信任锚 → 凭据 → 自述面（含**环回可达**）→ 执行器 → 升级态 → 中心可达。
pub fn diagnose(config: &Config) -> Report {
    let checks = vec![
        config_check(config),
        trust_check(config),
        credential_check(config),
        leftover_link_token_check(config),
        regist_token_check(config),
        self_endpoint_check(config),
        self_ca_check(config),
        self_reachable_check(config),
        upgrader_check(config),
        upgrade_project_check(config),
        upgrade_check(config),
        upgrade_tools_check(config),
        reachability_check(config),
    ];
    Report { checks }
}

/// 首跑置备遗留：有未消费的 RegistToken（link-upstream 成功、register 未成）。
/// 留着是好状态 —— 下次启动会**免接入券**直接重试注册。
fn regist_token_check(config: &Config) -> Check {
    match state::load_regist_token(&config.state_dir) {
        Some(_) => Check::warn(
            "credential.pending_regist",
            "有未完成的注册（待消费 RegistToken）",
            "上次 link-upstream 成功但 register 未完成；不会丢，下次启动会重试",
        )
        .with_hint("运行 `wist-gwlinkd run` 完成注册（**无需**再设接入券）"),
        None => Check::ok(
            "credential.pending_regist",
            "无待完成的注册",
            "无遗留 RegistToken",
        ),
    }
}

/// endpoint 形态（空 / 非法 scheme → 早退）。
fn config_check(config: &Config) -> Check {
    let endpoint = config.control_center_endpoint.trim();
    if endpoint.is_empty() {
        return Check::fail(
            "config.endpoint",
            "控制中心 endpoint 未配置",
            "control_center_endpoint 为空",
            "在 `gwlinkd-pub.toml` 填写 control_center_endpoint（或走网关「链接上级」页）",
        );
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        Check::ok("config.endpoint", "控制中心 endpoint 已配置", endpoint)
    } else {
        Check::warn(
            "config.endpoint",
            "控制中心 endpoint 缺少 scheme",
            format!("{endpoint}（应以 http:// 或 https:// 开头）"),
        )
        .with_hint("补成 https://… 或 http://…")
    }
}

/// 信任锚：文件在→必须可解析为 PEM 证书；不在→回落公共根（WARN）。
fn trust_check(config: &Config) -> Check {
    let path = config.trust_bundle.as_path();
    if !path.exists() {
        return Check::warn(
            "config.trust_bundle",
            "未找到信任锚文件，回落公共根",
            path.display().to_string(),
        )
        .with_hint("自签中心必须提供 trust_bundle（PEM）");
    }
    match validate_pem_certificate(path) {
        Ok(_) => Check::ok(
            "config.trust_bundle",
            "信任锚可解析",
            path.display().to_string(),
        ),
        Err(detail) => Check::fail(
            "config.trust_bundle",
            "信任锚无效",
            detail.to_string(),
            "换成合法的 PEM 证书文件",
        ),
    }
}

/// 读并校验一个 PEM 证书文件（存在为前提）：含 CERTIFICATE 块 + reqwest 可解析。
/// 错误串带上路径，便于诊断直接展示。
fn validate_pem_certificate(path: &Path) -> ConfigResult<()> {
    let pem = std::fs::read_to_string(path)
        .source_err(ConfigReason::Io, format!("{}", path.display()))?;
    if !pem.contains("-----BEGIN CERTIFICATE-----") {
        return Err(
            ConfigReason::Validation.err(format!("{}: 不含 CERTIFICATE 块", path.display()))
        );
    }
    reqwest::Certificate::from_pem(pem.as_bytes())
        .source_raw_err(ConfigReason::Validation, format!("{}", path.display()))?;
    Ok(())
}

/// 运行期凭据：损坏→FAIL，缺失→WARN（首跑），就位→看有效期。
///
/// 读的是**身份目录**（`identity_dir`）：它才是不可再生的那对（证书 + 私钥）。
///
/// 缺失那条的建议**按现状给**：endpoint 还没配就先让配它；已配好就不再说这件事 ——
/// `config.endpoint` 那条刚报过「已配置」，这里再让人去配只会把人带偏。
fn credential_check(config: &Config) -> Check {
    match state::credential_status(config.identity_dir()) {
        CredentialStatus::Present(credential) => {
            let remaining = credential.seconds_remaining();
            let lead = config.renew_lead_seconds.unwrap_or(3600);
            let not_after = credential
                .bundle
                .not_after
                .as_ref()
                .map(|expires| expires.to_chrono().to_rfc3339())
                .unwrap_or_else(|| "unknown".to_string());
            if remaining <= 0 {
                Check::warn(
                    "credential.local",
                    "客户端证书已过期",
                    format!("not_after={not_after}"),
                )
                .with_hint("等待自动轮换，或在中心重置该实例后重跑")
            } else if remaining <= lead {
                Check::warn(
                    "credential.local",
                    "客户端证书即将过期",
                    format!("还剩 {remaining}s（轮换提前量 {lead}s）"),
                )
            } else {
                Check::ok(
                    "credential.local",
                    "客户端证书有效",
                    format!("还剩 {remaining}s"),
                )
            }
        }
        CredentialStatus::Missing => {
            let hint = if config.control_center_endpoint.trim().is_empty() {
                "先在本机现场态（`gwlinkd-pub.toml` / 同目录 `.env`）配 `control_center_endpoint`，\
                 再走网关「链接上级」页提交接入链接（也可用 `link_token` + WIST_GWLINKD_LINK_TOKEN 离线预置）"
            } else {
                "走网关「链接上级」页提交接入链接（也可用 `link_token` + WIST_GWLINKD_LINK_TOKEN 离线预置）"
            };
            Check::warn(
                "credential.local",
                "尚无客户端身份",
                "首跑会走 link-upstream → register",
            )
            .with_hint(hint)
        }
        CredentialStatus::Corrupt(detail) => Check::fail(
            "credential.local",
            "客户端身份损坏",
            detail,
            "修复或删除**身份目录**下的 credential.json 后重跑；若中心已初始化该实例需先在中心重置",
        ),
    }
}

/// 现场态配置里是否残留**已消费**的一次性接入券。
///
/// 券用一次即废；接入成功后 gwlinkd 会把它删掉。若已持有长期身份却还留着它，只是一张明文废券
/// （不会自动重联 —— 有凭据就走 mTLS），故报 WARN 并提示删除。
fn leftover_link_token_check(config: &Config) -> Check {
    let Some(pub_path) = config.pub_config_path.as_deref() else {
        return Check::ok(
            "config.link_token",
            "未解析到现场态配置（跳过券检查）",
            "加载时未记录 gwlinkd-pub.toml 落点",
        );
    };
    let token = config
        .link_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match token {
        None => Check::ok(
            "config.link_token",
            "无残留接入券",
            pub_path.display().to_string(),
        ),
        Some(_)
            if matches!(
                state::credential_status(config.identity_dir()),
                CredentialStatus::Present(_)
            ) =>
        {
            Check::warn(
                "config.link_token",
                "已接入但现场态配置里仍留着一次性接入券",
                format!(
                    "{}（券一次性，接入成功即废；留着只是明文废券）",
                    pub_path.display()
                ),
            )
            .with_hint("删掉该文件里的 `link_token` 行（下次接入成功会自动删）")
        }
        Some(_) => Check::ok(
            "config.link_token",
            "有待消费的接入券（尚未接入）",
            pub_path.display().to_string(),
        ),
    }
}

/// 自述面 endpoint：**空串等同未配置**。
///
/// 配置里把它写成空串（`gateway_self_endpoint = ""` —— 模板没做非空判断、或手改过渲染产物）时，
/// 若照原样当「已配置」，新检查会拿空串拼出相对 URL 去探测，报一条「TCP/TLS 失败」的 FAIL
/// —— 与真因（根本没配）无关。故统一在这里收口：trim 后为空即视为未配。
fn configured_self_endpoint(config: &Config) -> Option<&str> {
    config
        .gateway_self_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
}

/// 自述面：未配→健康度恒为 unknown（WARN）；配了→OK。
fn self_endpoint_check(config: &Config) -> Check {
    match configured_self_endpoint(config) {
        Some(endpoint) => Check::ok("self.endpoint", "已配置网关自述面", endpoint),
        None => Check::warn(
            "self.endpoint",
            "未配置网关自述面",
            "gateway_self_endpoint 为空：上报的健康度恒为 unknown",
        )
        .with_hint("在 gwlinkd.toml 配 gateway_self_endpoint（如 https://127.0.0.1:3000）"),
    }
}

/// 环回面（self-state / link-request）信任锚：未配→用系统根（OK）；配了→**必须可解析**，
/// 否则真部署会够不到网关（与 `run` 启动即报错同源）。
fn self_ca_check(config: &Config) -> Check {
    let Some(path) = config.gateway_self_ca.as_deref() else {
        return Check::ok(
            "self.ca",
            "环回信任锚未配置（用系统根）",
            "gateway_self_ca 为空：网关非自签时适用",
        );
    };
    if !path.exists() {
        return Check::fail(
            "self.ca",
            "环回信任锚文件缺失",
            path.display().to_string(),
            "自签网关必须提供 gateway_self_ca（PEM）；或删除该项改用系统根",
        );
    }
    match validate_pem_certificate(path) {
        Ok(()) => Check::ok("self.ca", "环回信任锚可解析", path.display().to_string()),
        Err(detail) => Check::fail(
            "self.ca",
            "环回信任锚无效",
            detail.to_string(),
            "换成合法的 PEM 证书文件",
        ),
    }
}

/// 环回面可达：本机 gwlinkd **够不够得到本机网关**（`self-state` / `link-request` /
/// `linkd-status` / `agent-package` 共用同一个入口与信任锚）。
///
/// 为什么单列一条：`self.endpoint` / `self.ca` 只证明「配了、文件在」，**不证明连通** —— 而环回面
/// 在**容器部署**下可能把宿主自己的连接判成「外来」而全线 403（网关 < v0.11.0；见
/// `wist-design/doc/design/edge/gateway-status-report.md` §4）。那种故障的症状（心跳推不上去、
/// 页面报「未检测到 gwlinkd 心跳」、接入不了）看着像「中心配错」，其实卡在本机这一跳。
fn self_reachable_check(config: &Config) -> Check {
    let Some(endpoint) = configured_self_endpoint(config) else {
        // 没配自述面：本机这一跳不做（`self.endpoint` 已单独给 WARN），不是红。
        return Check::ok(
            "self.reachable",
            "未配置网关自述面（跳过探测）",
            "gateway_self_endpoint 为空",
        );
    };
    // 同 `center.reachable`：阻塞客户端**建/用/毁都放专用线程**，只把纯数据带回来。
    let probe = {
        let endpoint = endpoint.to_string();
        let trust = config.gateway_self_ca.clone();
        let gateway_id = config.gateway_id.clone();
        std::thread::spawn(move || probe_self(&endpoint, trust.as_deref(), &gateway_id))
            .join()
            .unwrap_or_else(|_| Probe::Transport("探测线程 panic".to_string()))
    };
    match probe {
        Probe::Response { status, .. } if (200..300).contains(&status) => Check::ok(
            "self.reachable",
            "网关环回面可达",
            format!("{endpoint}（HTTP {status}）"),
        ),
        // 403 **且**带着环回护栏的 code：自述面无鉴权，这说明「环回面**认不出本机**」
        // （容器部署下宿主那一跳）—— 正是本次真故障。
        // 为什么还要看 body：403 也可能来自前置代理 / 别的层，那种情形不能照抄这条结论。
        // （body 已按**展示**需要截断到 200 字符；`code` 在错误信封最前面，稳定落在窗口内。）
        Probe::Response { status, body } if status == 403 && body.contains("loopback_only") => {
            Check::fail(
                "self.reachable",
                "网关环回面拒绝了本机（把它判成了「外来」）",
                format!("HTTP {status}: {body}"),
                "容器部署需网关 ≥ v0.11.0（本机 = 环回 ∪ 容器默认网关）；核对网关启动日志 `event=LocalHostPeers`，\
                 必要时用 `[server] self_face_allow` 兑底",
            )
        }
        // 400 + 「缺 gateway_id」：自述面过了环回那关才校验该参数 —— 说明**本机这一跳是通的**，
        // 是本机 `gateway_id` 为空（首跑置备阶段可能如此）。结论要指向本机配置，
        // 而不是泛化成「网关返回非预期状态」（那样无从下手）。
        Probe::Response { status, body }
            if status == 400 && body.contains("self_state_missing_gateway_id") =>
        {
            Check::warn(
                "self.reachable",
                "网关环回面可达，但本机未提供 gateway_id",
                format!("HTTP {status}: {body}"),
            )
            .with_hint(
                "配本机 `gateway_id`（gwlinkd.toml，或现场态 `.env` 里的 `GWLINKD_GATEWAY_ID`）；\
                 自述面 / 状态上报都以它认人",
            )
        }
        Probe::Response { status, body } => {
            let detail = if body.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {body}")
            };
            Check::warn("self.reachable", "网关环回面可达但返回非预期状态", detail).with_hint(
                "确认 gateway_self_endpoint 指到网关、网关版本与期望一致；401/403 若不是环回护栏，\
                            看 detail 里的 code（可能中间有代理）",
            )
        }
        Probe::ClientBuild(reason) => Check::fail(
            "self.reachable",
            "无法构造环回探测客户端",
            reason,
            "检查 gateway_self_ca（PEM 信任锚）",
        ),
        Probe::Transport(reason) => Check::fail(
            "self.reachable",
            "网关环回面不可达（TCP/TLS 失败）",
            format!("{endpoint}: {reason}"),
            "核对 gateway_self_endpoint（地址 / 端口 / scheme）与 gateway_self_ca（自签网关必需）",
        ),
    }
}

/// 在**专用线程**里做一次阻塞探测：带环回信任锚，**不带客户端身份**
/// （自述面只看来源是不是本机，不需要凭据）。
fn probe_self(endpoint: &str, trust: Option<&Path>, gateway_id: &str) -> Probe {
    let client = match probe_client(trust, None) {
        Ok(client) => client,
        Err(probe) => return probe,
    };
    let url = format!(
        "{}/api/v1/gateway/self-state",
        endpoint.trim_end_matches('/')
    );
    probe_get(&client, url, gateway_id)
}

/// 升级执行器：能在 PATH / 给定路径解析到才算 OK（否则真升级必失败）。
fn upgrader_check(config: &Config) -> Check {
    let program = config
        .upgrader_program
        .as_deref()
        .unwrap_or(crate::executor::DEFAULT_UPGRADER_PROGRAM);
    match resolve_program(program) {
        Some(path) => Check::ok(
            "upgrader.program",
            "升级执行器可解析",
            path.display().to_string(),
        ),
        None => Check::fail(
            "upgrader.program",
            "升级执行器不可解析",
            format!("{program}（不在 PATH，也不是可执行文件）"),
            "安装 gops 或把 upgrader_program 指向绝对路径",
        ),
    }
}

/// 升级**工作目录**：执行器以它为 cwd（`GopsExecutor::command` 里 `current_dir`）。
///
/// 通用要求只有一条：**配了就得拿它当 cwd，所以它得是个真实存在的目录**。
/// `ops-prj.yml` 是 **gops 特有**的要求（`gops prj upgrade` 从 cwd 解析工程、**无**「指定工程」旗标）：
/// 缺 marker 时 gops 会在升级那刻以退出码 255 失败 —— 但那是执行器的要求，不是目录本身的问题，
/// 故这里只给 **WARN** 提示；运行期 [`crate::executor::GopsExecutor::preflight`] 仍会拒发（gops 确实跑不了）。
fn upgrade_project_check(config: &Config) -> Check {
    let program = config
        .upgrader_program
        .as_deref()
        .unwrap_or(crate::executor::DEFAULT_UPGRADER_PROGRAM);
    let is_gops = crate::executor::program_is_gops(program);
    match config.upgrade_project_dir.as_deref() {
        // 没配：gops 会跑到进程 cwd（不可预测）→ FAIL；非 gops 执行器不要求 cwd → OK。
        None if is_gops => Check::fail(
            "upgrade.project",
            "升级未配置工作目录",
            "upgrade_project_dir 未设置（gops 从 cwd 解析工程，缺失会让升级以退出码失败）",
            "把 upgrade_project_dir 指向执行器的工作目录（gops 需含 ops-prj.yml）",
        ),
        None => Check::ok(
            "upgrade.project",
            "非 gops 执行器：不校验工作目录",
            format!("upgrader_program = {program}"),
        ),
        // 配了、但目录不存在：任何执行器都没法拿它当 cwd → FAIL（通用错配，不是 gops 特有）。
        Some(dir) if !dir.is_dir() => Check::fail(
            "upgrade.project",
            "升级工作目录不存在",
            format!("{} 不是已存在的目录", dir.display()),
            "把 upgrade_project_dir 指到一个已存在的目录",
        ),
        // 目录在 = 通用要求满足；只有 gops 才额外要求它是运维工程根（含 ops-prj.yml）→ 只提示。
        Some(dir) if is_gops && !dir.join("ops-prj.yml").is_file() => Check::warn(
            "upgrade.project",
            "升级工作目录不是 gops 工程根",
            format!("{} 下找不到 ops-prj.yml", dir.display()),
        )
        .with_hint(
            "gops 升级时会拒掉：在工程根跑 `gops prj new` / `gops prj import`，或把 upgrade_project_dir 指对",
        ),
        Some(dir) => Check::ok(
            "upgrade.project",
            "升级工作目录已配置",
            if is_gops {
                format!("{}（含 ops-prj.yml）", dir.display())
            } else {
                format!("{}（执行器 {program} 不要求工程根）", dir.display())
            },
        ),
    }
}

/// 升级态检查：**复用 [`state::upgrader_is_declared_dead`]**（判据单来源）。
fn upgrade_check(config: &Config) -> Check {
    let now = SystemTime::now();
    match state::read_upgrade_record(&config.state_dir) {
        None => Check::ok("upgrade.local", "没有进行中的升级", "台账不存在"),
        // 终态（done / failed / rolled_back / unverified...）：上一次升级已结束 —— 别再说「进行中」。
        Some(record) if record.status != "running" => {
            let detail = format!("status={} step={}", record.status, record.step);
            if record.status == "done" {
                Check::ok("upgrade.local", "上次升级已成功结束", detail)
            } else {
                Check::warn("upgrade.local", "上次升级未成功结束", detail)
                    .with_hint("看执行器日志与台账；确认现场后到管理面重派升级")
            }
        }
        Some(_) if state::upgrader_is_declared_dead(&config.state_dir, now) => Check::fail(
            "upgrade.local",
            "升级器已失联（判定已死）",
            "有 running 升级但心跳已陈旧",
            "看升级器日志与那个瞬态 unit；确认当前版本后到管理面重派升级",
        ),
        Some(record) => Check::ok(
            "upgrade.local",
            "有升级正在进行（升级器心跳正常）",
            format!("step={}", record.step),
        ),
    }
}

/// 无状态工具（`install = "tool-copy"`）组件体检：目录、`binary` 配置与 `PATH` 命中。
///
/// 直接对着「表面修了其实没生效」的坑：配置里**没有** `tool-copy` 目录 → 升级会静默回退到 gops。
fn upgrade_tools_check(config: &Config) -> Check {
    use crate::config::UpgradeInstall;

    let tools: Vec<&crate::config::UpgradeComponentConfig> = config
        .upgrade
        .component
        .iter()
        .filter(|entry| entry.install == UpgradeInstall::ToolCopy)
        .collect();
    let require_arch = config.upgrade_tool_require_arch.unwrap_or(true);
    if tools.is_empty() {
        return Check::ok(
            "upgrade.tool",
            "无无状态工具（tool-copy）组件",
            format!("组件目录里没有 install=tool-copy 的项（require_arch={require_arch}）"),
        );
    }

    let mut lines = Vec::new();
    let mut has_bad_binary = false;
    let mut has_missing = false;
    for entry in &tools {
        let binary = entry.binary.as_deref().unwrap_or("").trim();
        if binary.is_empty() {
            has_bad_binary = true;
            lines.push(format!("{} → 未配 binary", entry.name));
            continue;
        }
        match crate::tool_install::which_binary(binary) {
            Some(path) => lines.push(format!("{} → {binary}（{}）", entry.name, path.display())),
            None => {
                has_missing = true;
                lines.push(format!("{} → {binary} 不在 PATH", entry.name));
            }
        }
    }
    lines.push(format!("require_arch={require_arch}"));
    let detail = lines.join("；");

    if has_bad_binary {
        Check::fail(
            "upgrade.tool",
            "无状态工具组件缺 binary",
            detail,
            "在 [[upgrade.component]] 给 install=tool-copy 的项补上 binary（如 galaxy-ops → gops）",
        )
    } else if has_missing {
        Check::warn("upgrade.tool", "无状态工具未装到 PATH", detail).with_hint(
            "tool-copy 就地覆盖 PATH 上的原二进制：先把工具装上，或把 binary 指向绝对路径",
        )
    } else {
        Check::ok(
            "upgrade.tool",
            format!("无状态工具组件可用（{} 个）", tools.len()),
            detail,
        )
    }
}

/// 一次阻塞探测的结局（在隔离线程里算完，只把纯数据带回来）。
///
/// **不带任何 reqwest 句柄回异步线程**：`reqwest::blocking` 的客户端自带运行时，
/// 在异步上下文里被 drop 会 panic（`Cannot drop a runtime in a context where blocking is not allowed`）。
enum Probe {
    /// 拿到 HTTP 响应（状态码 + 已截断的单行 body）。
    Response { status: u16, body: String },
    /// 建探测客户端失败（信任锚 / 凭据问题）。
    ClientBuild(String),
    /// 连接 / TLS 失败（scheme、网络、证书问题）。
    Transport(String),
}

/// 中心可达性：**真实打一次需要客户端证书的 GET**（`/api/v1/gateway/upgrade-plan`），
/// 一次验完「TCP / TLS / 证书被中心认」三关。
///
/// 为什么不是 TCP connect：TCP 通**不代表能用** —— 「`https://` 打到明文端口」这类
/// **协议/scheme 不匹配**只在 TLS 握手才暴露（rustls 报 `InvalidContentType`），
/// 只测 TCP 会给假 OK（中心其实一直在拒所有上报）。
fn reachability_check(config: &Config) -> Check {
    let endpoint = config.control_center_endpoint.trim();
    if endpoint.is_empty() {
        return Check::fail(
            "center.reachable",
            "控制中心未配置",
            "control_center_endpoint 为空",
            "先接入中心（网关「链接上级」页），或填 control_center_endpoint",
        );
    }
    // 同步 `diagnose` 要能从 `#[tokio::main]` 的异步上下文里真打一次接口；
    // 阻塞客户端自带运行时，**建/用/销毁都放到专用线程**做，只把纯数据带回来。
    // 本机是否已有客户端凭据，决定 401/403 该判 FAIL（证书被拒）还是 WARN（尚未注册）。
    let credential = state::load_credential(config.identity_dir());
    let has_identity = credential.is_some();
    // 探测用的 gateway_id：优先用**已注册凭据**里的身份（中心按客户端证书认人，query 必须与之相符）；
    // 没有凭据时回落配置里的 gateway_id。**只读一次凭据**（别在下面再读一遍）。
    let gateway_id = credential
        .as_ref()
        .map(|credential| credential.bundle.gateway_id.clone())
        .unwrap_or_else(|| config.gateway_id.clone());
    let probe = {
        let endpoint = endpoint.to_string();
        let trust = config.trust_bundle.clone();
        let identity = credential.map(|c| c.identity_pem());
        std::thread::spawn(move || probe_once(&endpoint, &trust, identity.as_deref(), &gateway_id))
            .join()
            .unwrap_or_else(|_| Probe::Transport("探测线程 panic".to_string()))
    };
    match probe {
        Probe::Response { status, .. } if (200..300).contains(&status) => Check::ok(
            "center.reachable",
            "控制中心可达（mTLS 已通过）",
            format!("{endpoint}（HTTP {status}）"),
        ),
        Probe::Response { status, body } => {
            let detail = if body.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status}: {body}")
            };
            if status == 401 || status == 403 {
                if has_identity {
                    Check::fail(
                        "center.reachable",
                        "控制中心可达但拒绝了本机身份",
                        detail,
                        "证书未注册 / 已失效 / 与 gateway_id 不符 → 到中心重置该实例后重新置备",
                    )
                } else {
                    // 本机还没有客户端证书：可达，只是**尚未注册**（首跑正是如此，不该判红）。
                    Check::warn(
                        "center.reachable",
                        "控制中心可达（TLS 通过），但本机尚未注册",
                        detail,
                    )
                    .with_hint("到网关「链接上级」页完成接入（link-upstream → register）")
                }
            } else {
                Check::warn("center.reachable", "控制中心可达但返回非预期状态", detail)
                    .with_hint("确认中心版本与网关协议一致（GET /api/v1/gateway/upgrade-plan）")
            }
        }
        Probe::ClientBuild(reason) => Check::fail(
            "center.reachable",
            "无法构造中心探测客户端",
            reason,
            "检查 trust_bundle 与客户端身份（credential.json）",
        ),
        Probe::Transport(reason) => Check::fail(
            "center.reachable",
            "控制中心不可达（TCP/TLS 失败）",
            format!("{endpoint}: {reason}"),
            "核对 scheme：中心开了 TLS 用 https、明文用 http；再查网络 / 证书 / 端口",
        ),
    }
}

/// 在**专用线程**里做一次阻塞探测：带信任锚（自签必需）与客户端身份（有凭据时），
/// 打一次需要客户端证书的 GET。
fn probe_once(endpoint: &str, trust: &Path, identity_pem: Option<&str>, gateway_id: &str) -> Probe {
    let client = match probe_client(Some(trust), identity_pem) {
        Ok(client) => client,
        Err(probe) => return probe,
    };
    let url = format!(
        "{}/api/v1/gateway/upgrade-plan",
        endpoint.trim_end_matches('/')
    );
    probe_get(&client, url, gateway_id)
}

/// 建阻塞探测客户端：信任锚（**存在才加** —— 没有就回落系统根）+ 可选客户端身份。
fn probe_client(
    trust: Option<&Path>,
    identity_pem: Option<&str>,
) -> Result<reqwest::blocking::Client, Probe> {
    let mut builder = reqwest::blocking::Client::builder().timeout(PROBE_TIMEOUT);
    if let Some(path) = trust.filter(|path| path.exists()) {
        let pem = std::fs::read(path).map_err(|err| {
            Probe::ClientBuild(format!("读取信任锚 {} 失败: {err}", path.display()))
        })?;
        let cert = reqwest::Certificate::from_pem(&pem).map_err(|err| {
            Probe::ClientBuild(format!("解析信任锚 {} 失败: {err}", path.display()))
        })?;
        builder = builder.add_root_certificate(cert);
    }
    if let Some(pem) = identity_pem {
        let identity = reqwest::Identity::from_pem(pem.as_bytes())
            .map_err(|err| Probe::ClientBuild(format!("加载客户端证书/私钥失败: {err}")))?;
        builder = builder.identity(identity);
    }
    builder
        .build()
        .map_err(|err| Probe::ClientBuild(format!("建探测客户端失败: {err}")))
}

/// 打一次 GET（带 `gateway_id` 查询），把响应/传输失败归一成 [`Probe`]。
fn probe_get(client: &reqwest::blocking::Client, url: String, gateway_id: &str) -> Probe {
    match client.get(url).query(&[("gateway_id", gateway_id)]).send() {
        Ok(response) => {
            let status = response.status().as_u16();
            let body = truncate_one_line(&response.text().unwrap_or_default(), 200);
            Probe::Response { status, body }
        }
        Err(err) => Probe::Transport(error_chain_one_line(&err)),
    }
}

/// 把错误连同**源链**压成一行（reqwest 的 `Display` 只有顶层，真因在 `source()` 里 ——
/// 「TLS 证书不认」「明文口」这类关键信息都在链上）。
fn error_chain_one_line(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut cursor = err.source();
    while let Some(source) = cursor {
        parts.push(source.to_string());
        cursor = source.source();
    }
    truncate_one_line(&parts.join(" -> "), 300)
}

/// 压成单行并截断（诊断展示用；错误串 / 响应体里的换行会让输出难读）。
fn truncate_one_line(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}…")
    }
}

/// 解析可执行程序：含路径分隔符→按文件查；否则扫 PATH。
fn resolve_program(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if program.contains('/') {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-doctor-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn config(dir: &std::path::Path) -> Config {
        Config {
            control_center_endpoint: "https://center.example".into(),
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
            upgrader_program: Some("/bin/sh".into()),
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

    fn status_of(report: &Report, name: &str) -> Status {
        report.find(name).expect("check exists").status
    }

    #[test]
    fn an_empty_endpoint_is_a_failure() {
        let dir = temp_dir("empty");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = "   ".into();
        assert_eq!(status_of(&diagnose(&cfg), "config.endpoint"), Status::Fail);
    }

    #[test]
    fn a_bad_scheme_is_a_warning() {
        let dir = temp_dir("scheme");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = "center.example".into();
        assert_eq!(status_of(&diagnose(&cfg), "config.endpoint"), Status::Warn);
    }

    #[test]
    fn a_missing_credential_is_a_warning_not_a_failure() {
        let dir = temp_dir("cred");
        assert_eq!(
            status_of(&diagnose(&config(&dir)), "credential.local"),
            Status::Warn
        );
    }

    /// 缺失身份的建议不该让人去配**已经配好**的 endpoint（上一行刚报过「已配置」）。
    #[test]
    fn a_missing_credential_hint_skips_an_endpoint_that_is_already_configured() {
        let dir = temp_dir("cred-hint-endpoint");
        let report = diagnose(&config(&dir));
        let hint = report
            .find("credential.local")
            .expect("check")
            .hint
            .clone()
            .expect("hint");
        assert!(!hint.contains("control_center_endpoint"), "{hint}");
        assert!(hint.contains("链接上级"), "{hint}");
    }

    /// endpoint 真缺的时候，建议还是要先把它配上。
    #[test]
    fn a_missing_credential_hint_asks_for_the_endpoint_when_it_is_missing() {
        let dir = temp_dir("cred-hint-no-endpoint");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = "  ".into();
        let report = diagnose(&cfg);
        let hint = report
            .find("credential.local")
            .expect("check")
            .hint
            .clone()
            .expect("hint");
        assert!(hint.contains("control_center_endpoint"), "{hint}");
    }

    /// 造一份就位的长期身份（落在**身份目录**）。
    fn write_credential(identity_dir: &Path) {
        let credential = state::StoredCredential {
            bundle: wist_control::GatewayCredentialBundle {
                credential_id: "cred-test".into(),
                gateway_id: "gw-1".into(),
                instance_id: None,
                certificate: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                    .into(),
                ca_bundle: None,
                issued_at: wist_control::DateTime::from_rfc3339("2026-10-04T00:00:00Z")
                    .expect("rfc3339"),
                not_before: None,
                not_after: Some(
                    wist_control::DateTime::from_rfc3339("2099-01-01T00:00:00Z").expect("rfc3339"),
                ),
            },
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nMIIB\n-----END PRIVATE KEY-----\n"
                .into(),
        };
        state::save_credential(identity_dir, &credential).expect("credential");
    }

    /// 已接入（持长期身份）却还在现场态配置里留着一次性接入券 → WARN（废券明文）。
    #[test]
    fn an_already_linked_host_with_a_leftover_link_token_warns() {
        let dir = temp_dir("leftover-token");
        let mut cfg = config(&dir);
        cfg.pub_config_path = Some(dir.join("gwlinkd-pub.toml"));
        cfg.link_token = Some("link_abc".into());
        write_credential(&dir);
        assert_eq!(
            status_of(&diagnose(&cfg), "config.link_token"),
            Status::Warn
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 尚无身份时的接入券是**待消费**（正常）→ OK。
    #[test]
    fn a_link_token_without_a_credential_is_fine() {
        let dir = temp_dir("pending-token");
        let mut cfg = config(&dir);
        cfg.pub_config_path = Some(dir.join("gwlinkd-pub.toml"));
        cfg.link_token = Some("link_abc".into());
        assert_eq!(status_of(&diagnose(&cfg), "config.link_token"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 未记录落点时跳过（不误报）。
    #[test]
    fn a_leftover_link_token_check_skips_without_a_pub_path() {
        let dir = temp_dir("no-pub-path");
        let cfg = config(&dir);
        assert_eq!(status_of(&diagnose(&cfg), "config.link_token"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pending_regist_token_is_a_warning() {
        let dir = temp_dir("regist");
        let cfg = config(&dir);
        assert_eq!(
            status_of(&diagnose(&cfg), "credential.pending_regist"),
            Status::Ok
        );
        state::save_regist_token(&dir, "reg_abc").expect("save");
        assert_eq!(
            status_of(&diagnose(&cfg), "credential.pending_regist"),
            Status::Warn
        );
    }

    #[test]
    fn an_unparseable_trust_bundle_fails() {
        let dir = temp_dir("trust");
        let cfg = config(&dir);
        std::fs::write(&cfg.trust_bundle, b"not a pem").expect("write");
        assert_eq!(
            status_of(&diagnose(&cfg), "config.trust_bundle"),
            Status::Fail
        );
    }

    #[test]
    fn a_missing_self_endpoint_is_a_warning() {
        let dir = temp_dir("self");
        assert_eq!(
            status_of(&diagnose(&config(&dir)), "self.endpoint"),
            Status::Warn
        );
    }

    #[test]
    fn an_unconfigured_self_ca_is_ok_and_a_missing_one_fails() {
        let dir = temp_dir("self-ca");
        let mut cfg = config(&dir);
        // 未配 → 用系统根，OK。
        assert_eq!(status_of(&diagnose(&cfg), "self.ca"), Status::Ok);
        // 配了但文件不在 → FAIL（真部署会够不到网关）。
        cfg.gateway_self_ca = Some(dir.join("self-ca.pem"));
        assert_eq!(status_of(&diagnose(&cfg), "self.ca"), Status::Fail);
        // 配了但不是合法 PEM → FAIL。
        std::fs::write(dir.join("self-ca.pem"), b"not a pem").expect("write");
        assert_eq!(status_of(&diagnose(&cfg), "self.ca"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unresolvable_upgrader_program_fails() {
        let dir = temp_dir("upgrader");
        let mut cfg = config(&dir);
        cfg.upgrader_program = Some("/nonexistent/gops-xyz".into());
        assert_eq!(status_of(&diagnose(&cfg), "upgrader.program"), Status::Fail);
    }

    #[test]
    fn an_unconfigured_upgrade_project_fails_for_the_gops_executor() {
        let dir = temp_dir("proj");
        let mut cfg = config(&dir);
        cfg.upgrader_program = None; // 缺省 = gops
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_gops_work_dir_without_the_ops_prj_marker_warns() {
        let dir = temp_dir("proj-marker");
        let mut cfg = config(&dir);
        cfg.upgrader_program = Some("gops".into());
        cfg.upgrade_project_dir = Some(dir.join("prj"));
        std::fs::create_dir_all(dir.join("prj")).expect("mkdir");
        // 目录在、但没 ops-prj.yml → WARN（gops 特有要求，不当红；运行期 preflight 才拒发）。
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Warn);
        std::fs::write(dir.join("prj").join("ops-prj.yml"), "kind: ops\n").expect("write");
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_work_dir_fails_for_any_executor() {
        let dir = temp_dir("proj-missing");
        let mut cfg = config(&dir);
        // 配了但目录不存在 → FAIL（通用错配：拿它当 cwd 会直接失败）。
        cfg.upgrader_program = Some("gops".into());
        cfg.upgrade_project_dir = Some(dir.join("nope"));
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Fail);
        // 非 gops 执行器同理：目录得存在，它才会被当作 cwd。
        cfg.upgrader_program = Some("/bin/sh".into());
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_non_gops_executor_only_needs_an_existing_dir() {
        let dir = temp_dir("proj-non-gops");
        let mut cfg = config(&dir);
        cfg.upgrader_program = Some("/bin/sh".into());
        std::fs::create_dir_all(dir.join("work")).expect("mkdir");
        // 目录在就够了，不要求 ops-prj.yml。
        cfg.upgrade_project_dir = Some(dir.join("work"));
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_non_gops_upgrader_skips_the_project_check() {
        let dir = temp_dir("proj-skip");
        let cfg = config(&dir); // upgrader_program = /bin/sh
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.project"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dead_upgrade_fails_and_a_live_one_does_not() {
        let dir = temp_dir("upg");
        let cfg = config(&dir);
        crate::state::write_upgrade_record(
            &dir,
            &crate::state::UpgradeRecord {
                work_id: "w-1".into(),
                from_version: "0.1.0".into(),
                to_version: "0.1.16".into(),
                step: "restart".into(),
                status: "running".into(),
                detail: String::new(),
                log_dir: None,
                task_id: None,
            },
        )
        .expect("record");
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.local"), Status::Fail);
        crate::state::touch_heartbeat(&dir).expect("heartbeat");
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.local"), Status::Ok);
    }

    #[test]
    fn a_finished_upgrade_is_not_reported_as_in_progress() {
        let dir = temp_dir("upg-finished");
        let cfg = config(&dir);

        // done：绿，且不再说「进行中」。
        crate::state::write_upgrade_record(
            &dir,
            &crate::state::UpgradeRecord {
                work_id: "w-1".into(),
                from_version: "0.1.0".into(),
                to_version: "0.1.16".into(),
                step: "verify".into(),
                status: "done".into(),
                detail: String::new(),
                log_dir: None,
                task_id: None,
            },
        )
        .expect("record");
        let check = diagnose(&cfg);
        let upgrade = check.find("upgrade.local").expect("check");
        assert_eq!(upgrade.status, Status::Ok);
        assert!(!upgrade.title.contains("正在进行"), "{}", upgrade.title);

        // 非 done 的终态：提示（Warn），但仍不判死、不说过行中。
        crate::state::write_upgrade_record(
            &dir,
            &crate::state::UpgradeRecord {
                work_id: "w-2".into(),
                from_version: "0.1.0".into(),
                to_version: "0.1.16".into(),
                step: "verify".into(),
                status: "unverified".into(),
                detail: String::new(),
                log_dir: None,
                task_id: None,
            },
        )
        .expect("record");
        let report = diagnose(&cfg);
        let upgrade = report.find("upgrade.local").expect("check");
        assert_eq!(upgrade.status, Status::Warn);
        assert!(!upgrade.title.contains("正在进行"), "{}", upgrade.title);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 起一个一次性 TCP 服务：accept 后回一段**原始字节**。
    ///
    /// 用它能造两种场景：真 HTTP 200（可达）、或往 TLS 端口回明文（复刻 `InvalidContentType`）。
    fn one_shot_raw_server(reply: impl AsRef<[u8]> + Send + 'static) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let reply = reply.as_ref().to_vec();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // 读走 ClientHello / 请求，再回
                let _ = stream.write_all(&reply);
                let _ = stream.flush();
            }
        });
        addr
    }

    /// 造一份 HTTP/1.1 响应。`content-length` **按实际 body 字节数算** ——
    /// 手写错（长短不一）会让 reqwest 读到 EOF 报错，测试就变成在测别的分支了。
    fn raw_http_response(status_line: &str, content_type: Option<&str>, body: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status_line}\r\n");
        if let Some(content_type) = content_type {
            out.push_str(&format!("content-type: {content_type}\r\n"));
        }
        out.push_str(&format!("content-length: {}\r\n\r\n{body}", body.len()));
        out.into_bytes()
    }

    #[test]
    fn a_reachable_center_endpoint_is_ok() {
        let dir = temp_dir("reach");
        let addr = one_shot_raw_server(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = format!("http://{addr}");
        assert_eq!(status_of(&diagnose(&cfg), "center.reachable"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 复刻本次线上问题：gwlinkd 用 `https://` 打一个**明文**端口 —— 必须是 FAIL，
    /// 而不是旧实现那种「TCP 连上了就 OK」的假绿。
    #[test]
    fn an_https_endpoint_against_a_plaintext_port_fails() {
        let dir = temp_dir("reach-tls");
        let addr = one_shot_raw_server(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = format!("https://{addr}");
        assert_eq!(status_of(&diagnose(&cfg), "center.reachable"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_center_port_fails() {
        let dir = temp_dir("reach-refused");
        // 绑一个口拿到地址后立即释放 —— 几乎不可能还在监听。
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = format!("http://{addr}");
        assert_eq!(status_of(&diagnose(&cfg), "center.reachable"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 本机**还没有**客户端证书（尚未注册）时，中心的 401 是**预期**的 —— 判 WARN，
    /// 而不是「身份被拒」的红（首跑就该是黄）。
    #[test]
    fn a_center_rejecting_an_unregistered_gateway_is_a_warning() {
        let dir = temp_dir("reach-401");
        let addr = one_shot_raw_server(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = format!("http://{addr}");
        assert_eq!(status_of(&diagnose(&cfg), "center.reachable"), Status::Warn);
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── 网关环回面可达（容器部署下「宿主判成外来」的眞故障） ──

    /// 环回面可达：自述面 200 → OK。
    #[test]
    fn a_reachable_gateway_loopback_face_is_ok() {
        let dir = temp_dir("self-reach");
        let addr = one_shot_raw_server(raw_http_response("200 OK", None, "{}"));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        assert_eq!(status_of(&diagnose(&cfg), "self.reachable"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **本次线上故障的复刻**：环回面把宿主判成「外来」而 403 —— 必须 FAIL，且提示要能**照做**
    /// （不只是「环回面拒绝」，而是「容器部署需网关 ≥ v0.11.0」）。
    #[test]
    fn a_loopback_only_rejection_is_a_failure_with_an_actionable_hint() {
        let dir = temp_dir("self-reach-403");
        let addr = one_shot_raw_server(raw_http_response(
            "403 Forbidden",
            Some("application/json"),
            r#"{"error":{"code":"self_state_loopback_only","message":"self-state is loopback-only"}}"#,
        ));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        let report = diagnose(&cfg);
        let check = report.find("self.reachable").expect("check present");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        let hint = check.hint.clone().unwrap_or_default();
        assert!(hint.contains("v0.11.0"), "{hint}");
        assert!(hint.contains("LocalHostPeers"), "{hint}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 端口不可达（网关挂了 / 地址写错）→ FAIL。
    #[test]
    fn a_refused_gateway_loopback_port_fails() {
        let dir = temp_dir("self-reach-refused");
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr");
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        assert_eq!(status_of(&diagnose(&cfg), "self.reachable"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 没配自述面就**不探**（`self.endpoint` 已经单独给 WARN），不是红。
    #[test]
    fn an_unset_self_endpoint_skips_the_probe() {
        let dir = temp_dir("self-reach-unset");
        let cfg = config(&dir);
        assert!(cfg.gateway_self_endpoint.is_none(), "夹具前提");
        assert_eq!(status_of(&diagnose(&cfg), "self.reachable"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **空串**等同未配置：别把它当「已配置」（否则会拿空串拼出相对 URL，报一条与本意无关的失败）。
    #[test]
    fn a_blank_self_endpoint_is_treated_as_unset() {
        let dir = temp_dir("self-reach-blank");
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some("   ".into());
        let report = diagnose(&cfg);
        assert_eq!(status_of(&report, "self.endpoint"), Status::Warn);
        assert_eq!(status_of(&report, "self.reachable"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 403 也**可能来自前置代理 / 别的层** —— 那种 403 不带环回护栏的 code，
    /// 不能照抄「容器部署需网关 ≥ v0.11.0」那条结论，只判 WARN。
    #[test]
    fn a_foreign_403_without_the_loopback_marker_is_only_a_warning() {
        let dir = temp_dir("self-reach-403-proxy");
        let addr = one_shot_raw_server(raw_http_response(
            "403 Forbidden",
            Some("application/json"),
            r#"{"error":{"code":"forbidden","message":"blocked by proxy"}}"#,
        ));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        let report = diagnose(&cfg);
        let check = report.find("self.reachable").expect("check present");
        assert_eq!(check.status, Status::Warn, "{check:?}");
        let hint = check.hint.clone().unwrap_or_default();
        assert!(!hint.contains("v0.11.0"), "普通 403 不该赖网关版本：{hint}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 本机 `gateway_id` 为空：网关自述面过环回那关后才回 400（`self_state_missing_gateway_id`）
    /// —— 结论要指向**本机配置**。
    #[test]
    fn a_missing_gateway_id_rejection_points_at_the_local_config() {
        let dir = temp_dir("self-reach-no-gateway-id");
        let addr = one_shot_raw_server(raw_http_response(
            "400 Bad Request",
            Some("application/json"),
            r#"{"error":{"code":"self_state_missing_gateway_id","message":"missing gateway_id"}}"#,
        ));
        let mut cfg = config(&dir);
        cfg.gateway_id = String::new();
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        let report = diagnose(&cfg);
        let check = report.find("self.reachable").expect("check present");
        assert_eq!(check.status, Status::Warn, "{check:?}");
        let hint = check.hint.clone().unwrap_or_default();
        assert!(hint.contains("gateway_id"), "{hint}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `https://` 打**明文**端口（scheme 不匹配）→ FAIL（不是「TCP 连上了就 OK」）。
    #[test]
    fn an_https_self_endpoint_against_a_plaintext_port_fails() {
        let dir = temp_dir("self-reach-tls");
        let addr = one_shot_raw_server(raw_http_response("400 Bad Request", None, ""));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("https://{addr}"));
        assert_eq!(status_of(&diagnose(&cfg), "self.reachable"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 环回信任锚**存在但读不了** → FAIL（构造探测客户端就失败），且 hint 要指向该项。
    ///
    /// 为什么用「目录」而不是 `chmod 000`：`Certificate::from_pem` 对垃圾内容是**容忍**的
    /// （返回 Ok、拿去探测照样 200），拿它造不出这个分支；而权限位对 root 无效（CI 可能是 root）。
    /// 路径存在、`fs::read` 必失败（EISDIR）—— 平台无关且不怕 root。
    #[test]
    fn an_unreadable_self_trust_anchor_fails_the_probe() {
        let dir = temp_dir("self-reach-bad-ca");
        let ca = dir.join("ca-as-dir");
        std::fs::create_dir(&ca).expect("mkdir ca");
        let addr = one_shot_raw_server(raw_http_response("200 OK", None, "{}"));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        cfg.gateway_self_ca = Some(ca);
        let report = diagnose(&cfg);
        let check = report.find("self.reachable").expect("check present");
        assert_eq!(check.status, Status::Fail, "{check:?}");
        let hint = check.hint.clone().unwrap_or_default();
        assert!(hint.contains("gateway_self_ca"), "{hint}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 2xx 都算可达（这里 204 无 body）—— 判据是**区间**，不是「恰好 200」。
    #[test]
    fn any_2xx_from_the_gateway_loopback_face_counts_as_reachable() {
        let dir = temp_dir("self-reach-204");
        let addr = one_shot_raw_server(raw_http_response("204 No Content", None, ""));
        let mut cfg = config(&dir);
        cfg.gateway_self_endpoint = Some(format!("http://{addr}"));
        let report = diagnose(&cfg);
        let check = report.find("self.reachable").expect("check present");
        assert_eq!(check.status, Status::Ok, "{check:?}");
        assert!(check.detail.contains("204"), "{:?}", check.detail);
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── 无状态工具（tool-copy）组件体检 ──

    use crate::config::{UpgradeComponentConfig, UpgradeInstall};

    fn tool_component(name: &str, binary: Option<&str>) -> UpgradeComponentConfig {
        UpgradeComponentConfig {
            name: name.into(),
            install: UpgradeInstall::ToolCopy,
            binary: binary.map(str::to_string),
        }
    }

    #[test]
    fn no_tool_copy_components_is_ok() {
        let dir = temp_dir("tool-none");
        let cfg = config(&dir);
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.tool"), Status::Ok);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tool_copy_component_without_a_binary_fails() {
        let dir = temp_dir("tool-nobinary");
        let mut cfg = config(&dir);
        cfg.upgrade.component = vec![tool_component("galaxy-ops", None)];
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.tool"), Status::Fail);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tool_copy_component_missing_from_path_warns() {
        let dir = temp_dir("tool-missing");
        let mut cfg = config(&dir);
        // 绝对路径但不存在 → 定位不到（与安装前置同一判据）。
        cfg.upgrade.component = vec![tool_component("galaxy-flow", Some("/nonexistent/gx"))];
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.tool"), Status::Warn);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tool_copy_component_present_on_path_is_ok_and_reports_arch_policy() {
        let dir = temp_dir("tool-ok");
        let bin = dir.join("gops");
        std::fs::write(&bin, b"x").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let mut cfg = config(&dir);
        cfg.upgrade.component = vec![tool_component("galaxy-ops", bin.to_str())];
        let report = diagnose(&cfg);
        let check = report.find("upgrade.tool").expect("check");
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("galaxy-ops"), "{}", check.detail);
        assert!(
            check.detail.contains("require_arch=true"),
            "{}",
            check.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_tool_check_reports_the_relaxed_arch_policy() {
        let dir = temp_dir("tool-relaxed");
        let mut cfg = config(&dir);
        cfg.upgrade_tool_require_arch = Some(false);
        let report = diagnose(&cfg);
        let check = report.find("upgrade.tool").expect("check");
        assert!(
            check.detail.contains("require_arch=false"),
            "{}",
            check.detail
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
