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
use crate::error::ServiceResult;

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";
const USER_UNIT_DIR: &str = ".config/systemd/user";
const UNIT_FILE_NAME: &str = "wist-gwlinkd.service";

/// unit 文件落盘路径。
pub fn unit_path(scope: ServiceScope) -> ServiceResult<PathBuf> {
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
    // system 作用域可选**非 root 运行**（`--run-as`）：身份材料回写、gops 升级都在部署用户下完成。
    // user 作用域不加（那本就以本人运行；系统 also 不接受 User= 与自身冲突）。
    if let Some(user) = spec.run_as_identity() {
        text.push_str(&format!("User={}\n", systemd_arg(user)));
        if let Some(group) = spec.run_as_group.as_deref() {
            text.push_str(&format!("Group={}\n", systemd_arg(group)));
        }
    }
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
    // 现场态配置（中心地址 / 网关标识 / 信任锚）与主配置分文件且**不一定同目录**
    // （栈布局在 identity/gwlinkd/）—— 必须显式告知进程。
    if let Some(pub_config) = spec.pub_config.as_deref() {
        text.push_str(&format!(
            "Environment={}={}\n",
            crate::config::PUB_CONFIG_ENV,
            systemd_arg(&pub_config.display().to_string())
        ));
    }
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

/// 从 unit 文本里读回落点（`ExecStart=` 的首个参数 + 两条 `Environment=`）。
///
/// 只认本模块 [`unit_text`] 渲染出来的形状（唯一来源）—— 不做通用 systemd 解析。
pub(super) fn parse_definition(text: &str) -> super::DefinitionPaths {
    let mut paths = super::DefinitionPaths::default();
    let config_key = format!("Environment={CONFIG_ENV}=");
    let pub_key = format!("Environment={}=", crate::config::PUB_CONFIG_ENV);
    for raw in text.lines() {
        let line = raw.trim();
        // 注释行不算：手工改过的 unit 常把旧值注掉（`# Environment=WIST_GWLINKD_CONFIG=/old`），
        // 当真值会把回落引到一份早已不用的配置上。
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(value) = line.strip_prefix("ExecStart=") {
            paths.bin = parse_first_arg(value).as_deref().and_then(non_empty);
        } else if let Some(value) = line.strip_prefix(&config_key) {
            paths.config_path = non_empty(&unquote_arg(value));
        } else if let Some(value) = line.strip_prefix(&pub_key) {
            paths.pub_config = non_empty(&unquote_arg(value));
        }
    }
    paths
}

/// 空值 ⇒ `None`：`Environment=KEY=` 是「没设」，别当成一个空路径（后面会变成 `is_file()` 永假）。
fn non_empty(value: &str) -> Option<PathBuf> {
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// 取 `ExecStart=` 的**首个参数**（`<bin> run` 里的那个 `<bin>`），并撤销 [`systemd_arg`] 的转义。
fn parse_first_arg(value: &str) -> Option<String> {
    let raw = match value.strip_prefix('"') {
        // 带引号（路径含空白 / `%` / `$` 时渲染会加）：按转义找收尾引号，**连引号**取出来交给 `unquote_arg`。
        Some(rest) => {
            let mut escaped = false;
            let mut end = None;
            for (idx, ch) in rest.char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => escaped = true,
                    '"' => {
                        end = Some(idx);
                        break;
                    }
                    _ => {}
                }
            }
            &value[..end? + 2]
        }
        None => value.split_whitespace().next()?,
    };
    Some(unquote_arg(raw))
}

/// [`systemd_arg`] 的逆：去掉一对包裹引号并还原 `\\` `\"` `%%` `$$`。
fn unquote_arg(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return value.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '%' | '$' => {
                // 渲染时写了两份（`%%` / `$$`），还原成一份。
                if chars.clone().next() == Some(ch) {
                    chars.next();
                }
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
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
        assert_eq!(
            systemd_arg("/usr/local/bin/wist-gwlinkd"),
            "/usr/local/bin/wist-gwlinkd"
        );
        assert_eq!(systemd_arg("/opt/my app/bin"), "\"/opt/my app/bin\"");
        assert_eq!(systemd_arg("/a%i/bin"), "\"/a%%i/bin\"");
        assert_eq!(systemd_arg("/a$HOME/bin"), "\"/a$$HOME/bin\"");
    }

    #[test]
    fn parse_definition_round_trips_what_render_writes() {
        // 路径刻意难缠：空格 / `%` / `$`（渲染会加引号并转义）—— 解析必须原样还原。
        let spec = ServiceSpec::new(
            ServiceScope::System,
            PathBuf::from("/opt/my app/bin/wist-gwlinkd"),
            PathBuf::from("/opt/con f%ig/gwlinkd.toml"),
        )
        .with_pub_config(Some(PathBuf::from("/opt/pu b/$g/gwlinkd-pub.toml")));

        let paths = parse_definition(&unit_text(&spec));
        assert_eq!(
            paths.bin,
            Some(PathBuf::from("/opt/my app/bin/wist-gwlinkd"))
        );
        assert_eq!(
            paths.config_path,
            Some(PathBuf::from("/opt/con f%ig/gwlinkd.toml"))
        );
        assert_eq!(
            paths.pub_config,
            Some(PathBuf::from("/opt/pu b/$g/gwlinkd-pub.toml"))
        );
    }

    #[test]
    fn parse_definition_yields_nothing_when_keys_are_absent() {
        let paths = parse_definition("[Unit]\nDescription=x\n");
        assert_eq!(paths, crate::service::DefinitionPaths::default());
    }

    #[test]
    fn parse_definition_reads_plain_paths() {
        // 普通路径不加引号（渲染器只在必要时才加）—— 也得认。
        let paths = parse_definition(
            "[Service]\nExecStart=/usr/local/bin/wist-gwlinkd run\n\
             Environment=WIST_GWLINKD_CONFIG=/etc/wist-gwlinkd/gwlinkd.toml\n",
        );
        assert_eq!(
            paths.bin,
            Some(PathBuf::from("/usr/local/bin/wist-gwlinkd"))
        );
        assert_eq!(
            paths.config_path,
            Some(PathBuf::from("/etc/wist-gwlinkd/gwlinkd.toml"))
        );
        assert_eq!(paths.pub_config, None);
    }

    #[test]
    fn parse_definition_ignores_commented_lines_and_empty_values() {
        // 注掉的旧值 + `KEY=` 空值：两者都不能被当真值（否则回落会指向一份不存在的配置）。
        let paths = parse_definition(
            "[Service]\n\
             # Environment=WIST_GWLINKD_CONFIG=/old/gone.toml\n\
             ; Environment=WIST_GWLINKD_PUB_CONFIG=/old/gone-pub.toml\n\
             Environment=WIST_GWLINKD_CONFIG=\n",
        );
        assert_eq!(paths.config_path, None);
        assert_eq!(paths.pub_config, None);
    }
}
