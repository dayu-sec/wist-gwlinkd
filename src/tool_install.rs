//! **无状态工具**的进程内安装。
//!
//! `galaxy-ops` / `galaxy-flow` 这类工具没有状态、也不归属某个 gops 工程：升级就是
//! **解包制品 → 把二进制覆盖回它在 `PATH` 上的原位置**（旧版先备份，可回滚）。这条路径
//! **不经 gops**（不需要 `ops-prj.yml` 工程根），也**不重启网关容器** —— 因此驱动对它
//! **不做成功佐证**（自述面与工具无关），见 `crate::upgrade`。
//!
//! 组件目录（组件名 → 二进制名）在 `gwlinkd.toml` 的 `[[upgrade.component]]` 里配
//! （`install = "tool-copy"`）。见 CR-003。
//!
//! **架构护栏**：制品是平台专用的，覆盖到错架构的宿主上不会报错、只会让工具静默报废。
//! 因此解包覆盖前先在 [`crate::target`] 里核 target-triple：不符**必拒**；读不出架构默认拒
//! （配置 `upgrade_tool_require_arch = false` 可放行不可校验的制品）。
//!
//! **摘要校验**：解包覆盖前先核制品 sha256。期望值来自两处 —— 中心契约带的
//! `artifact_sha256`（优先），或制品名本身的内容寻址前缀（`pkg-<sha256 前 16 位>`）。
//! 不符**必拒**（绝不用来路不明的字节覆盖本机工具）。

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use orion_error::prelude::*;
use wist_artifact::digest::{parse_digest, sha256_hex_bytes};
use wist_artifact::source::{FETCH_TIMEOUT, MAX_ARTIFACT_BYTES, read_source_with_client};

use crate::error::{UpgradeReason, UpgradeResult};
use crate::target::{self, ArchVerdict, HostTarget};

/// 旧版二进制的备份子目录（相对状态目录）。
const BACKUP_DIR: &str = "tool-backups";
/// 解包暂存子目录（相对状态目录）。
const STAGING_DIR: &str = "tool-staging";

/// `locate_binary` 扫描的最大目录深度。
///
/// 正规布局是 `<name>-<version>-<triple>/<binary>`（1 层）；放宽到足够深以免漏掉多套一层的
/// 布局（`<env>/<name>-<version>-<triple>/<binary>` 之类），同时仍给扫描一个上界。
const MAX_SCAN_DEPTH: usize = 16;

/// 无状态工具安装器：组件名 → 二进制名，据此**定位原位置并就地覆盖**。
#[derive(Debug, Clone)]
pub struct ToolInstaller {
    /// 组件名 → 二进制名（`PATH` 上定位 + 就地覆盖都据它）。
    binaries: BTreeMap<String, String>,
    /// 本机状态目录（备份与解包暂存都在其子目录里）。
    state_dir: PathBuf,
    /// 取制品的 HTTP 客户端（带信任锚 / 客户端证书 —— 自签的中心才拉得到）。
    http: reqwest::Client,
    /// 本机 target（架构护栏据它判「制品能否装本机」）。
    host: HostTarget,
    /// 是否**要求制品架构可校验且与本机一致**（缺省 `true`）。
    require_verified_arch: bool,
}

impl ToolInstaller {
    /// 建安装器：给定组件目录、状态目录与取制品的 HTTP 客户端。
    ///
    /// 本机 target 由 [`HostTarget::detect`] 现探；架构校验缺省**开启**。
    pub fn new(
        binaries: BTreeMap<String, String>,
        state_dir: impl Into<PathBuf>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            binaries,
            state_dir: state_dir.into(),
            http,
            host: HostTarget::detect(),
            require_verified_arch: true,
        }
    }

    /// 钉一个本机 target（测试 / 交叉场景用）。
    pub fn with_host_target(mut self, host: HostTarget) -> Self {
        self.host = host;
        self
    }

    /// 设置是否要求制品架构可校验（缺省 `true`）。
    ///
    /// `false` 只放宽「读不出架构」这一种：**已识别出的架构与本机不符仍拒**。
    pub fn with_require_verified_arch(mut self, require: bool) -> Self {
        self.require_verified_arch = require;
        self
    }

    /// 本安装器是否管该组件（即目录里标了 `install = "tool-copy"`）。
    pub fn handles(&self, component: &str) -> bool {
        self.binaries.contains_key(component)
    }

    /// 发之前的前置校验：组件有目录项、`binary` 非空、且在 `PATH` 上能定位到**原位置**。
    ///
    /// 返回 `(二进制名, 原位置)`。任何一步不成立 → `Err(可读原因)`（**别发** —— 发出去只会
    /// 以不可读的方式失败），驱动把它落成可读失败并回执。
    pub fn preflight(&self, component: &str) -> UpgradeResult<(String, PathBuf)> {
        let binary = self.binaries.get(component).ok_or_else(|| {
            UpgradeReason::Preflight.err(format!("组件 {component} 不在本机组件目录里"))
        })?;
        if binary.trim().is_empty() {
            return Err(UpgradeReason::Preflight.err(format!(
                "组件 {component} 标了 install=tool-copy 却没配 binary：无法定位要覆盖的二进制"
            )));
        }
        let existing = which(binary).ok_or_else(|| {
            UpgradeReason::Preflight.err(format!(
                "PATH 上找不到 {binary} 的原位置：无法就地覆盖安装（工具需已存在于 PATH）"
            ))
        })?;
        Ok((binary.clone(), existing))
    }

    /// 进程内安装（不额外声明期望摘要）：取制品 → 核摘要 → 核架构 → 解包 → 覆盖（旧版备份）。
    ///
    /// 期望摘要仍会从**内容寻址的制品名**（`pkg-<hex16>`）推出并校验；只是不额外声明。
    pub async fn install(&self, component: &str, artifact: &str) -> UpgradeResult<String> {
        self.install_with_digest(component, artifact, None).await
    }

    /// 同 [`Self::install`]，但可额外带**中心契约的期望摘要**（`artifact_sha256`）。
    ///
    /// 期望摘要取值优先级：`expected_sha256`（显式给了就用它，形态不对即拒）＞制品名里的
    /// 内容寻址前缀（`pkg-<sha256 前 16 位>`）。实得 sha256 与之不符 → **拒装**（不覆盖）。
    ///
    /// 成功返回可读说明（含原位置、制品内来源、备份路径、摘要与架构核对结果）。
    pub async fn install_with_digest(
        &self,
        component: &str,
        artifact: &str,
        expected_sha256: Option<&str>,
    ) -> UpgradeResult<String> {
        let (binary, existing) = self.preflight(component)?;

        // 取件来源必须是可取形态：本机**绝对路径**或 `http(s)://` URL。中心没派制品地址时
        // 驱动会退化成裸版本串（`v0.16.1-alpha`），那会被当 URL 解析而失败 —— 提前报可读错。
        if !is_fetchable_source(artifact) {
            return Err(UpgradeReason::Preflight.err(format!(
                "组件 {component} 没有可取的制品地址（来源={artifact:?}）：tool-copy 依赖中心派发的 \
                 release 制品（https://… 或 /abs/path）；裸版本串不是 URL"
            )));
        }

        let bytes =
            read_source_with_client(&self.http, artifact, MAX_ARTIFACT_BYTES, FETCH_TIMEOUT)
                .await
                .map_err(|err| {
                    UpgradeReason::Artifact.err(format!("取制品失败 {artifact}: {err}"))
                })?;

        // **摘要校验**：解包覆盖前先核 —— 不符的字节绝不能拿去覆盖本机工具。
        let sha_note = verify_expected_digest(&bytes, artifact, expected_sha256, &binary)?;

        // **架构护栏**：解包覆盖前先核 —— 错架构的二进制覆盖上去不会报错，只会让工具静默报废。
        // 只在「确实是 tar.gz」时核；不是 tar.gz 交给下面解包报「解包失败」。
        let mut arch_note = String::new();
        if target::has_tar_entry(&bytes) {
            let triple = target::artifact_triple(&bytes, artifact);
            match target::check_artifact_arch(triple.as_deref(), &self.host) {
                ArchVerdict::Match => {
                    if let Some(triple) = &triple {
                        arch_note = format!("（架构 {triple} 已核）");
                    }
                }
                ArchVerdict::Mismatch(reason) => {
                    return Err(
                        UpgradeReason::Arch.err(format!("架构校验失败，未覆盖 {binary}：{reason}"))
                    );
                }
                ArchVerdict::Unverifiable(reason) => {
                    if self.require_verified_arch {
                        return Err(UpgradeReason::Arch.err(format!(
                            "架构不可校验，未覆盖 {binary}：{reason}（确认制品与本机相符可在配置里设 \
                             upgrade_tool_require_arch = false 放行）"
                        )));
                    }
                    log::warn!("event=ToolArchUnverified component={component} {reason}");
                    arch_note = format!("（架构不可校验：{reason}）");
                }
            }
        }

        let backup_dir = self.state_dir.join(BACKUP_DIR);
        let staging = self.state_dir.join(STAGING_DIR).join(format!(
            "{}-{}-{}",
            sanitize(component),
            std::process::id(),
            now_stamp(),
        ));
        // 解包 / 备份 / 覆盖都是**阻塞**文件操作，搬到 blocking 线程，别占住 async worker。
        let detail = tokio::task::spawn_blocking(move || {
            let result = replace_binary(&bytes, &staging, &binary, &existing, &backup_dir);
            let _ = std::fs::remove_dir_all(&staging);
            result
        })
        .await
        .map_err(|err| UpgradeReason::Executor.err(format!("安装任务异常: {err}")))??;
        Ok(format!("{detail}{sha_note}{arch_note}"))
    }
}

/// 期望摘要：完整 64 hex（中心契约）或内容寻址前缀 16 hex（制品名自带）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedDigest {
    Full(String),
    Prefix(String),
}

impl ExpectedDigest {
    /// 进错误 / 说明文案的形态（前缀会标明「前缀」）。
    fn describe(&self) -> String {
        match self {
            ExpectedDigest::Full(hex) => hex.clone(),
            ExpectedDigest::Prefix(prefix) => format!("前缀 {prefix}"),
        }
    }
}

/// 校验制品摘要（不符即拒）。返回成功时的说明（无期望摘要时空串）。
fn verify_expected_digest(
    bytes: &[u8],
    source: &str,
    explicit: Option<&str>,
    binary: &str,
) -> UpgradeResult<String> {
    let Some(expected) = resolve_expected_digest(source, explicit)? else {
        return Ok(String::new());
    };
    let actual = sha256_hex_bytes(bytes);
    let ok = match &expected {
        ExpectedDigest::Full(hex) => &actual == hex,
        ExpectedDigest::Prefix(prefix) => actual.starts_with(prefix),
    };
    if !ok {
        return Err(UpgradeReason::Artifact.err(format!(
            "制品摘要不符，未覆盖 {binary}：期望 {}，实得 {actual}",
            expected.describe()
        )));
    }
    Ok(format!("（sha256 {} 已核）", expected.describe()))
}

/// 解析出期望摘要：显式给的优先；否则从内容寻址的制品名推前缀（`pkg-<hex16>`）。
fn resolve_expected_digest(
    source: &str,
    explicit: Option<&str>,
) -> UpgradeResult<Option<ExpectedDigest>> {
    if let Some(value) = explicit {
        // 中心给的该是完整摘要；形态不对就拒（别把笔误当「没给」静默放过）。
        let hex = parse_digest(value).map_err(|err| {
            UpgradeReason::Artifact.err(format!("期望摘要形态不对（{value}）：{err}"))
        })?;
        return Ok(Some(ExpectedDigest::Full(hex)));
    }
    Ok(content_addressed_prefix(source).map(ExpectedDigest::Prefix))
}

/// 制品名是内容寻址 id（`pkg-<sha256 前 16 位>` / `kbp-…`）时，取那段 hex 前缀。
fn content_addressed_prefix(source: &str) -> Option<String> {
    let base = target::source_basename(source);
    for prefix in ["pkg-", "kbp-"] {
        if let Some(rest) = base.strip_prefix(prefix)
            && rest.len() == 16
            && rest.chars().all(|ch| ch.is_ascii_hexdigit())
        {
            return Some(rest.to_ascii_lowercase());
        }
    }
    None
}

/// 解包 → 定位二进制 → 备份旧版 → **原子覆盖**。
fn replace_binary(
    bytes: &[u8],
    staging: &Path,
    binary: &str,
    existing: &Path,
    backup_dir: &Path,
) -> UpgradeResult<String> {
    // 解包到干净的暂存目录。
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).source_err(
        UpgradeReason::Io,
        format!("建解包暂存目录失败 {}", staging.display()),
    )?;
    unpack_into(bytes, staging)?;
    // 包里按**文件名的末段**找（`binary` 配名字或绝对路径都行 —— 以后者定位时不能整串拿去比）。
    let package_name = existing
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(binary);
    let source = locate_binary(staging, package_name)?;

    // 覆盖前先备份旧版（`fs::copy` 保留权限位）；备份放状态目录，升级后可回滚。
    std::fs::create_dir_all(backup_dir).source_err(
        UpgradeReason::Io,
        format!("建备份目录失败 {}", backup_dir.display()),
    )?;
    // 解析软链：覆盖的是**真身**，不是链接本身（否则会把链接换成普通文件）。
    let target = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    // 备份名用**末段**（`binary` 可能是绝对路径，直接入名会把 `join` 拉回绝对路径）。
    let backup = backup_dir.join(format!("{package_name}.{}", now_stamp()));
    std::fs::copy(&target, &backup).map_err(|err| {
        UpgradeReason::Io.err(format!(
            "备份旧版失败 {} -> {}: {err}",
            target.display(),
            backup.display()
        ))
    })?;

    // 原子替换：同目录写临时文件（同文件系统）→ rename 覆盖，读者要么看到旧、要么看到新。
    let new_bytes = std::fs::read(&source).source_err(
        UpgradeReason::Io,
        format!("读制品里的二进制失败 {}", source.display()),
    )?;
    let dir = target.parent().ok_or_else(|| {
        UpgradeReason::Io.err(format!("目标路径没有父目录：{}", target.display()))
    })?;
    let tmp = dir.join(format!(".{package_name}.new.{}", std::process::id()));
    // 写临时文件 → 置可执行位 → 原子 rename；**任一步失败都收走临时文件**（别留 .gops.new.<pid> 残骸）。
    let staged = std::fs::write(&tmp, &new_bytes).and_then(|()| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &target)
    });
    if let Err(err) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(UpgradeReason::Io.err(format!(
            "原地替换失败 {}（临时文件 {}，目标目录不可写？）: {err}",
            target.display(),
            tmp.display()
        )));
    }
    Ok(format!(
        "已就地覆盖 {package_name}：{} ← {}（旧版备份 {}）",
        target.display(),
        source.display(),
        backup.display()
    ))
}

/// gzip + tar 解包到 `staging`（`tar::Archive::unpack` 自带路径穿越防护）。
///
/// 路径穿越条目的行为需明确：`tar` 的 `unpack` 对 `..` / 绝对路径这类会**逃出目标目录**的条目
/// **静默跳过**（不报错，也不写入目标之外）—— 见 `unpack_does_not_write_outside_the_staging_dir`。
fn unpack_into(bytes: &[u8], staging: &Path) -> UpgradeResult<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(staging)
        .map_err(|err| UpgradeReason::Artifact.err(format!("解包失败（期望 tar.gz 制品）：{err}")))
}

/// 来源是否为**可取**形态：本机绝对路径（以 `/` 开头）或 `http(s)://` URL。
fn is_fetchable_source(source: &str) -> bool {
    source.starts_with('/') || source.starts_with("http://") || source.starts_with("https://")
}

/// 在解包结果里**按名字**找二进制（广度优先，最多 [`MAX_SCAN_DEPTH`] 层）：覆盖裸二进制与
/// `<name>-<version>-<triple>/<binary>` 两种布局。**只认普通文件**（符号链接作候选会被跳过，
/// 免得恶意 / 异常包用它把源指到包外）。找不到 → 列出包内文件帮助排错。
fn locate_binary(root: &Path, name: &str) -> UpgradeResult<PathBuf> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen: Vec<String> = Vec::new();
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < MAX_SCAN_DEPTH {
                    queue.push_back((path, depth + 1));
                }
                continue;
            }
            // 只把**普通文件**当候选：符号链接可能被恶意 / 异常包用来指到包外（读别处的文件当源）。
            if file_type.is_file() && path.file_name().and_then(|name| name.to_str()) == Some(name)
            {
                return Ok(path);
            }
            if seen.len() < 32 {
                seen.push(relative(root, &path));
            }
        }
    }
    Err(UpgradeReason::Artifact.err(format!(
        "制品里找不到二进制 {name}（包内文件：{}）",
        if seen.is_empty() {
            "（空）".to_string()
        } else {
            seen.join(", ")
        }
    )))
}

/// 相对 `root` 展示路径（不在其下就原样）。
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// `which` 等价：`binary` 含分隔符就当路径；否则按 `PATH` 逐目录找**可执行常规文件**。
///
/// 不引外部 `whereis` / `which` 进程：一是它们找的是各自的内置目录集、未必含 `PATH`，
/// 二是本函数要能单测。要求可执行位是为了不把 `PATH` 上靠前的**同名非可执行文件**误当原位置覆盖。
fn which(binary: &str) -> Option<PathBuf> {
    if binary.contains('/') {
        let path = PathBuf::from(binary);
        return is_executable_file(&path).then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable_file(candidate))
}

/// 在 `PATH`（或绝对路径）上定位二进制的**原位置** —— 诊断用；判据与安装前的
/// [`ToolInstaller::preflight`] 一致（含「需可执行位」），免得诊断与安装各说各话。
pub fn which_binary(binary: &str) -> Option<PathBuf> {
    which(binary)
}

/// 常规文件、且在 Unix 上带**可执行位**（非 Unix 不查位）。
fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 把组件名洗成安全的暂存目录名。
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// 当前 Unix **纳秒**时间戳（备份文件名 / 暂存目录后缀用）。
///
/// 用纳秒而非秒：同一秒内的两次安装（不同组件、或前后重驱）也不撞名。
fn now_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-tool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// 造一个 tar.gz：包内顶层目录 `<component>-<version>-<triple>/<binary>` = payload。
    fn tool_archive(entry: &str, payload: &[u8]) -> Vec<u8> {
        archive_with(&[(entry, payload)])
    }

    /// 造一个 tar.gz：任意多条目（`(包内路径, 内容)`）。
    fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, payload) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(payload.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, *name, *payload)
                    .expect("append tar entry");
            }
            builder.finish().expect("finish tar");
        }
        gzip(&tar_bytes)
    }

    /// 造一个 tar.gz，首个条目名字**直接写 header 字节**（绕过 `append_data` 对 `..` 的拒绝），
    /// 用来模拟恶意 / 异常制品；其余条目正常。
    fn archive_with_raw_name(
        raw_name: &str,
        raw_payload: &[u8],
        entries: &[(&str, &[u8])],
    ) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(raw_payload.len() as u64);
            header.set_mode(0o644);
            {
                let bytes = header.as_mut_bytes();
                let name = raw_name.as_bytes();
                bytes[..name.len()].copy_from_slice(name);
                for slot in &mut bytes[name.len()..100] {
                    *slot = 0;
                }
            }
            header.set_cksum();
            builder
                .append(&header, raw_payload)
                .expect("append raw entry");
            for (name, payload) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(payload.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, *name, *payload)
                    .expect("append tar entry");
            }
            builder.finish().expect("finish tar");
        }
        gzip(&tar_bytes)
    }

    fn gzip(tar_bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, tar_bytes).expect("gzip write");
        encoder.finish().expect("gzip finish")
    }

    /// 测试用假宿主（macOS arm64）—— 与多数用例的制品三元组对齐。
    fn host_aarch64_macos() -> HostTarget {
        HostTarget::new("aarch64", "macos", true)
    }

    /// 建一个装了工具的最小安装器（`binary` 用**绝对路径**，避开 `PATH` 依赖）；钉上 macOS arm64 假宿主。
    fn installer(dir: &Path, component: &str, binary: &Path) -> ToolInstaller {
        ToolInstaller::new(
            BTreeMap::from([(component.to_string(), binary.to_string_lossy().to_string())]),
            dir.to_path_buf(),
            reqwest::Client::new(),
        )
        .with_host_target(host_aarch64_macos())
    }

    /// 写一个**可执行**文件（`which` 要求可执行位；否则前置就拿不到原位置）。
    fn write_exec(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    #[test]
    fn preflight_reports_a_missing_binary_as_a_readable_failure() {
        let dir = temp_dir("preflight-missing");
        let installer = ToolInstaller::new(
            BTreeMap::from([("galaxy-ops".to_string(), "/nonexistent/gops".to_string())]),
            dir.clone(),
            reqwest::Client::new(),
        );
        // 目录里没有的组件 → 拒。
        assert!(installer.preflight("galaxy-flow").is_err());
        // 目录里有、但原位置不存在 → 拒（别发）。
        let err = installer
            .preflight("galaxy-ops")
            .expect_err("missing binary");
        assert!(err.contains("PATH"), "{err}");
        // 空 binary → 拒。
        let blank = ToolInstaller::new(
            BTreeMap::from([("galaxy-ops".to_string(), String::new())]),
            dir.clone(),
            reqwest::Client::new(),
        );
        assert!(blank.preflight("galaxy-ops").is_err());
        assert!(installer.handles("galaxy-ops"));
        assert!(!installer.handles("galaxy-flow"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn replace_binary_backs_up_then_overwrites_in_place() {
        let dir = temp_dir("replace");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let target = bin_dir.join("gops");
        std::fs::write(&target, b"OLD").expect("old");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
                .expect("mode");
        }

        let bytes = tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW");
        let staging = dir.join("staging");
        let backup_dir = dir.join("backups");
        let detail =
            replace_binary(&bytes, &staging, "gops", &target, &backup_dir).expect("install");

        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "新二进制应带可执行位");
        }
        // 备份里是旧内容。
        let backups: Vec<_> = std::fs::read_dir(&backup_dir)
            .expect("backups")
            .flatten()
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).expect("backup"), b"OLD");
        assert!(detail.contains("旧版备份"), "{detail}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn locate_binary_finds_nested_and_reports_when_missing() {
        let dir = temp_dir("locate");
        let nested = dir.join("gx-0.15.1-x86_64-unknown-linux-gnu");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(nested.join("gx"), b"bin").expect("gx");
        assert_eq!(locate_binary(&dir, "gx").expect("found"), nested.join("gx"));
        let err = locate_binary(&dir, "gops").expect_err("missing");
        assert!(
            err.contains("gx-0.15.1-x86_64-unknown-linux-gnu/gx"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn install_reads_a_local_artifact_and_overwrites_the_binary() {
        let dir = temp_dir("install");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let target = bin_dir.join("gx");
        write_exec(&target, b"OLD");

        // 制品用**本机绝对路径**：`read_source_with_client` 直接读文件，测试不触网。
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-gnu/gx", b"NEW"),
        )
        .expect("artifact");

        let installer = ToolInstaller::new(
            BTreeMap::from([(
                "galaxy-flow".to_string(),
                target.to_string_lossy().to_string(),
            )]),
            dir.clone(),
            reqwest::Client::new(),
        )
        .with_host_target(HostTarget::new("x86_64", "linux", true));
        let detail = installer
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect("install");
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        assert!(detail.contains("已就地覆盖 gx"), "{detail}");
        assert!(
            detail.contains("架构 x86_64-unknown-linux-gnu 已核"),
            "{detail}"
        );
        // 暂存目录用完即清（`tool-staging` 下不留残留）。
        let leftover = std::fs::read_dir(dir.join(STAGING_DIR))
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftover, 0, "暂存目录应清干净");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn locate_binary_finds_a_bare_top_level_binary() {
        let dir = temp_dir("locate-bare");
        std::fs::write(dir.join("gops"), b"bin").expect("write");
        assert_eq!(
            locate_binary(&dir, "gops").expect("found"),
            dir.join("gops")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `which` 要求**可执行位**：非可执行的同名文件不算原位置（不误覆盖 PATH 上的别的文件）。
    #[cfg(unix)]
    #[test]
    fn which_requires_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("which-exec");
        let non_exec = dir.join("plain-gops");
        std::fs::write(&non_exec, b"x").expect("write");
        std::fs::set_permissions(&non_exec, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(which(&non_exec.to_string_lossy()).is_none(), "非可执行不算");

        let exec = dir.join("exec-gops");
        write_exec(&exec, b"x");
        assert_eq!(
            which(&exec.to_string_lossy()).as_deref(),
            Some(exec.as_path())
        );
        // 绝对不存在的路径 → None。
        assert!(which("/nonexistent/nope").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 制品的路径穿越条目**不得写到暂存目录之外**（无论 tar 是跳过还是报错）。
    #[test]
    fn unpack_does_not_write_outside_the_staging_dir() {
        let dir = temp_dir("escape");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        std::fs::write(&target, b"OLD").expect("old");

        let bytes = archive_with_raw_name("../evil.txt", b"pwned", &[("pkg/gops", b"NEW")]);
        let staging = dir.join("staging");
        let backup_dir = dir.join("backups");
        match replace_binary(&bytes, &staging, "gops", &target, &backup_dir) {
            Ok(detail) => {
                assert!(detail.contains("已就地覆盖"), "{detail}");
                assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
            }
            // tar 拒绝穿越条目也算合格：只要没写到外面。
            Err(err) => assert!(err.contains("解包失败"), "{err}"),
        }
        assert!(!dir.join("evil.txt").exists(), "不得逃出暂存目录");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 软链：覆盖的是**真身**，链接本身保持是链接（不被换成普通文件）。
    #[cfg(unix)]
    #[tokio::test]
    async fn install_follows_a_symlink_and_keeps_it() {
        let dir = temp_dir("symlink");
        let real_dir = dir.join("real");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&real_dir).expect("real");
        std::fs::create_dir_all(&bin_dir).expect("bin");
        let real = real_dir.join("gops");
        write_exec(&real, b"OLD");
        let link = bin_dir.join("gops");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let artifact = dir.join("galaxy-ops.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW"),
        )
        .expect("artifact");
        installer(&dir, "galaxy-ops", &link)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect("install");

        assert_eq!(std::fs::read(&real).expect("read"), b"NEW", "真身被覆盖");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("meta")
                .file_type()
                .is_symlink(),
            "链接仍是链接"
        );
        assert_eq!(std::fs::read_link(&link).expect("target"), real);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 非 tar.gz 制品：可读失败，且**旧二进制原封不动**（解包在建备份/覆盖之前做）。
    #[tokio::test]
    async fn install_fails_readably_when_the_artifact_is_not_a_tarball() {
        let dir = temp_dir("bad-artifact");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("not-a-tarball.tar.gz");
        std::fs::write(&artifact, b"this is not gzip").expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect_err("must fail");
        assert!(err.contains("解包失败"), "{err}");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"OLD",
            "旧版未被破坏"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 制品取不到（路径不存在）：可读失败。
    #[tokio::test]
    async fn install_fails_readably_when_the_artifact_is_missing() {
        let dir = temp_dir("missing-artifact");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("does-not-exist.tar.gz");

        let err = installer(&dir, "galaxy-flow", &target)
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect_err("must fail");
        assert!(err.contains("取制品失败"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 包里的二进制名与目标**不同名** → 可读失败（列出包内文件），不误装。
    #[tokio::test]
    async fn install_fails_when_the_binary_name_is_absent_from_the_package() {
        let dir = temp_dir("wrong-name");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-ops.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/not-gops", b"NEW"),
        )
        .expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect_err("must fail");
        assert!(err.contains("找不到二进制 gops"), "{err}");
        assert!(
            err.contains("galaxy-ops-0.1.0-aarch64-apple-darwin/not-gops"),
            "错误里应列出包内文件：{err}"
        );
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"OLD",
            "旧版未被破坏"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 架构不符（x86_64 制品装上 arm64 宿主）：**拒装**，旧二进制原封不动。
    /// 这正是「装成功但工具静默报废」的破坏场景。
    #[tokio::test]
    async fn install_rejects_a_mismatched_arch_and_leaves_the_binary_intact() {
        let dir = temp_dir("arch-mismatch");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-musl/gx", b"NEW"),
        )
        .expect("artifact");

        // installer() 钉的是 macOS arm64 假宿主。
        let err = installer(&dir, "galaxy-flow", &target)
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect_err("mismatched arch must be rejected");
        assert!(err.contains("架构校验失败"), "{err}");
        assert!(err.contains("x86_64"), "{err}");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"OLD",
            "错架构制品绝不得覆盖旧二进制"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 读不出 target-triple（内容寻址名等）：缺省**拒装**；`require_verified_arch=false` 才放行。
    #[tokio::test]
    async fn install_rejects_an_unverifiable_arch_by_default_and_can_be_relaxed() {
        let dir = temp_dir("arch-unverifiable");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        // 包内首条目 `pkg/…` 与来源名都不带三元组（也不是内容寻址名）。
        let artifact = dir.join("pkg-unnamed");
        std::fs::write(&artifact, tool_archive("pkg/gops", b"NEW")).expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect_err("unverifiable arch must be rejected by default");
        assert!(err.contains("架构不可校验"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");

        // 显式放宽后才装得上，且成功说明里标注「架构不可校验」。
        let detail = installer(&dir, "galaxy-ops", &target)
            .with_require_verified_arch(false)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect("relaxed install");
        assert!(detail.contains("架构不可校验"), "{detail}");
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 只有裸版本串、没有制品地址（中心没派 release）：**可读失败**，而不是当 URL 误取。
    #[tokio::test]
    async fn install_rejects_a_bare_version_as_a_source() {
        let dir = temp_dir("bare-version");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");

        let err = installer(&dir, "galaxy-flow", &target)
            .install("galaxy-flow", "v0.16.1-alpha")
            .await
            .expect_err("bare version is not a fetchable source");
        assert!(err.contains("制品地址"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 深于四层的布局也能找到二进制（此前上限 4 层会漏）。
    #[test]
    fn locate_binary_finds_a_binary_deeper_than_four_levels() {
        let dir = temp_dir("locate-deep");
        let nested = dir.join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::write(nested.join("gops"), b"bin").expect("gops");
        assert_eq!(
            locate_binary(&dir, "gops").expect("found"),
            nested.join("gops")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 放宽架构要求只放宽「读不出」：**已识别出的不符仍拒**（防呆不防懒）。
    #[tokio::test]
    async fn a_relaxed_arch_requirement_still_rejects_a_recognized_mismatch() {
        let dir = temp_dir("arch-relaxed-mismatch");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-musl/gx", b"NEW"),
        )
        .expect("artifact");

        let err = installer(&dir, "galaxy-flow", &target)
            .with_require_verified_arch(false)
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect_err("a recognized mismatch is rejected even when relaxed");
        assert!(err.contains("架构校验失败"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 同为 aarch64、但制品是 Linux → macOS 宿主**拒装**（OS 也必须对上）。
    #[tokio::test]
    async fn install_rejects_a_matching_arch_but_a_different_os() {
        let dir = temp_dir("arch-os-mismatch");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-aarch64-unknown-linux-musl/gx", b"NEW"),
        )
        .expect("artifact");

        let err = installer(&dir, "galaxy-flow", &target)
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect_err("os mismatch must be rejected");
        assert!(err.contains("架构校验失败"), "{err}");
        assert!(err.contains("linux"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 包内首条目不带身份时，回落**来源末段**读三元组（内容寻址存盘也会带原名）。
    #[tokio::test]
    async fn install_falls_back_to_the_source_name_for_the_triple() {
        let dir = temp_dir("arch-source-name");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        // 包内顶层是 `pkg/`（读不出），但文件名带目标三元组。
        let artifact = dir.join("gops-0.1.0-aarch64-apple-darwin.tar.gz");
        std::fs::write(&artifact, tool_archive("pkg/gops", b"NEW")).expect("artifact");

        let detail = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect("install via source-name triple");
        assert!(
            detail.contains("架构 aarch64-apple-darwin 已核"),
            "{detail}"
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 架构拒装发生在**建暂存目录之前**：不留下解包残骸。
    #[tokio::test]
    async fn an_arch_rejection_leaves_no_staging_dir() {
        let dir = temp_dir("arch-no-staging");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-musl/gx", b"NEW"),
        )
        .expect("artifact");

        installer(&dir, "galaxy-flow", &target)
            .install("galaxy-flow", &artifact.to_string_lossy())
            .await
            .expect_err("mismatch");
        assert!(!dir.join(STAGING_DIR).exists(), "拒装不该创建暂存目录");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 放宽架构要求后，**真正的**定位失败照旧报出来（不被架构放松掩盖）。
    #[tokio::test]
    async fn a_relaxed_unverifiable_arch_still_reports_a_missing_binary() {
        let dir = temp_dir("arch-relaxed-missing-bin");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("pkg-unnamed");
        std::fs::write(&artifact, tool_archive("pkg/not-gops", b"NEW")).expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .with_require_verified_arch(false)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect_err("missing binary must still fail");
        assert!(err.contains("找不到二进制 gops"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 包内以**符号链接**冒充二进制名 → 不作候选（免得把源指到包外），落可读失败。
    #[cfg(unix)]
    #[test]
    fn locate_binary_skips_a_symlink_with_the_binary_name() {
        let dir = temp_dir("locate-symlink");
        std::os::unix::fs::symlink("/etc/passwd", dir.join("gops")).expect("symlink");
        std::fs::write(dir.join("other"), b"x").expect("other");
        let err = locate_binary(&dir, "gops").expect_err("symlink is not a candidate");
        assert!(err.contains("找不到二进制 gops"), "{err}");
        assert!(err.contains("gops"), "应在包内文件列表里露脸：{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 内容寻址名（`pkg-<sha256 前 16 位>`）→ 自动核前缀，对上了才装。
    #[tokio::test]
    async fn install_verifies_a_content_addressed_prefix() {
        let dir = temp_dir("sha-prefix");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let bytes = tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW");
        let digest = sha256_hex_bytes(&bytes);
        let artifact = dir.join(format!("pkg-{}", &digest[..16]));
        std::fs::write(&artifact, &bytes).expect("artifact");

        let detail = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect("install");
        assert!(detail.contains("sha256 前缀"), "{detail}");
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 内容寻址名里的前缀与字节不符 → **拒装**，旧二进制原封不动。
    #[tokio::test]
    async fn install_rejects_a_content_addressed_digest_mismatch() {
        let dir = temp_dir("sha-prefix-bad");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("pkg-deadbeefdeadbeef");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW"),
        )
        .expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .install("galaxy-ops", &artifact.to_string_lossy())
            .await
            .expect_err("digest mismatch must be rejected");
        assert!(err.contains("制品摘要不符"), "{err}");
        assert_eq!(
            std::fs::read(&target).expect("read"),
            b"OLD",
            "不符的字节绝不覆盖"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 显式期望摘要（中心契约）：对得上就装（说明里带完整摘要），对不上就拒。
    #[tokio::test]
    async fn install_verifies_an_explicit_expected_digest() {
        let dir = temp_dir("sha-explicit");
        let target = dir.join("bin").join("gx");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let bytes = tool_archive("galaxy-flow-0.15.1-x86_64-unknown-linux-gnu/gx", b"NEW");
        let artifact = dir.join("galaxy-flow.tar.gz");
        std::fs::write(&artifact, &bytes).expect("artifact");
        let digest = sha256_hex_bytes(&bytes);
        let tool = ToolInstaller::new(
            BTreeMap::from([(
                "galaxy-flow".to_string(),
                target.to_string_lossy().to_string(),
            )]),
            &dir,
            reqwest::Client::new(),
        )
        .with_host_target(HostTarget::new("x86_64", "linux", true));

        // 正确摘要（带 `sha256:` 前缀）→ 过。
        let detail = tool
            .install_with_digest(
                "galaxy-flow",
                &artifact.to_string_lossy(),
                Some(&format!("sha256:{digest}")),
            )
            .await
            .expect("install");
        assert!(
            detail.contains(&format!("sha256 {digest} 已核")),
            "{detail}"
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"NEW");

        // 错误摘要 → 拒，且旧版不动。
        write_exec(&target, b"OLD2");
        let err = tool
            .install_with_digest(
                "galaxy-flow",
                &artifact.to_string_lossy(),
                Some(&"0".repeat(64)),
            )
            .await
            .expect_err("mismatch");
        assert!(err.contains("制品摘要不符"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD2");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 显式摘要形态不对（非 64 hex）→ 早拒，不静默当「没给」。
    #[tokio::test]
    async fn install_rejects_a_malformed_explicit_digest() {
        let dir = temp_dir("sha-malformed");
        let target = dir.join("bin").join("gops");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("bin");
        write_exec(&target, b"OLD");
        let artifact = dir.join("galaxy-ops.tar.gz");
        std::fs::write(
            &artifact,
            tool_archive("galaxy-ops-0.1.0-aarch64-apple-darwin/gops", b"NEW"),
        )
        .expect("artifact");

        let err = installer(&dir, "galaxy-ops", &target)
            .install_with_digest(
                "galaxy-ops",
                &artifact.to_string_lossy(),
                Some("not-a-digest"),
            )
            .await
            .expect_err("malformed digest must be rejected");
        assert!(err.contains("期望摘要形态不对"), "{err}");
        assert_eq!(std::fs::read(&target).expect("read"), b"OLD");
        let _ = std::fs::remove_dir_all(dir);
    }
}
