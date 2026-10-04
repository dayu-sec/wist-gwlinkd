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
        checks: vec![
            config_check(config),
            credential_check(config),
            upgrade_check(config),
        ],
    }
}

/// 运行期凭据是否已就位（首跑前为 warn，不是错）。
fn credential_check(config: &Config) -> Check {
    match crate::state::load_credential(&config.state_dir) {
        Some(_) => Check::ok(
            "credential.local",
            "已有运行期凭据",
            "state/credential.json",
        ),
        None => {
            let mut check = Check::warn(
                "credential.local",
                "尚无运行期凭据",
                "首跑会走 link-upstream → register",
            );
            check.hint = Some(
                "设 WIST_GWLINKD_BOOTSTRAP_TOKEN 后运行 `wist-gwlinkd run` 完成首次置备"
                    .to_string(),
            );
            check
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
            upgrader_program: None,
        }
    }

    #[test]
    fn an_empty_endpoint_is_a_failure() {
        let dir = temp_dir("empty");
        let mut cfg = config(&dir);
        cfg.control_center_endpoint = "   ".into();
        assert_eq!(diagnose(&cfg).worst(), Status::Fail);
    }

    #[test]
    fn a_missing_credential_is_a_warning_not_a_failure() {
        let dir = temp_dir("cred");
        assert_eq!(diagnose(&config(&dir)).worst(), Status::Warn);
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
        assert_eq!(diagnose(&cfg).worst(), Status::Fail);
        crate::state::touch_heartbeat(&dir).expect("heartbeat");
        assert_ne!(diagnose(&cfg).worst(), Status::Fail);
    }
}
