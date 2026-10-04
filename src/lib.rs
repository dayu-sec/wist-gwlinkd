//! `wist-gwlinkd`：网关栈在 **host 侧** 的常驻，代表本机网关栈与 `WistCenter` 维持一条
//! **独立于网关容器**的控制链路：注册 / 心跳上报 / 凭据续期（`rt_` 唯一持有者）/ 升级取指令与回执。
//!
//! 与 `wist-agentd` 同构：常驻 + 诊断 + 判死 + 驱动升级。**判死与诊断共用同一判据**
//! （见 [`state::heartbeat_is_fresh`] / [`state::UPGRADER_DEAD_AFTER`]）—— 这是「判定」与
//! 「诊断」永不打架的关键。
//!
//! 背景与决策见 CR-003（`wist-design/doc/design/foundation/cross-repo-issues.md`）。

pub mod center;
pub mod config;
pub mod doctor;
pub mod state;
pub mod upgrade;

/// 当前版本（由 gx 同步 `Cargo.toml` / `version.txt`）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 缺省配置路径（`WIST_GWLINKD_CONFIG` 可覆盖）。
pub const DEFAULT_CONFIG_PATH: &str = "gwlinkd.toml";
