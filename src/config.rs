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
    /// 运行期凭据续期提前量（秒，缺省 3600）。
    #[serde(default)]
    pub renew_lead_seconds: Option<i64>,
    /// 升级执行器程序（缺省 `gops`）。
    #[serde(default)]
    pub upgrader_program: Option<String>,
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
        assert!(config.renew_lead_seconds.is_none());
        assert!(config.upgrader_program.is_none());
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
    fn an_unreadable_path_is_a_clear_error() {
        let dir = temp_dir("missing");
        assert!(Config::load(&dir.join("nope.toml")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
