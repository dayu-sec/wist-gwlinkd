//! 落盘状态与**判死判据（唯一来源）**。
//!
//! 「网关 / 升级器死没死」的判据只在这里定义；[`crate::doctor`]、上报、升级记账都复用它 ——
//! 不许各写一套，否则「判定说死了、诊断说没事」就会打架。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 心跳超过该时长即判死（与 `wist-agentd` 同量级）。
pub const UPGRADER_DEAD_AFTER: Duration = Duration::from_secs(60);

/// 升级记录文件名（字段对齐 `wist-agentd` / `gops` 的 `upgrade.json`）。
pub const UPGRADE_RECORD_FILE: &str = "upgrade.json";
/// 升级器心跳文件。
pub const UPGRADE_HEARTBEAT_FILE: &str = "upgrade.heartbeat";

/// 一次升级的记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpgradeRecord {
    pub work_id: String,
    pub from_version: String,
    pub to_version: String,
    pub step: String,
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

fn record_path(state_dir: &Path) -> PathBuf {
    state_dir.join(UPGRADE_RECORD_FILE)
}

fn heartbeat_path(state_dir: &Path) -> PathBuf {
    state_dir.join(UPGRADE_HEARTBEAT_FILE)
}

/// 读升级记录；不存在或不可解析 → `None`。
pub fn read_upgrade_record(state_dir: &Path) -> Option<UpgradeRecord> {
    let text = std::fs::read_to_string(record_path(state_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 心跳是否新鲜（未越过死亡阈值）—— **判死判据的唯一实现**。
pub fn heartbeat_is_fresh(state_dir: &Path, now: SystemTime) -> bool {
    let Ok(modified) =
        std::fs::metadata(heartbeat_path(state_dir)).and_then(|meta| meta.modified())
    else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) => age <= UPGRADER_DEAD_AFTER,
        // 心跳时间戳在未来（时钟回拨）→ 视为新鲜，不误判为死。
        Err(_) => true,
    }
}

/// 升级是否被判定为死：`status == "running"` 且心跳不新鲜（覆盖「停在中间态」）。
pub fn upgrader_is_declared_dead(state_dir: &Path, now: SystemTime) -> bool {
    let running =
        matches!(read_upgrade_record(state_dir), Some(record) if record.status == "running");
    running && !heartbeat_is_fresh(state_dir, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn running_record() -> UpgradeRecord {
        UpgradeRecord {
            work_id: "w-1".into(),
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            step: "restart".into(),
            status: "running".into(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_running_record_without_heartbeat_is_dead() {
        let dir = temp_dir("dead");
        let text = serde_json::to_string(&running_record()).expect("json");
        std::fs::write(record_path(&dir), text).expect("write record");
        assert!(upgrader_is_declared_dead(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fresh_heartbeat_is_not_dead() {
        let dir = temp_dir("alive");
        let text = serde_json::to_string(&running_record()).expect("json");
        std::fs::write(record_path(&dir), text).expect("write record");
        std::fs::write(heartbeat_path(&dir), b"").expect("touch heartbeat");
        assert!(heartbeat_is_fresh(&dir, SystemTime::now()));
        assert!(!upgrader_is_declared_dead(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_record_is_never_declared_dead() {
        let dir = temp_dir("none");
        assert!(!upgrader_is_declared_dead(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }
}
