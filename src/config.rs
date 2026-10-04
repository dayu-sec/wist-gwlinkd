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
}

impl Config {
    /// 从 TOML 文件读配置。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("读取配置失败 {}: {err}", path.display()))?;
        toml::from_str(&text).map_err(|err| format!("解析配置失败 {}: {err}", path.display()))
    }
}
