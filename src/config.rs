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
}

impl Config {
    /// 从 TOML 文件读配置。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("读取配置失败 {}: {err}", path.display()))?;
        toml::from_str(&text).map_err(|err| format!("解析配置失败 {}: {err}", path.display()))
    }
}

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
        assert!(config.gateway_self_endpoint.is_none());
        assert!(config.gateway_self_ca.is_none());
        assert!(config.renew_lead_seconds.is_none());
        assert!(config.upgrader_program.is_none());
        assert!(config.upgrade_on_failure.is_none());
        assert!(config.upgrade_project_dir.is_none());
        assert!(config.upgrade_project_name.is_none());
        assert!(config.upgrade_retry_on_dead.is_none());
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
    fn an_unreadable_path_is_a_clear_error() {
        let dir = temp_dir("missing");
        assert!(Config::load(&dir.join("nope.toml")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
