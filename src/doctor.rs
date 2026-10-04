//! `wist-gwlinkd diagnose`：分项 OK/WARN/FAIL + 证据 + 下一步（照 `wist-agentd doctor` 范式）。
//!
//! **复用 [`crate::state`] 的判据**，不另写一套 —— 诊断与判定必须同源。

use std::time::SystemTime;

use crate::config::Config;
use crate::state;

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
}

/// 运行全部本地检查（网络项：中心可达性，后续补）。
pub fn diagnose(config: &Config) -> Report {
    Report {
        checks: vec![config_check(config), upgrade_check(config)],
    }
}

fn config_check(config: &Config) -> Check {
    let endpoint = config.control_center_endpoint.trim();
    if endpoint.is_empty() {
        Check::fail(
            "config.endpoint",
            "控制中心 endpoint 未配置",
            "control_center_endpoint 为空",
            "在 gwlinkd.toml 填写 control_center_endpoint",
        )
    } else {
        Check::ok("config.endpoint", "控制中心 endpoint 已配置", endpoint)
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
