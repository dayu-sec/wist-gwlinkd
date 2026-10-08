//! `wist-gwlinkd` 本机配置（`gwlinkd.toml`）。

use std::path::{Path, PathBuf};

/// 本机配置。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    /// 控制中心 endpoint（https，形如 `https://center.example`，可带尾斜杠）。
    pub control_center_endpoint: String,
    /// 控制中心信任锚（PEM 路径）。
    pub trust_bundle: PathBuf,
    /// 状态目录（身份 / 凭据 / 升级记录 / 心跳）。
    pub state_dir: PathBuf,
    /// 本网关在中心侧的标识（admin 创建实例时确定）。
    pub gateway_id: String,
    /// 一次性接入券（Center「生成/轮换接入券」给出）：首跑（本机还没有客户端证书）时
    /// 用它做 link-upstream → register，**被消费即废**。
    ///
    /// 放在这里 = 「什么都不做就自联上」：写好这份配置、起 gwlinkd 即完成接入。
    /// 已注册后不再使用（要重接：改这项，或清 `state_dir` 重新注册）。
    /// 缺省（空 / 不写）= 本机没有待接入的券。
    #[serde(default)]
    pub link_token: Option<String>,
    /// 本机网关容器**自述面** endpoint（如 `https://127.0.0.1:3000`；缺省则不消费自述面）。
    #[serde(default)]
    pub gateway_self_endpoint: Option<String>,
    /// 网关容器 loopback 面（self-state / link-request）的**信任锚**（PEM 路径）。
    /// 网关以自签证书提供 HTTPS 时需要；缺省 = 用系统根。
    #[serde(default)]
    pub gateway_self_ca: Option<PathBuf>,
    /// 客户端证书轮换提前量（秒，缺省 3600）。
    #[serde(default)]
    pub renew_lead_seconds: Option<i64>,
    /// 升级执行器程序（缺省 `gops`）。
    #[serde(default)]
    pub upgrader_program: Option<String>,
    /// 升级失败处置（`rollback-all` | `halt`；缺省 `rollback-all`）。
    #[serde(default)]
    pub upgrade_on_failure: Option<String>,
    /// 栈外健康检查命令（给 gops `--health-cmd`；给 `sh -c`）：**给了才让 gops 据健康判定回滚**。
    #[serde(default)]
    pub upgrade_health_cmd: Option<String>,
    /// 健康检查超时秒数（给 gops `--health-timeout`；缺省由 gops 定）。
    #[serde(default)]
    pub upgrade_health_timeout_seconds: Option<u64>,
    /// 升级成功**佐证**的观测窗口秒数（执行器报成后，等网关自述面恢复健康的最长时间；缺省 300）。
    #[serde(default)]
    pub upgrade_verify_timeout_seconds: Option<u64>,
    /// gops 工程根（含 `ops-prj.yml`）：gops 从 **cwd** 解析工程，且 `gops prj upgrade` 没有
    /// 「指定工程」的旗标 —— **用 gops 执行器时必配**（缺配/错配会在发执行器前被拒：
    /// 见 `executor::GopsExecutor::preflight` 与 `diagnose` 的 `upgrade.project`）。
    #[serde(default)]
    pub upgrade_project_dir: Option<PathBuf>,
    /// 只升级该系统（gops 位置参数 NAME；缺省 = 工程里全部已导入系统）。
    #[serde(default)]
    pub upgrade_project_name: Option<String>,
    /// 升级被判死（心跳陈旧）后，是否自动清游标重驱同一计划（缺省 `true`）。
    ///
    /// - `true`：常驻自愈（可能重驱一个已被中断、实际还在跑的计划；靠 gops 工程锁串行化）；
    /// - `false`：仅报告，交由**管理面重派**（更保守，但机器会停在中间态直到人工介入）。
    #[serde(default)]
    pub upgrade_retry_on_dead: Option<bool>,
    /// 无状态工具（`tool-copy`）安装前是否**要求制品架构可校验且与本机一致**（缺省 `true`）。
    ///
    /// - `true`（缺省）：制品读不出 target-triple、或与本机架构 / 操作系统不符 → **拒装**
    ///   （宁可失败，也不用错架构的二进制覆盖本机工具 —— 那会静默报废工具）；
    /// - `false`：只放宽「读不出架构」这一种（记一笔事件后放行）；**已识别出的架构不符仍拒**。
    #[serde(default)]
    pub upgrade_tool_require_arch: Option<bool>,
    /// `[upgrade]` 段：本机**组件目录**（其余扁平 `upgrade_*` 键保持原样）。
    #[serde(default, rename = "upgrade")]
    pub upgrade: UpgradeSection,
}

/// `[upgrade]` 段。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct UpgradeSection {
    /// 本机组件目录（`[[upgrade.component]]`）：计划里的组件名 → 本机安装机制。
    #[serde(default)]
    pub component: Vec<UpgradeComponentConfig>,
}

/// 一个本机组件的安装口径。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UpgradeComponentConfig {
    /// 计划里的组件名（与中心发布的 release `component` 对齐）。
    pub name: String,
    /// 安装机制；缺省 `gops-project`（向后兼容：不在目录里的组件也走 gops）。
    #[serde(default)]
    pub install: UpgradeInstall,
    /// 二进制名（`tool-copy` 用）：据此在 `PATH` 上定位**原位置**（如 `galaxy-ops` → `gops`）。
    #[serde(default)]
    pub binary: Option<String>,
}

/// 组件安装机制。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeInstall {
    /// 交给 `gops prj upgrade`（缺省）—— 需要 `upgrade_project_dir` 工程根。
    #[default]
    GopsProject,
    /// **无状态工具**：解包制品后把二进制**复制覆盖**到它在 `PATH` 上的原位置（旧版备份），不经 gops。
    ToolCopy,
}

impl Config {
    /// 从 TOML 文件读配置。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("读取配置失败 {}: {err}", path.display()))?;
        toml::from_str(&text).map_err(|err| format!("解析配置失败 {}: {err}", path.display()))
    }
}

/// 把页面提交的接入物落到本机配置文件（`gwlinkd.toml`）：改写
/// `control_center_endpoint` / `gateway_id` / `link_token` / `trust_bundle`
/// （存在即替换、不存在即追加），**保留其余行与注释**。
///
/// 为什么写文件：链接关系是**部署接线**，落成配置文件即「持久记录」—— 这正是
/// 「在页面那一下 action 里改写配置」的落点（不落 DB）。
/// 空 `gateway_id` / 空 `link_token` 不覆盖已存值（页面允许留空）。
pub fn upsert_link_settings(
    path: &Path,
    center_endpoint: &str,
    gateway_id: &str,
    link_token: &str,
    trust_bundle: Option<&Path>,
) -> Result<(), String> {
    let original = std::fs::read_to_string(path)
        .map_err(|err| format!("读取配置失败 {}: {err}", path.display()))?;
    let mut text = original;
    text = upsert_scalar(
        &text,
        "control_center_endpoint",
        &toml_string(center_endpoint.trim()),
    );
    if !gateway_id.trim().is_empty() {
        text = upsert_scalar(&text, "gateway_id", &toml_string(gateway_id.trim()));
    }
    if !link_token.trim().is_empty() {
        text = upsert_scalar(&text, "link_token", &toml_string(link_token.trim()));
    }
    if let Some(ca) = trust_bundle {
        text = upsert_scalar(
            &text,
            "trust_bundle",
            &toml_string(&ca.display().to_string()),
        );
    }
    write_atomic(path, &text)
}

/// 把**顶层**标量键 `key` 替换为 `key = <literal>`；没有则**插到第一个 `[section]` 之前**
/// （顶层键必须落在所有段之前，否则会被 TOML 解析成那个段里的键）。
fn upsert_scalar(text: &str, key: &str, literal: &str) -> String {
    let replacement = format!("{key} = {literal}");
    let lines: Vec<&str> = text.lines().collect();
    let first_section = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with('[') && trimmed.ends_with(']')
    });
    let top_end = first_section.unwrap_or(lines.len());
    let mut out = String::new();
    match lines[..top_end]
        .iter()
        .position(|line| line_matches_key(line.trim_start(), key))
    {
        // 顶层已有该键：原地替换。
        Some(index) => {
            for (i, line) in lines.iter().enumerate() {
                out.push_str(if i == index { &replacement } else { line });
                out.push('\n');
            }
        }
        // 顶层没有：插到第一个段头之前（无段则追加到末尾）。
        None => {
            for (i, line) in lines.iter().enumerate() {
                if i == top_end {
                    out.push_str(&replacement);
                    out.push('\n');
                }
                out.push_str(line);
                out.push('\n');
            }
            if top_end == lines.len() {
                out.push_str(&replacement);
                out.push('\n');
            }
        }
    }
    out
}

/// 该行是否是顶层标量 `key = ……`（避免误命中 `key_extra = ……` / `==`）。
fn line_matches_key(trimmed: &str, key: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('=') && !rest.starts_with("==")
}

/// TOML 基本字符串字面量（转义 `\` 与 `"`）。
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// 原子写：先写临时文件再 rename，避免写一半损坏配置。
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("创建目录失败 {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content).map_err(|err| format!("写入失败 {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| format!("落盘失败 {}: {err}", path.display()))
}

/// `wist-gwlinkd init-config` 写出去的默认配置文本（带注释的 `gwlinkd.toml` 骶架）。
///
/// 由程序生成（与 `wist-gateway init-config` 同一套路）：不含任何密钥；接入券 `link_token`
/// 由「链接上级」页 / gwlinkd 自联时写回，或手工填。生成的文本必须能被 [`Config::load`] 解析
/// （有测试钉着）—— 否则现场 `init-config` 出来的配置一启动就报 missing field。
pub fn default_config_text() -> String {
    DEFAULT_CONFIG_TEMPLATE.to_string()
}

/// 默认配置路径（供 `init-config` 不传参数时用）。
pub fn default_config_path() -> PathBuf {
    PathBuf::from(crate::DEFAULT_CONFIG_PATH)
}

/// 默认配置模板（由 [`default_config_text`] 返回）。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# wist-gwlinkd 本机配置（由 `wist-gwlinkd init-config` 生成）。
#
# 把本网关接入上级控制中心：填 control_center_endpoint；自签中心再指向它的 CA（trust_bundle）。
# 首次接入的**一次性接入券**二选一：
#   * 走网关「链接上级」页提交接入链接 —— gwlinkd 拉到后会把值写回本文件；或
#   * 直接在本文件写 `link_token = "…"` —— 起进程即自联。
# 已注册后不要再动这些项；要重接：改本文件，或清 state_dir 重新注册。

# 上级控制中心地址（空 = 尚未接入）
control_center_endpoint = ""

# 中心 CA-S 信任锚路径（自签中心必需；公网中心可不配，回落系统根）
trust_bundle = "state/control-center.pem"

# 状态/身份目录（客户端证书与私钥落这里；**要备份**）
state_dir = "state"

# 本网关在中心侧的标识（须与中心实例名一致）
gateway_id = "gw-local"

# 可选：本机网关自述面 / 环回面（「链接上级」页面路才需要）
# gateway_self_endpoint = "https://127.0.0.1:3000"
# gateway_self_ca = "state/gateway-ca.crt.pem"

# 可选：一次性接入券（写上 = 起进程即自联；被消费即废）
# link_token = ""
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn loads_required_fields_and_defaults_the_optional_ones() {
        let dir = temp_dir("ok");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.control_center_endpoint, "https://c");
        assert_eq!(config.gateway_id, "gw-1");
        assert!(config.link_token.is_none());
        assert!(config.gateway_self_endpoint.is_none());
        assert!(config.gateway_self_ca.is_none());
        assert!(config.renew_lead_seconds.is_none());
        assert!(config.upgrader_program.is_none());
        assert!(config.upgrade_on_failure.is_none());
        assert!(config.upgrade_project_dir.is_none());
        assert!(config.upgrade_project_name.is_none());
        assert!(config.upgrade_retry_on_dead.is_none());
        assert!(config.upgrade_tool_require_arch.is_none());
        assert!(config.upgrade.component.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_token_parses_when_present() {
        let dir = temp_dir("link-token");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nlink_token = \"link_abc\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.link_token.as_deref(), Some("link_abc"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_link_settings_replaces_and_preserves_comments() {
        let dir = temp_dir("upsert");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "# 头部注释\ncontrol_center_endpoint = \"https://old\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/old.pem\"\nstate_dir = \"/s\"\n",
        )
        .expect("write");
        upsert_link_settings(
            &path,
            "https://new",
            "gw-2",
            "link_new",
            Some(Path::new("/new.pem")),
        )
        .expect("upsert");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# 头部注释"), "注释应保留：{text}");
        assert!(
            text.contains("control_center_endpoint = \"https://new\""),
            "{text}"
        );
        assert!(text.contains("gateway_id = \"gw-2\""), "{text}");
        assert!(text.contains("link_token = \"link_new\""), "{text}");
        assert!(text.contains("trust_bundle = \"/new.pem\""), "{text}");
        // 改完仍是合法配置。
        let config = Config::load(&path).expect("load");
        assert_eq!(config.link_token.as_deref(), Some("link_new"));
        assert_eq!(config.control_center_endpoint, "https://new");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_link_settings_appends_missing_keys() {
        let dir = temp_dir("upsert-append");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n",
        )
        .expect("write");
        upsert_link_settings(&path, "https://c", "", "link_abc", None).expect("upsert");
        let config = Config::load(&path).expect("load");
        // 追加了 link_token；空 gateway_id 不覆盖已存值。
        assert_eq!(config.link_token.as_deref(), Some("link_abc"));
        assert_eq!(config.gateway_id, "gw-1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_link_settings_inserts_before_the_first_section() {
        let dir = temp_dir("upsert-section");
        let path = dir.join("gwlinkd.toml");
        // 已有段：新键必须插在 `[upgrade]` **之前**，否则会被并进那个段（解析不到 / 静默忽略）。
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n[upgrade]\n",
        )
        .expect("write");
        upsert_link_settings(&path, "https://c", "", "link_abc", None).expect("upsert");
        let text = std::fs::read_to_string(&path).expect("read");
        let token_at = text.find("link_token").expect("token present");
        let section_at = text.find("[upgrade]").expect("section present");
        assert!(token_at < section_at, "顶层键必须在段之前：\n{text}");
        assert_eq!(
            Config::load(&path).expect("load").link_token.as_deref(),
            Some("link_abc")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_link_settings_is_idempotent() {
        let dir = temp_dir("upsert-idem");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n",
        )
        .expect("write");
        upsert_link_settings(&path, "https://c", "gw-1", "link_abc", None).expect("first");
        let once = std::fs::read_to_string(&path).expect("read");
        upsert_link_settings(&path, "https://c", "gw-1", "link_abc", None).expect("second");
        let twice = std::fs::read_to_string(&path).expect("read");
        assert_eq!(once, twice, "重复调用结果应稳定");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn line_matches_key_only_matches_top_level_assignments() {
        assert!(line_matches_key("link_token = \"x\"", "link_token"));
        assert!(line_matches_key("link_token=\"x\"", "link_token"));
        // 更长的键名（前缀命中）不算。
        assert!(!line_matches_key("link_token_extra = \"x\"", "link_token"));
        // 注释行不算。
        assert!(!line_matches_key("# link_token = \"x\"", "link_token"));
    }

    #[test]
    fn default_config_text_is_a_loadable_config() {
        // init-config 生成的配置必须能被解析 —— 否则现场一启动就报 missing field。
        let parsed: Config = toml::from_str(&default_config_text()).expect("默认配置必须可解析");
        assert_eq!(parsed.gateway_id, "gw-local");
        assert_eq!(parsed.control_center_endpoint, "");
        assert!(parsed.link_token.is_none());
        // 注释里的 `# link_token = ""` 不能被当成真配置（否则这里会是 Some("")）。
    }

    #[test]
    fn upgrade_component_catalog_parses_with_a_default_install() {
        let dir = temp_dir("components");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"galaxy-ops\"\ninstall = \"tool-copy\"\nbinary = \"gops\"\n\n\
             [[upgrade.component]]\nname = \"galaxy-flow\"\ninstall = \"tool-copy\"\nbinary = \"gx\"\n\n\
             [[upgrade.component]]\nname = \"wist-gateway-stack\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade.component.len(), 3);
        assert_eq!(config.upgrade.component[0].name, "galaxy-ops");
        assert_eq!(
            config.upgrade.component[0].install,
            UpgradeInstall::ToolCopy
        );
        assert_eq!(config.upgrade.component[0].binary.as_deref(), Some("gops"));
        assert_eq!(config.upgrade.component[1].binary.as_deref(), Some("gx"));
        // 不写 install → 缺省 gops-project（向后兼容）。
        assert_eq!(
            config.upgrade.component[2].install,
            UpgradeInstall::GopsProject
        );
        assert!(config.upgrade.component[2].binary.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_defaults_to_gops_project_and_parses_kebab_case() {
        assert_eq!(UpgradeInstall::default(), UpgradeInstall::GopsProject);

        let dir = temp_dir("install-values");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"a\"\ninstall = \"gops-project\"\n\n\
             [[upgrade.component]]\nname = \"b\"\ninstall = \"tool-copy\"\nbinary = \"gx\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(
            config.upgrade.component[0].install,
            UpgradeInstall::GopsProject
        );
        assert_eq!(
            config.upgrade.component[1].install,
            UpgradeInstall::ToolCopy
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unknown_install_value_is_rejected() {
        let dir = temp_dir("install-bogus");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"a\"\ninstall = \"bogus\"\n",
        )
        .expect("write");
        let err = Config::load(&path).expect_err("unknown install must be rejected");
        assert!(err.contains("配置"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gateway_self_ca_parses_when_present() {
        let dir = temp_dir("self-ca");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\ngateway_self_endpoint = \"https://127.0.0.1:3000\"\ngateway_self_ca = \"/self-ca.pem\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(
            config.gateway_self_endpoint.as_deref(),
            Some("https://127.0.0.1:3000")
        );
        assert_eq!(
            config.gateway_self_ca.as_deref(),
            Some(std::path::Path::new("/self-ca.pem"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_required_field_is_an_error() {
        let dir = temp_dir("bad");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(&path, "control_center_endpoint = \"https://c\"\n").expect("write");
        assert!(Config::load(&path).is_err(), "缺 gateway_id 必须报错");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn optional_upgrade_knobs_parse() {
        let dir = temp_dir("upgrade-knobs");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nupgrade_on_failure = \"halt\"\nupgrade_health_cmd = \"curl -fsS http://127.0.0.1:3000/health\"\nupgrade_health_timeout_seconds = 45\nupgrade_verify_timeout_seconds = 7\nupgrade_project_dir = \"/opt/prj\"\nupgrade_project_name = \"wist-gateway\"\nupgrade_retry_on_dead = false\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade_on_failure.as_deref(), Some("halt"));
        assert_eq!(
            config.upgrade_health_cmd.as_deref(),
            Some("curl -fsS http://127.0.0.1:3000/health")
        );
        assert_eq!(config.upgrade_health_timeout_seconds, Some(45));
        assert_eq!(config.upgrade_verify_timeout_seconds, Some(7));
        assert_eq!(
            config.upgrade_project_dir.as_deref(),
            Some(std::path::Path::new("/opt/prj"))
        );
        assert_eq!(config.upgrade_project_name.as_deref(), Some("wist-gateway"));
        assert_eq!(config.upgrade_retry_on_dead, Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_arch_requirement_parses_when_present() {
        let dir = temp_dir("tool-arch-knob");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nupgrade_tool_require_arch = false\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade_tool_require_arch, Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_path_is_a_clear_error() {
        let dir = temp_dir("missing");
        assert!(Config::load(&dir.join("nope.toml")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
