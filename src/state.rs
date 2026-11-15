//! 落盘状态与**判死判据（唯一来源）**。
//!
//! 「网关 / 升级器死没死」的判据只在这里定义；[`crate::doctor`]、上报、升级记账都复用它 ——
//! 不许各写一套，否则「判定说死了、诊断说没事」就会打架。
//!
//! 同时承载本机的**身份与凭据**：网关身份 `ident_`（首跑自生成、中心不存）与**客户端证书+私钥**
//! （`register` 后签发，本进程是唯一持有者；私钥永不出本机）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use orion_error::prelude::*;
use ring::rand::{SecureRandom, SystemRandom};
use wist_control::GatewayCredentialBundle;

use crate::error::{StateError, StateReason, StateResult};

/// 心跳超过该时长即判死（与 `wist-agentd` 同量级）。
pub const UPGRADER_DEAD_AFTER: Duration = Duration::from_secs(60);

/// 续期提前量的**兜底**（秒）：只在证书没带 `not_before` / `not_after`（算不出有效期）时用。
/// 正常路径见 [`StoredCredential::renew_lead_seconds`]（= 本证书有效期的一半）。
pub const FALLBACK_RENEW_LEAD_SECS: i64 = 3600;

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
/// 客户端证书+私钥文件（`StoredCredential`，0600）—— 落在**身份目录**（`Config::identity_dir`），
/// 不是 `state_dir`：它是唯一真正不可再生的那对（丢了只能换 `gateway_id`），要进 `contract`。
pub const CREDENTIAL_FILE: &str = "credential.json";
/// link-upstream 返回的链接配置（信任锚 / 协议版本 / 注册 token 引用）。
pub const LINK_CONFIG_FILE: &str = "link-config.json";
/// 首跑置备遗留的**注册 token**：link-upstream 已签发、register 尚未消费时落盘，
/// 使 register 失败（网络）后下次可**免接入券**重试。
pub const REGIST_TOKEN_FILE: &str = "regist-token";
/// 业务身份（Center 绑定 → gwlinkd 落盘；数据面字段名 `tenant_id`）。
/// 不是密钥，放 `identity_dir` 以便随 `contract` 备份。
pub const BUSINESS_IDENTITY_FILE: &str = "business-identity.json";
/// **部署级**单实例锁文件（`state_dir` 下）：回答「**这份部署**在跑吗」。
/// 「同机只能有一个 gwlinkd」由机器级闸门 [`crate::single_instance`] 负责（与配置无关）。
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
    /// 本次任务执行记录的目录（`state_dir/tasks/<kind>-<id>-<ts>`）。
    ///
    /// 回执时从这里读日志摘要（失败回执才带）；落盘是为了**重启后**（自更新）
    /// 仍能把上一次的现场一并回执。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
    /// 本次任务 ID（`tasks/<kind>-<task_id>-<ts>/` 里的那个）；回执时一并上报。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
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
pub fn new_secret_token(prefix: &str) -> StateResult<String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| StateReason::Random.err("failed to read system random source"))?;
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

fn ensure_dir(state_dir: &Path) -> StateResult<()> {
    std::fs::create_dir_all(state_dir)
        .source_err(
            StateReason::Io,
            format!("创建状态目录失败 {}", state_dir.display()),
        )
        .map_err(StateError::from)
}

/// 原子写：**先写临时文件 → rename**（读者永不会看到半截内容）。
fn write_atomic(path: &Path, content: &str) -> StateResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).source_err(
            StateReason::Io,
            format!("创建目录失败 {}", parent.display()),
        )?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .source_err(StateReason::Io, format!("写入失败 {}", tmp.display()))?;
    // 非密钥文件（升级簿记 / 业务身份）：显式 `0644`，不随服务进程的 umask 漂
    // （systemd 常设 `UMask=0077`，否则会变成 0600，与设计 §8.1 不一致）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))
            .source_err(StateReason::Io, format!("设置权限失败 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .source_err(StateReason::Io, format!("落盘失败 {}", path.display()))
        .map_err(StateError::from)
}

/// 写敏感文件：原子写 + 中间文件设 `0600`（崩溃不会留半截文件把凭据写坏）。
fn write_secret(path: &Path, content: &str) -> StateResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).source_err(
            StateReason::Io,
            format!("创建目录失败 {}", parent.display()),
        )?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .source_err(StateReason::Io, format!("写入失败 {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .source_err(StateReason::Io, format!("设权限失败 {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .source_err(StateReason::Io, format!("落盘失败 {}", path.display()))
        .map_err(StateError::from)
}

/// 部署级单实例锁：拿不到即说明**这份部署**已有常驻在跑。进程退出时由内核释放（无陈旧锁问题）。
///
/// 只管 `state_dir` 这一份部署；与配置无关的「同机一个」由 [`crate::single_instance`] 兑现。
pub struct LockGuard {
    _file: std::fs::File,
}

/// 获取单实例锁（`flock LOCK_EX|LOCK_NB`）。
pub fn acquire_single_instance_lock(state_dir: &Path) -> StateResult<LockGuard> {
    ensure_dir(state_dir)?;
    let path = path_in(state_dir, LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .source_err(StateReason::Io, format!("建锁文件失败 {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `flock` 只读 fd 的有效性；`file` 在本 guard 生命周期内存活。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(StateReason::AlreadyRunning
                .err("已有网关常驻在跑（拿不到单实例锁），拒绝启动第二个"));
        }
    }
    Ok(LockGuard { _file: file })
}

/// 单实例锁是否**已被持有**（= 已有 gwlinkd 在跑）。只读探测：拿得到锁就立刻释放并返回 `false`。
/// 锁文件不存在 → `false`（没在跑）。供 `service status` 判活。
pub fn is_running(state_dir: &Path) -> StateResult<bool> {
    let path = path_in(state_dir, LOCK_FILE);
    if !path.exists() {
        return Ok(false);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .source_err(
            StateReason::Io,
            format!("打开锁文件失败 {}", path.display()),
        )?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: 同 acquire_single_instance_lock；只探测，拿到随即解锁。
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(false);
        }
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

// ───────────────────────── 身份 / 实例 / 凭据 / 链接配置 ─────────────────────────

/// 读或生成网关身份 `ident_`（首跑自生成，落盘 `0600`；中心不存）。
pub fn load_or_create_identity(state_dir: &Path) -> StateResult<String> {
    let path = path_in(state_dir, IDENTITY_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let token = text.trim();
        if token.starts_with("ident_") {
            return Ok(token.to_string());
        }
        // 非空但不成形（不是空的首写中断）→ 拒绝静默换身份（换身份会导致重复置备）。
        if !token.is_empty() {
            return Err(StateReason::Corrupt.err(format!(
                "身份文件损坏 {}（内容不以 ident_ 开头）：修复或删除后重跑",
                path.display()
            )));
        }
    }
    let token = new_secret_token("ident")?;
    ensure_dir(state_dir)?;
    write_secret(&path, &token)?;
    Ok(token)
}

/// 读或生成本次运行的实例标识（注册用；重启保持稳定）。
pub fn load_or_create_instance_id(state_dir: &Path, gateway_id: &str) -> StateResult<String> {
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    pub fn certificate_serial_hex(&self) -> StateResult<String> {
        use x509_parser::prelude::{FromDer, X509Certificate};
        let (_, pem) = x509_parser::pem::parse_x509_pem(self.bundle.certificate.as_bytes())
            .map_err(|err| StateReason::Corrupt.err(format!("解析客户端证书 PEM 失败: {err}")))?;
        let (_, cert) = X509Certificate::from_der(&pem.contents)
            .map_err(|err| StateReason::Corrupt.err(format!("解析客户端证书 DER 失败: {err}")))?;
        Ok(hex_lower(cert.raw_serial()))
    }

    /// 续期提前量（秒）：显式配置优先，否则取**本证书有效期的一半**。
    ///
    /// 为什么是一半而不是固定 1 小时：中心签的客户端证书是 30 天，而**过期即不可再续**
    /// （`credentials:renew` 要用旧证书证明身份，中心在握手期就按有效期拒）。提前量太短时，
    /// 「到期前那一小段」只要机器休眠 / 网络或中心不可达，证书就永久失效、只能换 `gateway_id` 重接。
    /// 取一半 ⇒ 有半个有效期的离线容忍（30 天证书 = 15 天）。
    ///
    /// 显式配置**只能用来缩短**：
    /// - `> 半个有效期` 会被夹到一半 —— 配大了（比如照 TTL 抄了 `2592000`）会在续期成功后
    ///   立即又满足条件，每拍都去换证（每次换证都作废上一张）；
    /// - `<= 0` 视为未配置 —— 否则等于把续期关掉，证书到期就永久失联。
    pub fn renew_lead_seconds(&self, configured: Option<i64>) -> i64 {
        let half = self
            .lifetime_seconds()
            .map(|lifetime| lifetime / 2)
            .filter(|half| *half > 0);
        match (configured.filter(|lead| *lead > 0), half) {
            (Some(lead), Some(half)) => lead.min(half),
            (Some(lead), None) => lead,
            (None, Some(half)) => half,
            (None, None) => FALLBACK_RENEW_LEAD_SECS,
        }
    }

    /// 证书自身有效期（秒）：`not_after - not_before`；算不出来或非正 → `None`。
    pub fn lifetime_seconds(&self) -> Option<i64> {
        let (not_before, not_after) = self.validity_unix()?;
        let lifetime = not_after - not_before;
        (lifetime > 0).then_some(lifetime)
    }

    /// 距离证书到期还剩多少秒（无可用有效期 → `i64::MAX`，不触发轮换）。
    pub fn seconds_remaining(&self) -> i64 {
        let Some((_, not_after)) = self.validity_unix() else {
            return i64::MAX;
        };
        (not_after - now_unix()).max(0)
    }

    /// 到期时刻（RFC3339）：与 [`Self::seconds_remaining`] **同源**（以证书本体为准），
    /// 供体检 / 上报展示 —— 不能一边说「已过期」一边把回执里那个错的 `not_after` 摆给人看。
    pub fn not_after_rfc3339(&self) -> Option<String> {
        let (_, not_after) = self.validity_unix()?;
        Some(
            chrono::DateTime::from_timestamp(not_after, 0)?
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
    }

    /// 证书有效期（unix 秒）：**以证书本体为准**，本体解析不出来才回落到回执里的
    /// `not_before`（缺了用 `issued_at`）/ `not_after`。
    ///
    /// 为什么不信回执：老中心可能不带 `not_after`，而「不知道有效期」会让续期与体检**都**
    /// 变成「一切正常且永不续期」—— 正是提前量要消灭的那种静默失效。
    fn validity_unix(&self) -> Option<(i64, i64)> {
        if let Some((not_before, not_after)) = self.cert_validity_unix() {
            return Some((not_before, not_after));
        }
        let not_after = self.bundle.not_after.as_ref()?.to_chrono().timestamp();
        let not_before = self
            .bundle
            .not_before
            .as_ref()
            .unwrap_or(&self.bundle.issued_at)
            .to_chrono()
            .timestamp();
        Some((not_before, not_after))
    }

    /// 从**证书本体**（DER）读有效期：解析失败（测试夹具 / 文件被裁）→ `None`。
    fn cert_validity_unix(&self) -> Option<(i64, i64)> {
        use x509_parser::prelude::{FromDer, X509Certificate};
        let (_, pem) = x509_parser::pem::parse_x509_pem(self.bundle.certificate.as_bytes()).ok()?;
        let (_, cert) = X509Certificate::from_der(&pem.contents).ok()?;
        let validity = cert.validity();
        let (not_before, not_after) = (
            validity.not_before.timestamp(),
            validity.not_after.timestamp(),
        );
        (not_after > not_before).then_some((not_before, not_after))
    }
}

/// 当前 unix 秒。
fn now_unix() -> i64 {
    wist_control::DateTime::now().to_chrono().timestamp()
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
///
/// 传**身份目录**（`Config::identity_dir`）—— 与 `state_dir` 里的锁 / 簿记分开。
pub fn save_credential(identity_dir: &Path, credential: &StoredCredential) -> StateResult<()> {
    let text =
        serde_json::to_string_pretty(credential).source_err(StateReason::Json, "序列化凭据失败")?;
    write_secret(&path_in(identity_dir, CREDENTIAL_FILE), &text)
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

/// 读长期身份的就位状态（见 [`CredentialStatus`]）。传**身份目录**。
pub fn credential_status(identity_dir: &Path) -> CredentialStatus {
    let path = path_in(identity_dir, CREDENTIAL_FILE);
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

/// 读长期身份；不存在、损坏或不可解析 → `None`。传**身份目录**。
pub fn load_credential(identity_dir: &Path) -> Option<StoredCredential> {
    match credential_status(identity_dir) {
        CredentialStatus::Present(credential) => Some(credential),
        _ => None,
    }
}

/// 保存待消费的 RegistToken（首跑 register 前落盘；register 成功或已失效时清）。
pub fn save_regist_token(state_dir: &Path, token: &str) -> StateResult<()> {
    write_secret(&path_in(state_dir, REGIST_TOKEN_FILE), token)
}

/// 中心信任锚（CA-S）落盘文件名（PEM）。落在**身份目录**（与凭据同处 —— 都是现场事实）。
pub const TRUST_BUNDLE_FILE: &str = "control-center.pem";

/// 落盘中心信任锚（CA-S PEM）：接入物从页面来时可免预置。返回落盘路径。传**身份目录**。
pub fn save_trust_bundle(identity_dir: &Path, pem: &str) -> StateResult<std::path::PathBuf> {
    let path = path_in(identity_dir, TRUST_BUNDLE_FILE);
    write_secret(&path, pem)?;
    Ok(path)
}

/// 读待消费的 RegistToken；不存在 / 空白 → `None`。
pub fn load_regist_token(state_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path_in(state_dir, REGIST_TOKEN_FILE)).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// 清掉待消费的 RegistToken（register 成功，或已确认失效）。幂等。
pub fn clear_regist_token(state_dir: &Path) {
    let _ = std::fs::remove_file(path_in(state_dir, REGIST_TOKEN_FILE));
}

/// 保存 link-upstream 返回的链接配置（信任锚 / 协议版本 / 注册 token 引用）—— 落盘留痕，不丢。
pub fn save_link_config(
    state_dir: &Path,
    config: &wist_control::GatewayInitialConfig,
) -> StateResult<()> {
    let text =
        serde_json::to_string_pretty(config).source_err(StateReason::Json, "序列化链接配置失败")?;
    write_secret(&path_in(state_dir, LINK_CONFIG_FILE), &text)
}

/// 业务身份（`tenant_id` + `revision`）：Center 绑定后由命令 / 初始配置带来。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BusinessIdentity {
    pub tenant_id: String,
    #[serde(default = "default_business_revision")]
    pub revision: i64,
    /// wparse 事件号的实例段（`WP_INS_ID`，100..=1123）。
    /// 旧中心 / 旧身份文件不带 → `None`（不动现场已有的实例号）。
    #[serde(default)]
    pub wp_ins_id: Option<i64>,
    /// 是否已写入 wparse `.env` 并触发重启；false = 只落盘、待生效。
    #[serde(default)]
    pub wparse_applied: bool,
}

fn default_business_revision() -> i64 {
    1
}

/// 读业务身份；文件不存在 / 解析失败 → `None`（旧现场还没绑定过是正常态）。
pub fn load_business_identity(identity_dir: &Path) -> Option<BusinessIdentity> {
    let text = std::fs::read_to_string(path_in(identity_dir, BUSINESS_IDENTITY_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 原子写业务身份（非密钥，默认权限）。
pub fn save_business_identity(identity_dir: &Path, identity: &BusinessIdentity) -> StateResult<()> {
    let text = serde_json::to_string_pretty(identity)
        .source_err(StateReason::Json, "序列化业务身份失败")?;
    write_atomic(&path_in(identity_dir, BUSINESS_IDENTITY_FILE), &text)
}

/// 读取已落盘的链接配置（link-upstream 返回）：用于遗留 RegistToken 的**免接入券**重试
/// （拿到当时的中心 endpoint）。文件不存在/解析失败 → `None`。
pub fn load_link_config(state_dir: &Path) -> Option<wist_control::GatewayInitialConfig> {
    let text = std::fs::read_to_string(path_in(state_dir, LINK_CONFIG_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

// ───────────────────────── 升级记录 / 心跳 / 游标 ─────────────────────────

/// 读升级记录；不存在或不可解析 → `None`。
pub fn read_upgrade_record(state_dir: &Path) -> Option<UpgradeRecord> {
    let text = std::fs::read_to_string(path_in(state_dir, UPGRADE_RECORD_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写升级记录（**原子**：诊断/判死都在读它，半截写会被当成「无升级」）。
pub fn write_upgrade_record(state_dir: &Path, record: &UpgradeRecord) -> StateResult<()> {
    let text =
        serde_json::to_string_pretty(record).source_err(StateReason::Json, "序列化升级记录失败")?;
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
pub fn save_upgrade_cursor(state_dir: &Path, cursor: &UpgradeCursor) -> StateResult<()> {
    let text =
        serde_json::to_string_pretty(cursor).source_err(StateReason::Json, "序列化升级游标失败")?;
    write_secret(&path_in(state_dir, UPGRADE_CURSOR_FILE), &text)
}

/// 打一次心跳（升级器进程周期调用）。
pub fn touch_heartbeat(state_dir: &Path) -> StateResult<()> {
    ensure_dir(state_dir)?;
    std::fs::write(path_in(state_dir, UPGRADE_HEARTBEAT_FILE), b"")
        .source_err(StateReason::Io, "写心跳失败")
        .map_err(StateError::from)
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
            log_dir: None,
            task_id: None,
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

    /// RFC3339 字面量 → 模型 `DateTime`（测试夹具用）。
    fn ts(value: &str) -> wist_control::DateTime {
        wist_control::DateTime::from_rfc3339(value).expect("rfc3339")
    }

    fn stored_credential() -> StoredCredential {
        StoredCredential {
            bundle: GatewayCredentialBundle {
                credential_id: "cred-1".into(),
                gateway_id: "gw-1".into(),
                instance_id: Some("gw-1/boot-1".into()),
                certificate: "-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n".into(),
                ca_bundle: None,
                issued_at: ts("2026-10-04T00:00:00Z"),
                not_before: Some(ts("2026-10-04T00:00:00Z")),
                not_after: Some(ts("2026-11-04T00:00:00Z")),
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
    fn certificate_serial_hex_reads_the_real_serial() {
        let cert = rcgen::generate_simple_self_signed(vec!["gw-1".to_string()]).expect("cert");
        let credential = StoredCredential {
            bundle: GatewayCredentialBundle {
                certificate: cert.cert.pem(),
                ..stored_credential().bundle
            },
            private_key_pem: "unused".into(),
        };
        let serial = credential.certificate_serial_hex().expect("serial");
        assert!(!serial.is_empty());
        assert!(serial.chars().all(|c| c.is_ascii_hexdigit()), "{serial}");
    }

    /// 有效期**以证书本体为准**：回执字段缺失/说谎时不能变成「永不续期」。
    #[test]
    fn validity_is_read_from_the_certificate_not_the_echoed_metadata() {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(vec!["gw-1".to_string()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2026, 2, 1); // 31 天前就已过期
        let cert = params.self_signed(&key).expect("cert");

        let credential = StoredCredential {
            bundle: GatewayCredentialBundle {
                certificate: cert.pem(),
                // 回执字段故意写错（老中心不带 / 元数据与证书不一致）。
                issued_at: ts("2999-01-01T00:00:00Z"),
                not_before: Some(ts("2999-01-01T00:00:00Z")),
                not_after: Some(ts("2999-02-01T00:00:00Z")),
                ..stored_credential().bundle
            },
            private_key_pem: "unused".into(),
        };

        assert_eq!(credential.lifetime_seconds(), Some(31 * 86_400));
        assert_eq!(
            credential.seconds_remaining(),
            0,
            "以证书准：2026-02-01 已过期"
        );
        // 证据字段也要同源：不能把回执里那个 2999 印出来。
        assert_eq!(
            credential.not_after_rfc3339().as_deref(),
            Some("2026-02-01T00:00:00Z")
        );
    }

    /// 真证书路径同样满足「刚签发不落窗内」（回执回落分支已有同名不变式）。
    #[test]
    fn a_der_backed_fresh_certificate_sits_outside_the_renewal_window() {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(vec!["gw-1".to_string()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2999, 1, 1);
        params.not_after = rcgen::date_time_ymd(2999, 2, 1);
        let cert = params.self_signed(&key).expect("cert");
        let credential = StoredCredential {
            bundle: GatewayCredentialBundle {
                certificate: cert.pem(),
                ..stored_credential().bundle
            },
            private_key_pem: "unused".into(),
        };
        assert_eq!(credential.lifetime_seconds(), Some(31 * 86_400));
        assert!(credential.seconds_remaining() > credential.renew_lead_seconds(None));
    }

    #[test]
    fn certificate_serial_hex_errors_on_a_non_certificate() {
        let credential = StoredCredential {
            bundle: GatewayCredentialBundle {
                certificate: "not a certificate".into(),
                ..stored_credential().bundle
            },
            private_key_pem: "unused".into(),
        };
        assert!(credential.certificate_serial_hex().is_err());
    }

    #[test]
    fn renew_lead_defaults_to_half_the_certificate_lifetime() {
        // 夹具：2026-10-04 → 2026-11-04（31 天）⇒ 提前量 15.5 天 = 1_339_200s。
        let credential = stored_credential();
        assert_eq!(credential.renew_lead_seconds(None), 1_339_200);
        // 显式配置只能**缩短**（现场可提前续，但不能把窗口拉到超过一半）。
        assert_eq!(credential.renew_lead_seconds(Some(60)), 60);
        assert_eq!(credential.renew_lead_seconds(Some(9_999_999)), 1_339_200);
        // `<= 0` 视为未配置：否则等于把续期关掉（证书到期即永久失效）。
        assert_eq!(credential.renew_lead_seconds(Some(0)), 1_339_200);
        assert_eq!(credential.renew_lead_seconds(Some(-60)), 1_339_200);
    }

    /// 按中心真实签发的形状（刚签发、30 天）构造：续期**不能**立刻又满足条件，
    /// 否则每拍都去换证（每次换证都作废上一张）。这是「取一半」能成立的支点。
    #[test]
    fn a_freshly_issued_certificate_sits_outside_the_renewal_window() {
        let mut credential = stored_credential();
        credential.bundle.issued_at = wist_control::DateTime::now();
        credential.bundle.not_before = Some(wist_control::DateTime::now());
        credential.bundle.not_after = Some(wist_control::DateTime::in_days(30));
        assert!(
            credential.seconds_remaining() > credential.renew_lead_seconds(None),
            "刚签发的证书不应落在续期窗口内"
        );
    }

    #[test]
    fn renew_lead_falls_back_without_a_usable_lifetime() {
        let mut credential = stored_credential();
        credential.bundle.not_after = None;
        assert_eq!(
            credential.renew_lead_seconds(None),
            FALLBACK_RENEW_LEAD_SECS
        );

        // 没有 `not_before` → 用 `issued_at` 算有效期（老中心不填 not_before）。
        credential.bundle.not_after = Some(ts("2026-11-04T00:00:00Z"));
        credential.bundle.not_before = None; // issued_at = 2026-10-04
        assert_eq!(credential.renew_lead_seconds(None), 1_339_200);

        // 有效期为负（数据坏了）→ 兜底，不给出奇怪的负提前量。
        credential.bundle.not_before = Some(ts("2026-12-04T00:00:00Z"));
        assert_eq!(
            credential.renew_lead_seconds(None),
            FALLBACK_RENEW_LEAD_SECS
        );
    }

    #[test]
    fn seconds_remaining_handles_none_past_and_future() {
        let mut credential = stored_credential();

        credential.bundle.not_after = None;
        assert_eq!(credential.seconds_remaining(), i64::MAX);

        credential.bundle.not_after = Some(ts("2000-01-01T00:00:00+00:00"));
        assert_eq!(credential.seconds_remaining(), 0, "已过期应夹到 0");

        credential.bundle.not_after = Some(ts("2999-01-01T00:00:00+00:00"));
        assert!(credential.seconds_remaining() > 0);
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
    fn regist_token_persists_loads_and_clears() {
        let dir = temp_dir("regist");
        assert_eq!(load_regist_token(&dir), None);

        save_regist_token(&dir, "reg_abc").expect("save");
        assert_eq!(load_regist_token(&dir).as_deref(), Some("reg_abc"));

        // 空白不算（与身份文件同口径）。
        save_regist_token(&dir, "   ").expect("save blank");
        assert_eq!(load_regist_token(&dir), None);

        save_regist_token(&dir, "reg_xyz").expect("save");
        clear_regist_token(&dir);
        assert_eq!(load_regist_token(&dir), None);
        clear_regist_token(&dir); // 幂等：再清一次不报错
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
    fn business_identity_round_trips_and_defaults_revision() {
        let dir = temp_dir("identity");
        assert_eq!(load_business_identity(&dir), None);
        let identity = BusinessIdentity {
            tenant_id: "tenant-a".into(),
            revision: 2,
            wp_ins_id: Some(101),
            wparse_applied: true,
        };
        save_business_identity(&dir, &identity).expect("save identity");
        assert_eq!(load_business_identity(&dir), Some(identity));
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
