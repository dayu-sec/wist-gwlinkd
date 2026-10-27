//! wparse 业务身份的现场值：`<work_dir>/.env` 里的 `WIST_TENANT_ID`。
//!
//! 设计 `wist-design/doc/design/edge/gateway-business-identity.md`：
//! - wparse 从**工作根**的 `.env` 读取 `${WIST_TENANT_ID}`（已实测，按 work-root 解析）；
//! - 该文件不是密钥；模式按**挂载策略**（读写 `0660` / 只读 `0640`，与 `gops sys perms`
//!   同口径）—— 写死 `0644` 会让 `gops run start` 判待修并拒启；只合并这一个键，保留现场其它键；
//! - 写入后需让 wparse 重新读取（开发态 `svc.sh restart`、发布态 `gops run stop/start`；
//!   都没配也能推出来，见 [`resolve_apply_action`]）。
//!
//! 安全边界：工作根是**容器可写**目录（wparse 以 uid 999 写 `.warp_parse` / `data`），
//! 而本进程按部署用户运行 —— 所以这里**不跟随符号链接**、临时文件用 `create_new`，
//! 否则被攻陷的容器可以让 gwlinkd 覆盖任意本用户可写的文件。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::sys_root::control_workdir;

/// wparse 工作根下的现场值文件。
pub const ENV_FILE: &str = ".env";
/// 业务身份键（与 wparse `topology/sinks/defaults.toml` 的 `${WIST_TENANT_ID}` 对应）。
pub const TENANT_KEY: &str = "WIST_TENANT_ID";

/// `tenant_id` 会成为 wparse tag 的 value：只允许 tag-safe 字符集（与 wparse `validate_tags` 同口径）。
///
/// 与 Center 的 `api::is_tag_safe` 是同一口径（D2）。这里是**写边界**的防御性校验：
/// 不能假设上游（Center / 初始配置 / 磁盘上的旧身份文件）一定校验过。
pub fn is_tag_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | '.' | ':' | '/' | '=' | '@' | '+' | ',' | '-')
        })
}

/// 在工作根里写/更新 `WIST_TENANT_ID`（原子写；保留其它键）。
///
/// 失败一律上抛：**静默失败**（例如 chmod 失败导致容器读不到）会表现为
/// 「Center 认为已生效、数据面仍是 `unbound`」，比直接报错难查得多。
pub fn write_tenant_id(work_dir: &Path, tenant_id: &str) -> Result<PathBuf, String> {
    // 写边界再校验一次（参数来自命令 / 初始配置 / 磁盘上的旧身份文件）。
    if !is_tag_safe(tenant_id) {
        return Err(format!(
            "tenant_id 不是 tag-safe：{}",
            value_preview(tenant_id)
        ));
    }
    let path = work_dir.join(ENV_FILE);
    ensure_regular_file(&path)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        // 只有「不存在」才当空文件；权限/IO 错误**不能**当成空串 —— 否则会把现场其它键
        // （含 wparse 需要的变量）整段覆盖掉。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(format!(
                "读 {} 失败（不会覆盖，请先修权限/编码）：{err}",
                path.display()
            ));
        }
    };
    std::fs::create_dir_all(work_dir)
        .map_err(|err| format!("创建 wparse 工作根失败 {}: {err}", work_dir.display()))?;
    let content = upsert_env_value(&existing, TENANT_KEY, tenant_id);
    // 随机名 + `create_new`：并发写不互相踩，且**拒绝**被预置的符号链接/同名文件。
    let tmp = work_dir.join(format!(
        ".env.wist-{}-{}",
        std::process::id(),
        monotonic_nanos()
    ));
    // 失败时**清掉临时文件**：补偿每 60s 会重试，不清理会在工作根里攒一堆半成品。
    let written = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|err| format!("创建临时文件失败 {}: {err}", tmp.display()))?;
        file.write_all(content.as_bytes())
            .map_err(|err| format!("写 .env 临时文件失败 {}: {err}", tmp.display()))?;
        // 落盘后再 rename：断电不会留下半截 `.env`（wparse 启动时读它）。
        file.sync_all()
            .map_err(|err| format!("刷新临时文件失败 {}: {err}", tmp.display()))?;
        drop(file);
        #[cfg(unix)]
        {
            // 模式跟**挂载策略**走，不写死：`.env` 落在 bind 挂载里，模式必须满足
            // `gops sys perms` 的契约（读写 0660 / 只读 0640），否则 `gops run start`
            // 的前置会判待修并拒启 —— 那会让「绑定客户」把整栈停在停止态。
            // 工作根是 setgid 目录，文件组仍是容器 gid，0660/0640 对 wparse(999) 均可读。
            let (_, file_mode) = crate::host_mode::mount_policy(work_dir)
                .map_err(|err| format!("判定 .env 挂载策略失败 {}: {err}", work_dir.display()))?;
            crate::host_mode::set_mode(&tmp, file_mode)
                .map_err(|err| format!("设置 .env 权限失败 {}: {err}", tmp.display()))?;
        }
        Ok(())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("落盘 .env 失败 {}: {err}", path.display()));
    }
    Ok(path)
}

/// 目标已存在但不是普通文件（符号链接 / 设备…）→ 拒绝。
///
/// `rename` 只替换链接本身、不会跟随，但**读**旧内容会跟随：不挡住的话，
/// 容器把一个指向 `credential.json` 的链接放到 `.env` 位置就能把内容读出来再写回可读位置。
fn ensure_regular_file(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => Err(format!(
            "{} 已存在但不是普通文件（可能是符号链接），拒绝读写；请人工确认后删除它",
            path.display()
        )),
        _ => Ok(()),
    }
}

/// 把**已存在** `.env` 的模式对齐到挂载策略（内容不动、不触发重启）。
///
/// 为什么需要：老版本把 `.env` 写死 `0644`，文件一旦落下就不会自己变好 —— 而
/// `gops run start` 的前置按模式判待修，会把**整个栈**挡在停止态。写入路径只在新绑定 /
/// 值漂移时才跑，所以启动自检这里顺手收一次（与 0.18.1 交付包「顺手对齐旧文件」同一思路）。
///
/// 返回是否真的改了模式（供调用方记日志）。
pub fn align_env_mode(work_dir: &Path) -> Result<bool, String> {
    let path = work_dir.join(ENV_FILE);
    // 不跟符号链接：工作根对容器可写，跟着改会变成「让 gwlinkd chmod 任意本用户可写的文件」。
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => meta,
        Ok(_) => {
            return Err(format!(
                "{} 不是普通文件（可能是符号链接），拒绝调整",
                path.display()
            ));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(format!("读 {} 失败：{err}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let (_, file_mode) = crate::host_mode::mount_policy(work_dir)
            .map_err(|err| format!("判定 .env 挂载策略失败 {}: {err}", work_dir.display()))?;
        if meta.permissions().mode() & 0o7777 == file_mode {
            return Ok(false);
        }
        crate::host_mode::set_mode(&path, file_mode)
            .map_err(|err| format!("对齐 .env 权限失败 {}: {err}", path.display()))?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(false)
    }
}

/// `.env` 里是否有 `KEY=value` 这一行（启动自检用：身份文件与现场值是否一致）。
pub fn env_has_value(content: &str, key: &str, value: &str) -> bool {
    let wanted = format!("{key}={value}");
    content
        .lines()
        .any(|line| normalize_env_line(line).as_deref() == Some(wanted.as_str()))
}

/// 合并式更新：`KEY=value` 已存在则替换，缺失则追加；其它行（注释/其它键）原样保留。
pub fn upsert_env_value(content: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut lines: Vec<String> = content
        .lines()
        .map(|line| {
            if env_line_key(line).as_deref() == Some(key) {
                found = true;
                format!("{key}={value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.push(format!("{key}={value}"));
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// 取一行 dotenv 的键（容忍 `export KEY=...`、`KEY = ...`、行首空白）；注释 / 非赋值行 → `None`。
fn env_line_key(line: &str) -> Option<String> {
    let rest = line.trim_start();
    if rest.starts_with('#') {
        return None;
    }
    let rest = rest
        .strip_prefix("export ")
        .or_else(|| rest.strip_prefix("export\t"))
        .unwrap_or(rest)
        .trim_start();
    let (key, _) = rest.split_once('=')?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// `KEY=value` 的规范形式（去掉 `export ` 与等号两侧空白）：自检按它比较。
fn normalize_env_line(line: &str) -> Option<String> {
    let rest = line.trim_start();
    if rest.starts_with('#') {
        return None;
    }
    let rest = rest
        .strip_prefix("export ")
        .or_else(|| rest.strip_prefix("export\t"))
        .unwrap_or(rest)
        .trim_start();
    let (key, value) = rest.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some(format!("{key}={}", value.trim()))
}

/// 解析 wparse 工作根：
/// 1. 显式 `wparse_work_dir`（必须已存在，不替人建目录）；
/// 2. 系统根下 `data-plane-run/dev`（开发态）；
/// 3. 系统根下 `data-plane-run/work`（发布态）。
///
/// **两个候选并存时报错**，不猜（写错工作根 = 回执「已生效」但数据面带错租户）。
///
/// 刻意**不读**栈根 `.env` 的 `WPARSE_RUN_WORK`：它描述的是**发布态**容器的工作根
/// （dev 栈的 `.env` 也写着 `./data-plane-run/work`），而开发态 wparse 实际用
/// `data-plane-run/dev` —— 拿它当来源会把 dev 写错。开发态请显式配 `wparse_work_dir`。
pub fn resolve_work_dir(config: &Config) -> Result<PathBuf, String> {
    if let Some(dir) = config.wparse_work_dir.as_ref() {
        if !dir.is_dir() {
            return Err(format!(
                "配置的 wparse_work_dir 不存在或不是目录：{}",
                dir.display()
            ));
        }
        return Ok(dir.clone());
    }
    let root = control_workdir(config)?;
    let dev = root.join("data-plane-run").join("dev");
    let work = root.join("data-plane-run").join("work");
    match (dev.is_dir(), work.is_dir()) {
        (true, false) => Ok(dev),
        (false, true) => Ok(work),
        (true, true) => Err(format!(
            "{} 下 data-plane-run/dev 与 data-plane-run/work 并存，无法确定 wparse 读哪个；请显式配置 wparse_work_dir",
            root.display()
        )),
        (false, false) => Err(format!(
            "未找到 wparse 工作根：{} 下 data-plane-run/dev|work 都不存在；请配置 wparse_work_dir",
            root.display()
        )),
    }
}

/// 显式配置的**非空**生效命令。空白值与未配置同义（返回 `None`）。
///
/// 「**是否**要生效」不在这里：那是指令规则（`bind-tenant` 的语义就是让租户生效）。
/// 本函数只回答「配置里有没有指定**怎么做**」—— 没有就用 [`resolve_apply_action`] 的缺省。
pub fn apply_command(config: &Config) -> Option<&str> {
    config
        .wparse_apply_command
        .as_deref()
        .map(str::trim)
        .filter(|command| !command.is_empty())
}

/// 生效动作：`.env` 写完后**在哪跑什么**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyAction {
    pub command: String,
    /// 命令的 cwd：gops 从 cwd 解析系统，所以必须是**系统根**。
    pub dir: PathBuf,
    /// `true` = 配置里没写，按工作根推断出的缺省值（日志 / 回执要能分开）。
    pub defaulted: bool,
}

/// 开发态缺省：wparse 是**宿主进程**，`gops run start` 起的是容器 —— 会在旁边多起一套；
/// 开发栈的 `dev/svc.sh` 才是「只重启 wparse」的正确入口。
pub const DEFAULT_DEV_APPLY_COMMAND: &str = "./dev/svc.sh restart wparse --no-build";

/// 发布态缺省生效命令（与栈模板同口径：整栈 `stop` + `start`）。
///
/// 为什么不「只重启 wparse」：gops `run` 只有系统级动词，按服务重启要靠 compose
/// 项目名 / 服务名这些**部署细节**；猜错时要么静默不动、要么动错栈。
pub fn default_apply_command(program: &str) -> String {
    format!("{program} run stop && {program} run start")
}

/// 解析生效动作：**是否生效由指令规则决定**（`bind-tenant` 一定会走到这里），
/// 本函数只回答「**怎么做**」：显式配置优先，没配就按工作根的形态推断缺省。
///
/// `Err(reason)` = 这次跑不了（**软失败**：`.env` 照写，回执 `wparse_applied:false` + 原因）。
/// 现场教训（2026-10-26）：让「生效」只依赖一条手写配置，等于把绑定客户的成败挂在
/// 一次 `localize` + 一次重启上；缺省必须能从现场自己推得出来。
///
/// **空白值不是「关闭生效」**：留空 = 未配置 = 用缺省。（0.17 曾把「留空」当「只写 `.env`」，
/// 那等于让配置决定“做不做”；某个现场真不想重启，就不应该下发绑定指令，而不是靠配置关掉。）
///
/// 推断顺序：
/// 1. 工作根是 `<系统根>/data-plane-run/dev` 且栈根带 `dev/svc.sh` → [`DEFAULT_DEV_APPLY_COMMAND`]；
/// 2. 否则 `<系统根>/data-plane-run/{dev,work}` 能反推出系统根（或配置的工程根可用）
///    → [`default_apply_command`]（发布态整栈重启）。
pub fn resolve_apply_action(config: &Config, work_dir: &Path) -> Result<ApplyAction, String> {
    if let Some(command) = apply_command(config) {
        let dir =
            control_workdir(config).map_err(|error| format!("生效命令缺少系统根：{error}"))?;
        return Ok(ApplyAction {
            command: command.to_string(),
            dir,
            defaulted: false,
        });
    }
    // 系统根：先从工作根反推，再退回配置的工程根（工作根可能是显式配的任意目录）。
    let system_root = stack_root_of(work_dir)
        .filter(|root| crate::sys_root::is_system_root(root))
        .map(Path::to_path_buf)
        .or_else(|| control_workdir(config).ok());
    let is_dev_work_dir = work_dir.file_name().is_some_and(|name| name == "dev");
    if is_dev_work_dir {
        if let Some(root) = system_root.as_deref()
            && root.join("dev/svc.sh").is_file()
        {
            return Ok(ApplyAction {
                command: DEFAULT_DEV_APPLY_COMMAND.to_string(),
                dir: root.to_path_buf(),
                defaulted: true,
            });
        }
        // 开发态没有 `svc.sh`：**不能**拿发布态的整栈重启充数 —— `gops run start` 会在
        // 旁边另起一套容器（与宿主进程抢同一份工作根）。
        return Err(format!(
            "工作根 {} 是开发态（data-plane-run/dev），但没有可用的 dev/svc.sh（系统根未定位或栈根下缺这个脚本），推不出该怎么重启 wparse；请显式配置 wparse_apply_command",
            work_dir.display()
        ));
    }
    let Some(root) = system_root else {
        return Err(format!(
            "未配置 wparse_apply_command，且工作根 {} 不在 `<系统根>/data-plane-run/{{dev,work}}` 布局下，推不出生效命令的系统根",
            work_dir.display()
        ));
    };
    Ok(ApplyAction {
        command: default_apply_command(&upgrader_program(config)),
        dir: root,
        defaulted: true,
    })
}

/// `<系统根>/data-plane-run/{dev,work}` → `<系统根>`；其它布局 → `None`（不猜）。
fn stack_root_of(work_dir: &Path) -> Option<&Path> {
    let data_plane_run = work_dir.parent()?;
    if data_plane_run.file_name()? != "data-plane-run" {
        return None;
    }
    data_plane_run.parent()
}

/// 缺省程序名与升级执行器同一口径（`upgrader_program`，缺省 `gops`）。
fn upgrader_program(config: &Config) -> String {
    config
        .upgrader_program
        .as_deref()
        .map(str::trim)
        .filter(|program| !program.is_empty())
        .unwrap_or(crate::executor::DEFAULT_UPGRADER_PROGRAM)
        .to_string()
}

/// 生效命令超时（缺省 120s，夹 `[1, 300]`）。
pub fn apply_timeout(config: &Config) -> Duration {
    Duration::from_secs(
        config
            .wparse_apply_timeout_seconds
            .unwrap_or(120)
            .clamp(1, 300),
    )
}

/// 回显不可信输入时的安全预览：只保留可打印 ASCII，限长。
///
/// 校验失败的值仍要能被排查，但**不能**把换行等控制字符原样写进日志 / 命令回执
/// （否则可以在 journald / 中心页面上伪造一行日志）。
pub fn value_preview(value: &str) -> String {
    let shown: String = value
        .chars()
        .take(24)
        .map(|ch| {
            if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                '.'
            }
        })
        .collect();
    let truncated = if value.chars().count() > 24 {
        "…"
    } else {
        ""
    };
    format!("`{shown}{truncated}`（{} 字节）", value.len())
}

fn monotonic_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_existing_key_and_preserves_others() {
        let content = "# comment\nWIST_TENANT_ID=old\nOTHER=keep\n";
        let updated = upsert_env_value(content, TENANT_KEY, "tenant-new");
        assert_eq!(
            updated,
            "# comment\nWIST_TENANT_ID=tenant-new\nOTHER=keep\n"
        );
    }

    #[test]
    fn upsert_appends_missing_key() {
        let updated = upsert_env_value("OTHER=keep\n", TENANT_KEY, "tenant-new");
        assert_eq!(updated, "OTHER=keep\nWIST_TENANT_ID=tenant-new\n");
    }

    #[test]
    fn upsert_handles_empty_and_leading_spaces() {
        assert_eq!(
            upsert_env_value("", TENANT_KEY, "tenant-a"),
            "WIST_TENANT_ID=tenant-a\n"
        );
        assert_eq!(
            upsert_env_value("  WIST_TENANT_ID=old\n", TENANT_KEY, "tenant-a"),
            "WIST_TENANT_ID=tenant-a\n"
        );
    }

    /// `export KEY=` / `KEY =` 也要认出来，否则会追加出**重复键**（dotenv 取第一条 → 换绑静默不生效）。
    #[test]
    fn upsert_recognises_export_and_spaced_assignments() {
        assert_eq!(
            upsert_env_value("export WIST_TENANT_ID=old\n", TENANT_KEY, "new"),
            "WIST_TENANT_ID=new\n"
        );
        assert_eq!(
            upsert_env_value("WIST_TENANT_ID = old\n", TENANT_KEY, "new"),
            "WIST_TENANT_ID=new\n"
        );
        // 注释与前缀相近的键不受影响。
        let kept = upsert_env_value("#WIST_TENANT_ID=x\nWIST_TENANT_ID_2=y\n", TENANT_KEY, "v");
        assert_eq!(
            kept,
            "#WIST_TENANT_ID=x\nWIST_TENANT_ID_2=y\nWIST_TENANT_ID=v\n"
        );
    }

    #[test]
    fn env_has_value_matches_the_exact_line() {
        let content = "OTHER=keep\nexport WIST_TENANT_ID=tenant-a\n";
        assert!(env_has_value(content, TENANT_KEY, "tenant-a"));
        assert!(!env_has_value(content, TENANT_KEY, "tenant-b"));
        assert!(!env_has_value("", TENANT_KEY, "tenant-a"));
    }

    fn temp_work_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gwlinkd-wparse-env-{}-{name}-{}",
            std::process::id(),
            monotonic_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_tenant_id_is_atomic_and_keeps_other_keys() {
        let dir = temp_work_dir("atomic");
        std::fs::write(dir.join(ENV_FILE), "OTHER=keep\n").unwrap();
        let path = write_tenant_id(&dir, "tenant-a").expect("write");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("OTHER=keep"));
        assert!(text.contains("WIST_TENANT_ID=tenant-a"));
        // 临时文件不残留。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".env.wist-")
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 写边界自己校验，不依赖调用方。
    #[test]
    fn write_tenant_id_rejects_unsafe_values() {
        let dir = temp_work_dir("unsafe");
        assert!(write_tenant_id(&dir, "").is_err());
        assert!(write_tenant_id(&dir, "tenant acme").is_err());
        assert!(write_tenant_id(&dir, "a\nWP_SINK=evil").is_err());
        assert!(!dir.join(ENV_FILE).exists(), "被拒的值不该落盘");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `.env` 的模式必须跟挂载策略一致：写死 `0644` 会让 `gops run start` 的
    /// perms preflight 判待修并拒启（曾经的整栈停机就是这个）。
    #[cfg(unix)]
    #[test]
    fn write_tenant_id_aligns_env_mode_with_mount_policy() {
        use std::os::unix::fs::PermissionsExt;
        fn mode_of(path: &Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
        }

        // 读写挂载（setgid + 组可写）→ 0660
        let rw = temp_work_dir("mode-rw");
        std::fs::set_permissions(&rw, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let path = write_tenant_id(&rw, "tenant-a").expect("write");
        assert_eq!(mode_of(&path), 0o660, "读写挂载的 .env 必须是 0660");
        let _ = std::fs::remove_dir_all(&rw);

        // 只读挂载 → 0640
        let ro = temp_work_dir("mode-ro");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o2750)).unwrap();
        let path = write_tenant_id(&ro, "tenant-a").expect("write");
        assert_eq!(mode_of(&path), 0o640, "只读挂载的 .env 必须是 0640");
        let _ = std::fs::remove_dir_all(&ro);
    }

    /// 老版本落下的 `0644` 要能在启动自检里被顺手收回（内容不动、幂等）。
    #[cfg(unix)]
    #[test]
    fn align_env_mode_fixes_a_legacy_0644_env() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_work_dir("align");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let path = dir.join(ENV_FILE);
        std::fs::write(&path, "WIST_TENANT_ID=tenant-a\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            align_env_mode(&dir).expect("align"),
            "0644 → 0660 应报告改动"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o660
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "WIST_TENANT_ID=tenant-a\n",
            "只改模式，不能动内容"
        );
        assert!(!align_env_mode(&dir).expect("align again"), "已对齐则幂等");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 符号链接一律拒绝调整（不能跟着改到目标文件）。
    #[cfg(unix)]
    #[test]
    fn align_env_mode_refuses_a_symlinked_env() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = temp_work_dir("align-symlink");
        let target = dir.join("credential.json");
        std::fs::write(&target, "SECRET=1\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, dir.join(ENV_FILE)).unwrap();
        assert!(align_env_mode(&dir).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o600,
            "不能改到链接目标"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_tenant_id_refuses_a_symlinked_env() {
        use std::os::unix::fs::symlink;
        let dir = temp_work_dir("symlink");
        let target = dir.join("credential.json");
        std::fs::write(&target, "SECRET=1\n").unwrap();
        symlink(&target, dir.join(ENV_FILE)).unwrap();
        let err = write_tenant_id(&dir, "tenant-a").expect_err("must refuse");
        assert!(err.contains("符号链接"), "{err}");
        // 目标文件没被改写。
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "SECRET=1\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 读失败（非 NotFound）不能被当成空文件 —— 否则会把 `.env` 其它键整段抹掉。
    #[cfg(unix)]
    #[test]
    fn write_tenant_id_does_not_wipe_env_on_read_error() {
        use std::os::unix::fs::PermissionsExt;
        // root 无视权限位，这个用例只在普通用户下有意义。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = temp_work_dir("unreadable");
        let path = dir.join(ENV_FILE);
        std::fs::write(&path, "OTHER=keep\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = write_tenant_id(&dir, "tenant-a").expect_err("读失败要报错");
        assert!(err.contains("读"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "OTHER=keep\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn value_preview_neutralises_control_characters() {
        let preview = value_preview("bad\nvalue");
        assert!(!preview.contains('\n'), "{preview}");
        assert!(preview.contains("bad.value"), "{preview}");
    }

    fn config_in(dir: &Path, body: &str) -> Config {
        let path = dir.join("gwlinkd.toml");
        std::fs::write(&path, format!("state_dir = \"{}\"\n{body}", dir.display())).unwrap();
        Config::load(&path).expect("config")
    }

    /// 发布态（`<系统根>/data-plane-run/work`）：不配也能推出整栈重启。
    #[test]
    fn resolve_apply_action_defaults_to_the_stack_restart() {
        let root = temp_work_dir("root-release");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = root.join("data-plane-run/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "");

        let action = resolve_apply_action(&config, &work).expect("缺省应推得出来");
        assert_eq!(action.command, "gops run stop && gops run start");
        assert_eq!(action.dir, root);
        assert!(action.defaulted, "缺省要能看出来不是配置里写的");
        let _ = std::fs::remove_dir_all(root);
    }

    /// 缺省程序名跟着 `upgrader_program` 走（换过 gops 位置的现场不能又回去调 PATH 里的旧 gops）。
    #[test]
    fn resolve_apply_action_follows_the_configured_upgrader() {
        let root = temp_work_dir("root-upgrader");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = root.join("data-plane-run/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "upgrader_program = \"/opt/ops/gops-2\"\n");

        let action = resolve_apply_action(&config, &work).expect("缺省");
        assert_eq!(
            action.command,
            "/opt/ops/gops-2 run stop && /opt/ops/gops-2 run start"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 开发态：wparse 是宿主进程，缺省要用 `dev/svc.sh`（用 gops 会多起一套容器）。
    #[test]
    fn resolve_apply_action_defaults_to_svc_sh_in_dev() {
        let root = temp_work_dir("root-dev");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("dev")).unwrap();
        std::fs::write(root.join("dev/svc.sh"), "#!/bin/sh\n").unwrap();
        let work = root.join("data-plane-run/dev");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "");

        let action = resolve_apply_action(&config, &work).expect("缺省");
        assert_eq!(action.command, DEFAULT_DEV_APPLY_COMMAND);
        assert_eq!(action.dir, root);
        let _ = std::fs::remove_dir_all(root);
    }

    /// 显式配置优先，且要能看出「这不是缺省」。
    #[test]
    fn resolve_apply_action_prefers_the_configured_command() {
        let root = temp_work_dir("root-configured");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = root.join("data-plane-run/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(
            &root,
            &format!(
                "upgrade_project_dir = \"{}\"\nwparse_apply_command = \"true\"\n",
                root.display()
            ),
        );

        let action = resolve_apply_action(&config, &work).expect("配置的命令");
        assert_eq!(action.command, "true");
        assert_eq!(action.dir, root);
        assert!(!action.defaulted);
        let _ = std::fs::remove_dir_all(root);
    }

    /// 开发态但没有 `svc.sh`：**拒绝**退回发布态整栈重启（那会在旁边另起一套容器）。
    #[test]
    fn resolve_apply_action_refuses_the_release_default_for_dev_without_svc_sh() {
        let root = temp_work_dir("root-dev-no-svc");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = root.join("data-plane-run/dev");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "");

        let reason = resolve_apply_action(&config, &work).expect_err("不能猜");
        assert!(reason.contains("dev/svc.sh"), "{reason}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// **空白值不是「关闭生效」**：「是否生效」是指令规则（`bind-tenant` 一定生效），
    /// `wparse_apply_command` 只决定「怎么做」—— 留空 = 未配置 = 用缺省。
    #[test]
    fn resolve_apply_action_treats_a_blank_command_as_unset() {
        let root = temp_work_dir("root-blank");
        std::fs::create_dir_all(root.join("sys")).unwrap();
        std::fs::write(
            root.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = root.join("data-plane-run/work");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "wparse_apply_command = \"\"\n");

        let action = resolve_apply_action(&config, &work).expect("留空 = 缺省，不是关闭");
        assert_eq!(action.command, "gops run stop && gops run start");
        assert!(action.defaulted);
        let _ = std::fs::remove_dir_all(root);
    }

    /// 空白值与未配置同义（都是 `None`）—— 这个用例钉住“配置不能决定做不做”。
    #[test]
    fn apply_command_does_not_distinguish_absent_from_an_explicit_blank() {
        let root = temp_work_dir("apply-command");
        assert_eq!(apply_command(&config_in(&root, "")), None);
        assert_eq!(
            apply_command(&config_in(&root, "wparse_apply_command = \"  \"\n")),
            None
        );
        assert_eq!(
            apply_command(&config_in(&root, "wparse_apply_command = \" true \"\n")),
            Some("true")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 推不出来就如实说（软失败），不能默默不动。
    #[test]
    fn resolve_apply_action_refuses_to_guess_an_unknown_layout() {
        let root = temp_work_dir("root-unknown");
        let work = root.join("work-elsewhere");
        std::fs::create_dir_all(&work).unwrap();
        let config = config_in(&root, "");

        let reason = resolve_apply_action(&config, &work).expect_err("推不出来就要报");
        assert!(reason.contains("wparse_apply_command"), "{reason}");
        let _ = std::fs::remove_dir_all(root);
    }
}
