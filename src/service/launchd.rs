//! launchd plist 渲染（macOS 长期托管）。
//!
//! 关键取舍（与 `wist-agentd/src/service/launchd.rs` 同形）：
//! - `RunAtLoad` + `KeepAlive`：加载即启动，退出（正常或崩溃）都拉起；`launchctl bootout` 才是停。
//! - `ThrottleInterval=10`：最小重启间隔，避免启动即失败时形成重启风暴。
//! - 标准输出/错误：系统级落 `/var/log/wist-gwlinkd/`，用户级落 `~/Library/Logs/wist-gwlinkd/`
//!   （用户写不了 `/var/log`），需配合 newsyslog / logrotate 轮转。
//! - `WorkingDirectory` 落在稳定目录；配置用**绝对路径**（经 `WIST_GWLINKD_CONFIG`）。

use std::path::{Path, PathBuf};

use super::{CONFIG_ENV, LAUNCHD_LABEL, SYSTEM_LOG_DIR, ServiceScope, ServiceSpec, home_dir};
use crate::error::ServiceResult;

const SYSTEM_PLIST_DIR: &str = "/Library/LaunchDaemons";
const USER_PLIST_DIR: &str = "Library/LaunchAgents";
const PLIST_FILE_NAME: &str = "com.dayu-sec.wist-gwlinkd.plist";
const USER_LOG_DIR: &str = "Library/Logs/wist-gwlinkd";

/// 标准输出落盘文件名。
pub const STDOUT_FILE: &str = "gwlinkd.out";
/// 标准错误落盘文件名（gwlinkd 的运维输出都走 stderr）。
pub const STDERR_FILE: &str = "gwlinkd.err";

/// plist 落盘路径。
pub fn plist_path(scope: ServiceScope) -> ServiceResult<PathBuf> {
    match scope {
        ServiceScope::System => Ok(PathBuf::from(SYSTEM_PLIST_DIR).join(PLIST_FILE_NAME)),
        ServiceScope::User => Ok(home_dir()?.join(USER_PLIST_DIR).join(PLIST_FILE_NAME)),
    }
}

/// launchd 标准输出/错误目录：系统级 `/var/log/wist-gwlinkd`，用户级 `~/Library/Logs/wist-gwlinkd`。
pub fn log_dir(scope: ServiceScope) -> ServiceResult<PathBuf> {
    match scope {
        ServiceScope::System => Ok(PathBuf::from(SYSTEM_LOG_DIR)),
        ServiceScope::User => Ok(home_dir()?.join(USER_LOG_DIR)),
    }
}

/// 把 stdio 落点交给**运行身份**（`system` + `--run-as` 才有意义）。
///
/// 为什么必须做：带 `UserName` 的 LaunchDaemon，launchd 会用**该用户**去打开 `StandardOutPath` /
/// `StandardErrorPath`；而 `service install` 是经 `sudo` 以 **root** 跑的，`create_dir_all` 建出来的
/// 目录是 `root:wheel`（0755）—— 用户写不了这两个文件，于是
/// `posix_spawn` 直接失败：`Service could not initialize: posix_spawn(...) error 0xd - Permission denied`，
/// 而且**应用日志永远是空的**（launchd 连文件都没建），现场极易误判成「配置/二进制不对」。
///
/// 一并处理**已存在**的 stdio 文件：老安装是由 root 建的（那时服务可能以 root 在跑），
/// 打开「已存在但写不了」的文件一样 EACCES。
pub(super) fn hand_log_dir_to_run_as(log_dir: &Path, spec: &ServiceSpec) -> ServiceResult<()> {
    // user 作用域本就以本人运行，目录天然可写。
    if spec.scope != ServiceScope::System {
        return Ok(());
    }
    let Some(user) = spec.run_as.as_deref() else {
        return Ok(());
    };
    let identity = super::resolve_user(user, spec.run_as_group.as_deref())?;
    super::chown_path(log_dir, identity, "launchd 日志目录")?;
    for name in [STDOUT_FILE, STDERR_FILE] {
        let file = log_dir.join(name);
        if file.exists() {
            super::chown_path(&file, identity, "launchd stdio 文件")?;
        }
    }
    Ok(())
}

/// 渲染 plist 文本。`log_dir_override` 为 `None` 时回退到作用域默认日志目录。
pub fn plist_text(spec: &ServiceSpec, log_dir_override: Option<&Path>) -> String {
    let working_dir = match spec.scope {
        ServiceScope::System => PathBuf::from("/"),
        ServiceScope::User => home_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };
    let log_dir = match log_dir_override {
        Some(dir) => dir.to_path_buf(),
        None => log_dir(spec.scope).unwrap_or_else(|_| PathBuf::from("/tmp")),
    };

    let mut text = String::new();
    text.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    text.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    text.push_str("<plist version=\"1.0\">\n<dict>\n");
    text.push_str(&kv_string("Label", LAUNCHD_LABEL));
    // LaunchDaemon（system 作用域）可选**非 root 运行**：身份回写 / gops 升级在部署用户下完成。
    // UserAgent（user 作用域）不加（那本就以本人运行）。
    if let Some(user) = spec.run_as_identity() {
        text.push_str(&kv_string("UserName", user));
        if let Some(group) = spec.run_as_group.as_deref() {
            text.push_str(&kv_string("GroupName", group));
        }
    }
    text.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    text.push_str(&format!(
        "    <string>{}</string>\n",
        escape_xml(&spec.bin.display().to_string())
    ));
    text.push_str("    <string>run</string>\n");
    text.push_str("  </array>\n");
    text.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    text.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    text.push_str("  <key>ThrottleInterval</key>\n  <integer>10</integer>\n");
    text.push_str("  <key>ExitTimeOut</key>\n  <integer>30</integer>\n");
    text.push_str("  <key>ProcessType</key>\n  <string>Background</string>\n");
    text.push_str(&kv_string(
        "WorkingDirectory",
        &working_dir.display().to_string(),
    ));
    text.push_str(&kv_string(
        "StandardOutPath",
        &log_dir.join(STDOUT_FILE).display().to_string(),
    ));
    text.push_str(&kv_string(
        "StandardErrorPath",
        &log_dir.join(STDERR_FILE).display().to_string(),
    ));
    text.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
    text.push_str("    <key>PATH</key>\n");
    text.push_str("    <string>/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n");
    text.push_str(&format!("    <key>{CONFIG_ENV}</key>\n"));
    text.push_str(&format!(
        "    <string>{}</string>\n",
        escape_xml(&spec.config_path.display().to_string())
    ));
    // 现场态配置（中心地址 / 网关标识 / 信任锚）与主配置分文件，必须显式告知进程。
    if let Some(pub_config) = spec.pub_config.as_deref() {
        text.push_str(&format!(
            "    <key>{}</key>\n",
            escape_xml(crate::config::PUB_CONFIG_ENV)
        ));
        text.push_str(&format!(
            "    <string>{}</string>\n",
            escape_xml(&pub_config.display().to_string())
        ));
    }
    text.push_str("  </dict>\n");
    text.push_str("</dict>\n</plist>\n");
    text
}

/// 从 plist 文本里读回落点（`ProgramArguments` 的首个 string + 两条环境变量）。
///
/// 只认本模块 [`plist_text`] 渲染出来的形状（唯一来源）—— 不做通用 plist 解析。
pub(super) fn parse_definition(text: &str) -> super::DefinitionPaths {
    let body = strip_xml_comments(text);
    super::DefinitionPaths {
        bin: plist_string_after_key(&body, "ProgramArguments"),
        config_path: plist_string_after_key(&body, CONFIG_ENV),
        pub_config: plist_string_after_key(&body, crate::config::PUB_CONFIG_ENV),
    }
}

/// 去掉 `<!-- … -->` 段：手工改过的 plist 常把旧值注掉，注释里的键不该被当成真值。
fn strip_xml_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + "-->".len()..],
            // 注释没闭合：后面都当注释（宁可读不到，也不要把半截注释当真值）。
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// 取 `<key>KEY</key>` 之后**第一个** `<string>…</string>` 并做 XML 反转义（空值 ⇒ `None`）。
///
/// `ProgramArguments` 是个 `<array>`，它的首个 `<string>` 就是程序路径 —— 正好也适用
/// （后面的 `--config-dir` 等参数不在首个，天然忽略）。
fn plist_string_after_key(text: &str, key: &str) -> Option<PathBuf> {
    let needle = format!("<key>{key}</key>");
    let rest = &text[text.find(&needle)? + needle.len()..];
    let rest = &rest[rest.find("<string>")? + "<string>".len()..];
    let end = rest.find("</string>")?;
    let value = unescape_xml(&rest[..end]);
    (!value.is_empty()).then(|| PathBuf::from(value))
}

/// [`escape_xml`] 的逆（只反渲染时用到的五个实体；`&amp;` 得放最后，否则会二次解码）。
fn unescape_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn kv_string(key: &str, value: &str) -> String {
    format!(
        "  <key>{}</key>\n  <string>{}</string>\n",
        escape_xml(key),
        escape_xml(value)
    )
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_escapes_xml_special_characters() {
        assert_eq!(escape_xml("a&b<c\"d"), "a&amp;b&lt;c&quot;d");
    }

    #[test]
    fn parse_definition_round_trips_what_render_writes() {
        // 路径刻意难缠：`&` / `<` / 引号（渲染会做 XML 转义）—— 解析必须原样还原。
        let spec = ServiceSpec::new(
            ServiceScope::System,
            PathBuf::from("/opt/a&b/bin/wist-gwlinkd"),
            PathBuf::from("/opt/<cf g>/gwlinkd.toml"),
        )
        .with_pub_config(Some(PathBuf::from("/opt/\"pub\"/gwlinkd-pub.toml")));

        let paths = parse_definition(&plist_text(&spec, None));
        assert_eq!(paths.bin, Some(PathBuf::from("/opt/a&b/bin/wist-gwlinkd")));
        assert_eq!(
            paths.config_path,
            Some(PathBuf::from("/opt/<cf g>/gwlinkd.toml"))
        );
        assert_eq!(
            paths.pub_config,
            Some(PathBuf::from("/opt/\"pub\"/gwlinkd-pub.toml"))
        );
    }

    #[test]
    fn parse_definition_yields_nothing_when_keys_are_absent() {
        let paths = parse_definition("<plist version=\"1.0\"><dict></dict></plist>");
        assert_eq!(paths, crate::service::DefinitionPaths::default());
    }

    #[test]
    fn parse_definition_ignores_comments_and_empty_strings() {
        // 注掉的旧值（`<!-- … -->`）+ 空 `<string></string>`：两者都不能被当真值。
        let text = concat!(
            "<plist version=\"1.0\"><dict>\n",
            "<!-- <key>WIST_GWLINKD_CONFIG</key><string>/old/gone.toml</string> -->\n",
            "<key>WIST_GWLINKD_CONFIG</key>\n<string></string>\n",
            "</dict></plist>"
        );
        assert_eq!(parse_definition(text).config_path, None);
    }

    #[test]
    fn parse_definition_takes_only_the_first_argument() {
        // 与 agentd 同形：程序后面可以跟参数（如 `--config-dir …`）—— 只取首个当 bin。
        let text = concat!(
            "<plist version=\"1.0\"><dict>\n",
            "<key>ProgramArguments</key>\n<array>\n",
            "<string>/usr/local/bin/wist-gwlinkd</string>\n",
            "<string>run</string>\n",
            "<string>--config-dir</string>\n",
            "<string>/etc/wist-gwlinkd</string>\n",
            "</array>\n</dict></plist>"
        );
        let paths = parse_definition(text);
        assert_eq!(
            paths.bin,
            Some(PathBuf::from("/usr/local/bin/wist-gwlinkd"))
        );
        assert_eq!(paths.config_path, None);
    }

    #[test]
    fn plist_path_uses_launch_daemons_for_system_scope() {
        assert_eq!(
            plist_path(ServiceScope::System).expect("plist path"),
            PathBuf::from("/Library/LaunchDaemons/com.dayu-sec.wist-gwlinkd.plist")
        );
    }
}
