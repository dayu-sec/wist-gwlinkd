//! `wist-gwlinkd diagnose`：分项 OK/WARN/FAIL + 证据 + 下一步（照 `wist-agentd doctor` 范式）。
//!
//! **复用 [`crate::state`] 的判据**，不另写一套 —— 诊断与判定必须同源。

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
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

/// 运行全部检查：配置 → 信任锚 → 凭据 → 自述面 → 执行器 → 升级态 → 中心可达。
pub fn diagnose(config: &Config) -> Report {
    let checks = vec![
        config_check(config),
        trust_check(config),
        credential_check(config),
        regist_token_check(config),
        self_endpoint_check(config),
        upgrader_check(config),
        upgrade_check(config),
        reachability_check(config),
    ];
    Report { checks }
}

/// 首跑置备遗留：有未消费的 RegistToken（link-upstream 成功、register 未成）。
/// 留着是好状态 —— 下次启动会**免 bootstrap** 直接重试注册。
fn regist_token_check(config: &Config) -> Check {
    match state::load_regist_token(&config.state_dir) {
        Some(_) => Check::warn(
            "credential.pending_regist",
            "有未完成的注册（待消费 RegistToken）",
            "上次 link-upstream 成功但 register 未完成；不会丢，下次启动会重试",
        )
        .with_hint("运行 `wist-gwlinkd run` 完成注册（**无需**再设 bootstrap token）"),
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
            "在 gwlinkd.toml 填写 control_center_endpoint",
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
    match std::fs::read_to_string(path) {
        Ok(pem) if !pem.contains("-----BEGIN CERTIFICATE-----") => Check::fail(
            "config.trust_bundle",
            "信任锚不是 PEM 证书",
            format!("{}: 不含 CERTIFICATE 块", path.display()),
            "换成合法的 PEM 证书文件",
        ),
        Ok(pem) => match reqwest::Certificate::from_pem(pem.as_bytes()) {
            Ok(_) => Check::ok(
                "config.trust_bundle",
                "信任锚可解析",
                path.display().to_string(),
            ),
            Err(err) => Check::fail(
                "config.trust_bundle",
                "信任锚无法解析",
                format!("{}: {err}", path.display()),
                "换成合法的 PEM 证书文件",
            ),
        },
        Err(err) => Check::fail(
            "config.trust_bundle",
            "信任锚不可读",
            format!("{}: {err}", path.display()),
            "检查文件权限",
        ),
    }
}

/// 运行期凭据：损坏→FAIL，缺失→WARN（首跑），就位→看有效期。
fn credential_check(config: &Config) -> Check {
    match state::credential_status(&config.state_dir) {
        CredentialStatus::Present(credential) => {
            let remaining = credential.seconds_remaining();
            let lead = config.renew_lead_seconds.unwrap_or(3600);
            let not_after = credential
                .bundle
                .not_after
                .clone()
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
        CredentialStatus::Missing => Check::warn(
            "credential.local",
            "尚无客户端身份",
            "首跑会走 link-upstream → register",
        )
        .with_hint("设 WIST_GWLINKD_BOOTSTRAP_TOKEN 后运行 `wist-gwlinkd run` 完成首次置备"),
        CredentialStatus::Corrupt(detail) => Check::fail(
            "credential.local",
            "客户端身份损坏",
            detail,
            "修复或删除 state/credential.json 后重跑；若中心已初始化该实例需先在中心重置",
        ),
    }
}

/// 自述面：未配→健康度恒为 unknown（WARN）；配了→OK。
fn self_endpoint_check(config: &Config) -> Check {
    match config.gateway_self_endpoint.as_deref() {
        Some(endpoint) => Check::ok("self.endpoint", "已配置网关自述面", endpoint),
        None => Check::warn(
            "self.endpoint",
            "未配置网关自述面",
            "gateway_self_endpoint 为空：上报的健康度恒为 unknown",
        )
        .with_hint("在 gwlinkd.toml 配 gateway_self_endpoint（如 https://127.0.0.1:3000）"),
    }
}

/// 升级执行器：能在 PATH / 给定路径解析到才算 OK（否则真升级必失败）。
fn upgrader_check(config: &Config) -> Check {
    let program = config
        .upgrader_program
        .as_deref()
        .unwrap_or(crate::upgrade::DEFAULT_UPGRADER_PROGRAM);
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

/// 升级态检查：**复用 [`state::upgrader_is_declared_dead`]**（判据单来源）。
fn upgrade_check(config: &Config) -> Check {
    let now = SystemTime::now();
    match state::read_upgrade_record(&config.state_dir) {
        None => Check::ok("upgrade.local", "没有进行中的升级", "台账不存在"),
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

/// 中心可达性：解析 endpoint → TCP 连通。
fn reachability_check(config: &Config) -> Check {
    let endpoint = config.control_center_endpoint.trim();
    let (host, port) = match parse_host_port(endpoint) {
        Ok(parts) => parts,
        Err(err) => {
            return Check::fail(
                "center.reachable",
                "控制中心地址无法解析",
                err,
                "检查 control_center_endpoint",
            );
        }
    };
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
                Ok(_) => Check::ok(
                    "center.reachable",
                    "TCP 可达控制中心",
                    format!("已连上 {addr}"),
                ),
                Err(err) => Check::fail(
                    "center.reachable",
                    "控制中心不可达",
                    format!("连接 {addr} 失败: {err}"),
                    "检查网络/防火墙/中心是否在跑",
                ),
            },
            None => Check::fail(
                "center.reachable",
                "控制中心地址无法解析",
                format!("{host} 无地址记录"),
                "检查 DNS",
            ),
        },
        Err(err) => Check::fail(
            "center.reachable",
            "控制中心地址无法解析",
            format!("{host}: {err}"),
            "检查 DNS / /etc/hosts",
        ),
    }
}

/// 从 endpoint 解析 `(host, port)`：缺端口按 scheme 取默认（http=80 / https=443）。
fn parse_host_port(endpoint: &str) -> Result<(String, u16), String> {
    let (default_port, rest) = if let Some(rest) = endpoint.strip_prefix("https://") {
        (443, rest)
    } else if let Some(rest) = endpoint.strip_prefix("http://") {
        (80, rest)
    } else {
        return Err(format!("{endpoint} 缺少 http(s):// scheme"));
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err(format!("{endpoint} 缺少主机名"));
    }
    // IPv6 字面量（[::1]:port）也要能吃下。
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("{authority} 方括号不配对"))?;
        let port = match tail.strip_prefix(':') {
            Some(value) => value.parse().map_err(|_| format!("{authority} 端口非法"))?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((
            host.to_string(),
            port.parse().map_err(|_| format!("{authority} 端口非法"))?,
        )),
        None => Ok((authority.to_string(), default_port)),
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
    use std::net::TcpListener;

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
            gateway_id: "gw-1".into(),
            gateway_self_endpoint: None,
            renew_lead_seconds: None,
            upgrader_program: Some("/bin/sh".into()),
            upgrade_on_failure: None,
            upgrade_project_dir: None,
            upgrade_project_name: None,
            upgrade_retry_on_dead: None,
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
    fn an_unresolvable_upgrader_program_fails() {
        let dir = temp_dir("upgrader");
        let mut cfg = config(&dir);
        cfg.upgrader_program = Some("/nonexistent/gops-xyz".into());
        assert_eq!(status_of(&diagnose(&cfg), "upgrader.program"), Status::Fail);
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
            },
        )
        .expect("record");
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.local"), Status::Fail);
        crate::state::touch_heartbeat(&dir).expect("heartbeat");
        assert_eq!(status_of(&diagnose(&cfg), "upgrade.local"), Status::Ok);
    }

    #[test]
    fn a_reachable_center_endpoint_is_ok() {
        let dir = temp_dir("reach");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = format!("http://{addr}");
        assert_eq!(status_of(&diagnose(&cfg), "center.reachable"), Status::Ok);
        drop(listener);
    }

    #[test]
    fn parse_host_port_handles_scheme_defaults_and_ipv6() {
        assert_eq!(
            parse_host_port("https://c.example").unwrap(),
            ("c.example".to_string(), 443)
        );
        assert_eq!(
            parse_host_port("http://c.example:8080/x").unwrap(),
            ("c.example".to_string(), 8080)
        );
        assert_eq!(
            parse_host_port("http://[::1]:9000").unwrap(),
            ("::1".to_string(), 9000)
        );
        assert!(parse_host_port("c.example").is_err());
    }
}
