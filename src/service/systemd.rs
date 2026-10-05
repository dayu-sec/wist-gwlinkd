//! systemd unit 渲染（Linux 长期托管）。
//!
//! 关键取舍（与 `wist-agentd/src/service/systemd.rs` 同形）：
//! - `Type=simple` + 前台进程：gwlinkd 不做 double-fork，PID 1 直接跟踪主进程。
//! - `Restart=always`：正常/异常退出都拉起；`systemctl stop` 不触发重启。
//! - `Environment=WIST_GWLINKD_CONFIG=<绝对路径>`：配置走**单个绝对文件路径**（工作目录不确定，
//!   不能用相对的 `gwlinkd.toml`）。
//! - `KillMode=control-group` + `TimeoutStopSec=30`：停止时把进程组一起收走。

use std::path::PathBuf;

use super::{CONFIG_ENV, SERVICE_NAME, ServiceScope, ServiceSpec, home_dir};

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";
const USER_UNIT_DIR: &str = ".config/systemd/user";
const UNIT_FILE_NAME: &str = "wist-gwlinkd.service";

/// unit 文件落盘路径。
pub fn unit_path(scope: ServiceScope) -> Result<PathBuf, String> {
    match scope {
        ServiceScope::System => Ok(PathBuf::from(SYSTEM_UNIT_DIR).join(UNIT_FILE_NAME)),
        ServiceScope::User => Ok(home_dir()?.join(USER_UNIT_DIR).join(UNIT_FILE_NAME)),
    }
}

/// 渲染 unit 文本。写进 unit 的路径都经 [`systemd_arg`] 转义（空白/`%`/`$` 不会破坏指令）。
pub fn unit_text(spec: &ServiceSpec) -> String {
    let wanted_by = match spec.scope {
        ServiceScope::System => "multi-user.target",
        ServiceScope::User => "default.target",
    };
    let mut text = String::new();
    text.push_str("[Unit]\n");
    text.push_str("Description=wist gateway link daemon (host-side resident)\n");
    text.push_str("Documentation=https://github.com/dayu-sec/wist-gwlinkd\n");
    text.push_str("After=network-online.target\n");
    text.push_str("Wants=network-online.target\n");
    // 单实例锁冲突（前任未退出）会快速失败，这里限流避免重启风暴刷满 journal。
    text.push_str("StartLimitIntervalSec=60\n");
    text.push_str("StartLimitBurst=10\n");
    text.push('\n');
    text.push_str("[Service]\n");
    text.push_str("Type=simple\n");
    text.push_str(&format!(
        "ExecStart={} run\n",
        systemd_arg(&spec.bin.display().to_string())
    ));
    text.push_str(&format!(
        "Environment={}={}\n",
        CONFIG_ENV,
        systemd_arg(&spec.config_path.display().to_string())
    ));
    text.push_str("Restart=always\n");
    text.push_str("RestartSec=5\n");
    text.push_str("KillSignal=SIGTERM\n");
    text.push_str("KillMode=control-group\n");
    text.push_str("TimeoutStopSec=30\n");
    text.push_str(&format!("SyslogIdentifier={SERVICE_NAME}\n"));
    text.push_str("StandardOutput=journal\n");
    text.push_str("StandardError=journal\n");
    text.push_str("LimitNOFILE=65536\n");
    text.push_str("NoNewPrivileges=true\n");
    text.push('\n');
    text.push_str("[Install]\n");
    text.push_str(&format!("WantedBy={wanted_by}\n"));
    text
}

/// 把一个路径写成 systemd 指令参数：普通路径原样输出（可读），需要时加引号并转义。
/// systemd 规则：双引号内 `\\` `\"` 转义，`%` 是 specifier（写 `%%`），`$` 展开（写 `$$`）。
pub(super) fn systemd_arg(value: &str) -> String {
    let plain = value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(ch, '/' | '.' | '-' | '_' | ':' | '@' | '+' | '=' | ',')
    });
    if plain {
        return value.to_string();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_path_uses_system_dir_for_system_scope() {
        assert_eq!(
            unit_path(ServiceScope::System).expect("unit path"),
            PathBuf::from("/etc/systemd/system/wist-gwlinkd.service")
        );
    }

    #[test]
    fn systemd_arg_quotes_and_escapes_special_characters() {
        assert_eq!(systemd_arg("/usr/local/bin/wist-gwlinkd"), "/usr/local/bin/wist-gwlinkd");
        assert_eq!(systemd_arg("/opt/my app/bin"), "\"/opt/my app/bin\"");
        assert_eq!(systemd_arg("/a%i/bin"), "\"/a%%i/bin\"");
        assert_eq!(systemd_arg("/a$HOME/bin"), "\"/a$$HOME/bin\"");
    }
}
