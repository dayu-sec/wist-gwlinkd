//! 落盘状态与**判死判据（唯一来源）**。
//!
//! 「网关 / 升级器死没死」的判据只在这里定义；[`crate::doctor`]、上报、升级记账都复用它 ——
//! 不许各写一套，否则「判定说死了、诊断说没事」就会打架。
//!
//! 同时承载本机的**身份与凭据**：网关身份 `ident_`（首跑自生成、中心不存）与运行期凭据
//! `rt_`（`register` 后签发，本进程是唯一持有者）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use wist_contracts::gateway_control::GatewayCredentialBundle;

/// 心跳超过该时长即判死（与 `wist-agentd` 同量级）。
pub const UPGRADER_DEAD_AFTER: Duration = Duration::from_secs(60);

/// 升级记录文件名（字段对齐 `wist-agentd` / `gops` 的 `upgrade.json`）。
pub const UPGRADE_RECORD_FILE: &str = "upgrade.json";
/// 升级器心跳文件。
pub const UPGRADE_HEARTBEAT_FILE: &str = "upgrade.heartbeat";
/// 升级游标：记录「已驱动的计划 + 目标版本」，跨重启幂等。
pub const UPGRADE_CURSOR_FILE: &str = "upgrade-cursor.json";
/// 网关身份 `ident_` 文件。
pub const IDENTITY_FILE: &str = "identity";
/// 一次运行的实例标识文件（注册用，重启保持稳定）。
pub const INSTANCE_FILE: &str = "instance";
/// 运行期凭据文件。
pub const CREDENTIAL_FILE: &str = "credential.json";
/// link-upstream 返回的链接配置（信任锚 / 协议版本 / 注册 token 引用）。
pub const LINK_CONFIG_FILE: &str = "link-config.json";
/// 单实例锁文件。
pub const LOCK_FILE: &str = "gwlinkd.lock";

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

/// 升级游标：跨重启记住「已驱动的计划」，避免重启后重驱同一计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpgradeCursor {
    #[serde(default)]
    pub last_plan_id: Option<String>,
    #[serde(default)]
    pub last_to_version: String,
}

fn path_in(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(name)
}

/// 生成 `<prefix>_<64hex>`（镜像 `wist-center` / `wist-gateway` 的 `new_secret_token`）。
pub fn new_secret_token(prefix: &str) -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to read system random source".to_string())?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(prefix.len() + 1 + bytes.len() * 2);
    out.push_str(prefix);
    out.push('_');
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(out)
}

fn ensure_dir(state_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|err| format!("创建状态目录失败 {}: {err}", state_dir.display()))
}

/// 原子写：**先写临时文件 → rename**（读者永不会看到半截内容）。
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("创建目录失败 {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|err| format!("写入失败 {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|err| format!("落盘失败 {}: {err}", path.display()))
}

/// 写敏感文件：原子写 + 中间文件设 `0600`（崩溃不会留半截文件把凭据写坏）。
fn write_secret(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("创建目录失败 {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|err| format!("写入失败 {}: {err}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("设权限失败 {}: {err}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|err| format!("落盘失败 {}: {err}", path.display()))
}

/// 单实例锁：拿不到即说明本机已有常驻在跑。进程退出时由内核释放（无陈旧锁问题）。
pub struct LockGuard {
    _file: std::fs::File,
}

/// 获取单实例锁（`flock LOCK_EX|LOCK_NB`）。
pub fn acquire_single_instance_lock(state_dir: &Path) -> Result<LockGuard, String> {
    ensure_dir(state_dir)?;
    let path = path_in(state_dir, LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| format!("建锁文件失败 {}: {err}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `flock` 只读 fd 的有效性；`file` 在本 guard 生命周期内存活。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err("已有网关常驻在跑（拿不到单实例锁），拒绝启动第二个".to_string());
        }
    }
    Ok(LockGuard { _file: file })
}

// ───────────────────────── 身份 / 实例 / 凭据 / 链接配置 ─────────────────────────

/// 读或生成网关身份 `ident_`（首跑自生成，落盘 `0600`；中心不存）。
pub fn load_or_create_identity(state_dir: &Path) -> Result<String, String> {
    let path = path_in(state_dir, IDENTITY_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim();
        if token.starts_with("ident_") {
            return Ok(token.to_string());
        }
        // 非空但不成形（不是空的首写中断）→ 拒绝静默换身份（换身份会导致重复置备）。
        if !token.is_empty() {
            return Err(format!(
                "身份文件损坏 {}（内容不以 ident_ 开头）：修复或删除后重跑",
                path.display()
            ));
        }
    }
    let token = new_secret_token("ident")?;
    ensure_dir(state_dir)?;
    write_secret(&path, &token)?;
    Ok(token)
}

/// 读或生成本次运行的实例标识（注册用；重启保持稳定）。
pub fn load_or_create_instance_id(state_dir: &Path, gateway_id: &str) -> Result<String, String> {
    let path = path_in(state_dir, INSTANCE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let value = text.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    let boot = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let value = format!("{gateway_id}/inst-{boot}");
    ensure_dir(state_dir)?;
    write_secret(&path, &value)?;
    Ok(value)
}

/// 本机持有的网关长期身份：中心签发的**客户端证书** + 本机私钥（私钥永不出本机）。
///
/// 取代旧的运行期 bearer `rt_`：mTLS 是网关↔中心的唯一凭据路径。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredCredential {
    /// 中心回执的凭据包（含客户端证书 PEM；`ca_bundle` / `not_after` 供排障）。
    pub bundle: GatewayCredentialBundle,
    /// 客户端私钥（PEM，PKCS#8）。**永不上送 / 不出本机**。
    pub private_key_pem: String,
}

impl StoredCredential {
    /// mTLS 身份 PEM（证书在前、私钥在后），供 `reqwest::Identity::from_pem`。
    pub fn identity_pem(&self) -> String {
        format!(
            "{}\n{}\n",
            self.bundle.certificate.trim_end(),
            self.private_key_pem.trim_end()
        )
    }

    /// 证书序列号（小写 hex，无分隔符）：轮换时提交给中心（用旧证书证明身份）。
    pub fn certificate_serial_hex(&self) -> Result<String, String> {
        use x509_parser::prelude::{FromDer, X509Certificate};
        let (_, pem) = x509_parser::pem::parse_x509_pem(self.bundle.certificate.as_bytes())
            .map_err(|err| format!("解析客户端证书 PEM 失败: {err}"))?;
        let (_, cert) = X509Certificate::from_der(&pem.contents)
            .map_err(|err| format!("解析客户端证书 DER 失败: {err}"))?;
        Ok(hex_lower(cert.raw_serial()))
    }

    /// 距离证书到期还剩多少秒（无 `not_after` → `i64::MAX`，不触发轮换）。
    pub fn seconds_remaining(&self) -> i64 {
        self.bundle
            .not_after
            .as_deref()
            .and_then(wist_control::types::DateTime::from_rfc3339)
            .map(|expires| wist_control::DateTime::now().seconds_until(&expires))
            .unwrap_or(i64::MAX)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 保存长期身份（`register` / `renew` 后；原子写 + 0600）。
pub fn save_credential(state_dir: &Path, credential: &StoredCredential) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(credential).map_err(|err| format!("序列化凭据失败: {err}"))?;
    write_secret(&path_in(state_dir, CREDENTIAL_FILE), &text)
}

/// 长期身份的就位状态（区分「缺失」与「损坏」，不把损坏当缺失静默重置备）。
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialStatus {
    /// 文件不存在（首跑前置备状态）。
    Missing,
    /// 文件在但读不了/解析不了：**不可**当成缺失自动重置备。
    Corrupt(String),
    /// 就绪。
    Present(StoredCredential),
}

/// 读长期身份的就位状态（见 [`CredentialStatus`]）。
pub fn credential_status(state_dir: &Path) -> CredentialStatus {
    let path = path_in(state_dir, CREDENTIAL_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CredentialStatus::Missing,
        Err(err) => {
            return CredentialStatus::Corrupt(format!("读取失败 {}: {err}", path.display()));
        }
    };
    match serde_json::from_str(&text) {
        Ok(credential) => CredentialStatus::Present(credential),
        Err(err) => CredentialStatus::Corrupt(format!("解析失败 {}: {err}", path.display())),
    }
}

/// 读长期身份；不存在、损坏或不可解析 → `None`。
pub fn load_credential(state_dir: &Path) -> Option<StoredCredential> {
    match credential_status(state_dir) {
        CredentialStatus::Present(credential) => Some(credential),
        _ => None,
    }
}

/// 保存 link-upstream 返回的链接配置（信任锚 / 协议版本 / 注册 token 引用）—— 落盘留痕，不丢。
pub fn save_link_config(
    state_dir: &Path,
    config: &wist_control::GatewayInitialConfig,
) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(config).map_err(|err| format!("序列化链接配置失败: {err}"))?;
    write_secret(&path_in(state_dir, LINK_CONFIG_FILE), &text)
}

// ───────────────────────── 升级记录 / 心跳 / 游标 ─────────────────────────

/// 读升级记录；不存在或不可解析 → `None`。
pub fn read_upgrade_record(state_dir: &Path) -> Option<UpgradeRecord> {
    let text = std::fs::read_to_string(path_in(state_dir, UPGRADE_RECORD_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写升级记录（**原子**：诊断/判死都在读它，半截写会被当成「无升级」）。
pub fn write_upgrade_record(state_dir: &Path, record: &UpgradeRecord) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(record).map_err(|err| format!("序列化升级记录失败: {err}"))?;
    write_atomic(&path_in(state_dir, UPGRADE_RECORD_FILE), &text)
}

/// 读升级游标；不存在或不可解析 → 默认（无已驱计划）。
pub fn load_upgrade_cursor(state_dir: &Path) -> UpgradeCursor {
    std::fs::read_to_string(path_in(state_dir, UPGRADE_CURSOR_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 写升级游标。
pub fn save_upgrade_cursor(state_dir: &Path, cursor: &UpgradeCursor) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(cursor).map_err(|err| format!("序列化升级游标失败: {err}"))?;
    write_secret(&path_in(state_dir, UPGRADE_CURSOR_FILE), &text)
}

/// 打一次心跳（升级器进程周期调用）。
pub fn touch_heartbeat(state_dir: &Path) -> Result<(), String> {
    ensure_dir(state_dir)?;
    std::fs::write(path_in(state_dir, UPGRADE_HEARTBEAT_FILE), b"")
        .map_err(|err| format!("写心跳失败: {err}"))
}

/// 心跳是否新鲜（未越过死亡阈值）—— **判死判据的唯一实现**。
pub fn heartbeat_is_fresh(state_dir: &Path, now: SystemTime) -> bool {
    let Ok(modified) = std::fs::metadata(path_in(state_dir, UPGRADE_HEARTBEAT_FILE))
        .and_then(|meta| meta.modified())
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

/// 是否**有升级在飞**（`running` 且心跳新鲜）—— 驱动升级前的互斥判据。
pub fn upgrade_in_flight(state_dir: &Path, now: SystemTime) -> bool {
    let running =
        matches!(read_upgrade_record(state_dir), Some(record) if record.status == "running");
    running && heartbeat_is_fresh(state_dir, now)
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
        write_upgrade_record(&dir, &running_record()).expect("write record");
        assert!(upgrader_is_declared_dead(&dir, SystemTime::now()));
        assert!(!upgrade_in_flight(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fresh_heartbeat_is_not_dead_and_counts_as_in_flight() {
        let dir = temp_dir("alive");
        write_upgrade_record(&dir, &running_record()).expect("write record");
        touch_heartbeat(&dir).expect("touch heartbeat");
        assert!(heartbeat_is_fresh(&dir, SystemTime::now()));
        assert!(!upgrader_is_declared_dead(&dir, SystemTime::now()));
        assert!(upgrade_in_flight(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_record_is_never_declared_dead() {
        let dir = temp_dir("none");
        assert!(!upgrader_is_declared_dead(&dir, SystemTime::now()));
        assert!(!upgrade_in_flight(&dir, SystemTime::now()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn identity_is_generated_once_and_stays_stable() {
        let dir = temp_dir("ident");
        let first = load_or_create_identity(&dir).expect("identity");
        assert!(first.starts_with("ident_"));
        assert_eq!(first.len(), "ident_".len() + 64);
        assert_eq!(
            first,
            load_or_create_identity(&dir).expect("identity"),
            "身份必须稳定（重启不变）"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_identity_is_an_error_not_a_silent_rotation() {
        let dir = temp_dir("ident-corrupt");
        std::fs::write(dir.join(IDENTITY_FILE), "garbage").expect("write");
        assert!(load_or_create_identity(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn instance_id_is_stable_and_scoped_to_gateway() {
        let dir = temp_dir("instance");
        let first = load_or_create_instance_id(&dir, "gw-x").expect("instance");
        assert!(first.starts_with("gw-x/inst-"));
        assert_eq!(
            first,
            load_or_create_instance_id(&dir, "gw-x").expect("instance")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn stored_credential() -> StoredCredential {
        StoredCredential {
            bundle: GatewayCredentialBundle {
                credential_id: "cred-1".into(),
                gateway_id: "gw-1".into(),
                instance_id: Some("gw-1/boot-1".into()),
                certificate: "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n".into(),
                ca_bundle: None,
                issued_at: "2026-10-04T00:00:00Z".into(),
                not_before: Some("2026-10-04T00:00:00Z".into()),
                not_after: Some("2026-11-04T00:00:00Z".into()),
            },
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nA\n-----END PRIVATE KEY-----\n".into(),
        }
    }

    #[test]
    fn credential_saves_and_loads() {
        let dir = temp_dir("cred");
        let credential = stored_credential();
        save_credential(&dir, &credential).expect("save");
        assert_eq!(load_credential(&dir), Some(credential.clone()));
        assert_eq!(
            credential_status(&dir),
            CredentialStatus::Present(credential)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stored_credential_builds_an_mtls_identity_pem() {
        let credential = stored_credential();
        let identity = credential.identity_pem();
        assert!(identity.contains("BEGIN CERTIFICATE"));
        assert!(identity.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn missing_and_corrupt_credentials_are_distinguished() {
        let dir = temp_dir("cred-status");
        assert_eq!(credential_status(&dir), CredentialStatus::Missing);
        std::fs::write(dir.join(CREDENTIAL_FILE), "{ not json").expect("write");
        assert!(matches!(
            credential_status(&dir),
            CredentialStatus::Corrupt(_)
        ));
        assert_eq!(load_credential(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upgrade_cursor_round_trips_and_defaults_to_empty() {
        let dir = temp_dir("cursor");
        assert_eq!(load_upgrade_cursor(&dir), UpgradeCursor::default());
        let cursor = UpgradeCursor {
            last_plan_id: Some("plan-1".into()),
            last_to_version: "0.2.0".into(),
        };
        save_upgrade_cursor(&dir, &cursor).expect("save cursor");
        assert_eq!(load_upgrade_cursor(&dir), cursor);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn single_instance_lock_excludes_a_second_holder() {
        let dir = temp_dir("lock");
        let first = acquire_single_instance_lock(&dir).expect("first lock");
        assert!(
            acquire_single_instance_lock(&dir).is_err(),
            "第二个持锁者必须被拒"
        );
        drop(first);
        assert!(acquire_single_instance_lock(&dir).is_ok(), "释放后可再拿锁");
        let _ = std::fs::remove_dir_all(dir);
    }
}
