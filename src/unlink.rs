//! 断开本机网关与 Center 的连接（`wist-gwlinkd unlink`）。
//!
//! 「与 Center 的连接」= 本机 gwlinkd 手里的**注册态**：客户端证书 + 链接配置 + 待消费注册券 +
//! 身份 / 实例，以及 `gwlinkd.toml` 里的一次性接入券 `link_token`（留着它下次启动会**绕过页面
//! 自动重连**）。本命令把这些清掉，让网关回到**未接入**。
//!
//! 这样 **link（`onboard`）与 unlink 同属 gwlinkd** —— 同一份状态、同一个 owner，由它集中管理；
//! 上层（如开发态 `dev/unlink-center.sh`）只需「停进程 + 调本命令」，不做文件手术。
//!
//! **不碰**中心信任锚（[`Config::trust_bundle`]）：它是中心的**公开证书**，不是连接；只有
//! `forget_center` 才一并清掉。因此即便锚落在 `state_dir` 内（页面接入会这么写），也**无需搬动** ——
//! 本命令只删**注册态那几个文件**，不整目录清空。
//!
//! 运行中的 gwlinkd 会不停续期 / 写回凭据（清掉也会被它重写）—— 而它是**另一个进程**，本命令无法
//! 安全地替它收尾，故直接**拒绝**：调用方先 `stop`，再 `unlink`。

use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::error::{StateReason, StateResult};
use crate::state;

/// unlink 删除的文件：**注册态**（客户端证书 / 链接配置 / 待消费注册券 / 身份 / 实例）+ 升级
/// 簿记（游标 / 记录 / 心跳，清成干净起点；运行中已拒绝，不会有在飞的升级）。
/// **不含**中心信任锚（默认保留，见 [`Config::trust_bundle`]）与 `gwlinkd.lock`。
///
/// 公开（而非私有）：bin 层集成测试（`tests/unlink_cli.rs`）据此断言产物，与实现共享**同一份清单**，
/// 免得新增/改名文件时两边漂移。
pub const LINK_STATE_FILES: &[&str] = &[
    state::CREDENTIAL_FILE,
    state::LINK_CONFIG_FILE,
    state::REGIST_TOKEN_FILE,
    state::IDENTITY_FILE,
    state::INSTANCE_FILE,
    state::UPGRADE_CURSOR_FILE,
    state::UPGRADE_RECORD_FILE,
    state::UPGRADE_HEARTBEAT_FILE,
];

/// `unlink` 的结果（供 CLI / 上层打印）。`dry_run` 时列出的是**将要**删除的东西，未落盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkReport {
    /// 状态目录。
    pub state_dir: PathBuf,
    /// 删除（或 dry-run 下将删除）的注册态文件（文件名）。
    pub removed_state_files: Vec<String>,
    /// 是否从 `gwlinkd.toml` 去掉了 `link_token`。
    pub link_token_removed: bool,
    /// 中心信任锚**配置路径**（`trust_bundle`；未删时供展示）。
    pub trust_bundle: PathBuf,
    /// 实际删除（或 dry-run 下将删除）的中心 CA 路径（仅 `forget_center`）：
    /// `trust_bundle` 指向的那份 + `state/control-center.pem` 那份副本（去重）。
    pub removed_anchors: Vec<PathBuf>,
    /// 是否是干跑（未落盘）。
    pub dry_run: bool,
}

/// 已知的**中心 CA**（信任锚，约定名 [`state::TRUST_BUNDLE_FILE`]）落点：
/// `trust_bundle` 指向的那份、`state/` 里页面接入写的那份副本、以及**配置目录**下那份（手工预置的常见落点，
/// 可能是换了 `trust_bundle` 后遗留的陈旧副本）。三处各可能存一份 ——「忘掉这个中心」得把它们**都**清干净，
/// 而不是只删配置当下指的那份。去重；不存在的候选在删除时按 NotFound 跳过。
fn anchor_candidates(config: &Config, config_path: &Path) -> Vec<PathBuf> {
    let mut out = vec![config.trust_bundle.clone()];
    let mut candidates = vec![config.state_dir.join(state::TRUST_BUNDLE_FILE)];
    if let Some(dir) = config_path.parent() {
        candidates.push(dir.join(state::TRUST_BUNDLE_FILE));
    }
    for candidate in candidates {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// 断开连接。
///
/// `config_path` 用来改写 `gwlinkd.toml`（去掉 `link_token`）。`forget_center` 为真时连中心信任锚
/// （[`Config::trust_bundle`]）一并删除，否则保留。`dry_run` 为真时只算不落盘（也就不需要为「正在运行」
/// 买单 —— 那是对**真删**的保护）。真删时运行中（拿得到单实例锁）→ 报错，要求先停。
pub fn unlink(
    config_path: &Path,
    config: &Config,
    forget_center: bool,
    dry_run: bool,
) -> StateResult<UnlinkReport> {
    if !dry_run
        && state::is_running(&config.state_dir)
            .map_err(|err| StateReason::Io.err(format!("检查 gwlinkd 是否在运行失败：{err}")))?
    {
        return Err(StateReason::AlreadyRunning
            .err("gwlinkd 正在运行（state/ 被它占用）；先停掉它，再 `wist-gwlinkd unlink`"));
    }

    // **先去掉一次性接入券 `link_token`**：它是「下次启动**绕过页面**自动重连」的闸门。放在删注册态
    // **之前**，这样即使配置写失败，也不会停在「证书已删、券还在」那种会**悄悄自动重连**的半截态
    // （顺序反了的话，部分失败恰好把机器留在最不该留的状态）。
    let link_token_removed = if dry_run {
        // 与 `remove_link_token` 同判据：配置文件里存在 `link_token` 键。
        config.link_token.is_some()
    } else {
        config::remove_link_token(config_path)
            .map_err(|err| StateReason::Io.err(err.display_chain()))?
    };

    let mut removed_state_files = Vec::new();
    for name in LINK_STATE_FILES {
        let path = config.state_dir.join(name);
        if dry_run {
            if path.exists() {
                removed_state_files.push((*name).to_string());
            }
            continue;
        }
        // 直接删、容忍 NotFound（幂等；也免受并发重复调用影响），等价于先 exists 再删。
        match std::fs::remove_file(&path) {
            Ok(()) => removed_state_files.push((*name).to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(StateReason::Io.err(format!("删除 {} 失败：{err}", path.display())));
            }
        }
    }

    // `--forget-center`：把**已知的每一份**中心 CA 都删掉（`trust_bundle` 指向的 + `state/` 里那份副本），
    // 否则「忘掉这个中心」会漏掉另一份副本。默认（保留）时一份不动。
    let mut removed_anchors = Vec::new();
    if forget_center {
        for path in anchor_candidates(config, config_path) {
            if dry_run {
                if path.exists() {
                    removed_anchors.push(path);
                }
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => removed_anchors.push(path),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(StateReason::Io
                        .err(format!("删除中心信任锚 {} 失败：{err}", path.display())));
                }
            }
        }
    }

    Ok(UnlinkReport {
        state_dir: config.state_dir.clone(),
        removed_state_files,
        link_token_removed,
        trust_bundle: config.trust_bundle.clone(),
        removed_anchors,
        dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-unlink-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// 写一份最小可加载的 gwlinkd.toml（CA 落在 `anchor` 指定处），返回配置路径。
    fn write_config(dir: &Path, anchor: &Path, link_token: Option<&str>) -> PathBuf {
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let mut text = format!(
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"{}\"\nstate_dir = \"{}\"\n",
            anchor.display(),
            state_dir.display()
        );
        if let Some(token) = link_token {
            text.push_str(&format!("link_token = \"{token}\"\n"));
        }
        let path = dir.join("gwlinkd.toml");
        std::fs::write(&path, text).expect("config");
        path
    }

    fn touch_registration_state(state_dir: &Path) {
        for name in LINK_STATE_FILES {
            std::fs::write(state_dir.join(name), b"x").expect("state file");
        }
    }

    #[test]
    fn unlink_clears_registration_state_and_link_token_but_keeps_the_anchor() {
        let dir = temp_dir("full");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);

        let report = unlink(&path, &config, false, false).expect("unlink");
        assert!(!report.dry_run);
        assert_eq!(report.removed_state_files.len(), LINK_STATE_FILES.len());
        for name in LINK_STATE_FILES {
            assert!(!config.state_dir.join(name).exists(), "{name} 应被删");
        }
        assert!(report.link_token_removed, "应去掉 link_token");
        assert!(Config::load(&path).expect("reload").link_token.is_none());
        assert!(report.removed_anchors.is_empty());
        assert!(anchor.exists(), "信任锚默认应保留");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_an_anchor_that_lives_inside_the_state_dir() {
        let dir = temp_dir("anchor-in-state");
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let anchor = state_dir.join(state::TRUST_BUNDLE_FILE);
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, None);
        let config = Config::load(&path).expect("load");
        touch_registration_state(&state_dir);

        unlink(&path, &config, false, false).expect("unlink");
        // 页面接入把 CA 写进 state_dir —— 默认只删注册态文件，不碰信任锚（也就不需要搬动 / 改指）。
        assert!(anchor.exists(), "state/ 内的信任锚默认应保留");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_center_removes_every_known_anchor_copy() {
        let dir = temp_dir("forget");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, None);
        let config = Config::load(&path).expect("load");
        // 页面接入会在 state/ 里也写一份中心 CA —— forget_center 必须把它也删掉，
        // 不能只删配置指的那份（否则「忘掉这个中心」会漏掉另一份副本）。
        let state_copy = config.state_dir.join(state::TRUST_BUNDLE_FILE);
        std::fs::write(&state_copy, "-----BEGIN CERTIFICATE-----\n").expect("state ca");

        let report = unlink(&path, &config, true, false).expect("unlink");
        assert_eq!(
            report.removed_anchors.len(),
            2,
            "两份中心 CA 都该删：{:?}",
            report.removed_anchors
        );
        assert!(!anchor.exists(), "trust_bundle 指向的那份应删");
        assert!(!state_copy.exists(), "state/ 里的副本也应删");
        assert!(!report.link_token_removed, "本就没有券");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_center_also_removes_a_stray_config_dir_copy() {
        // 换了 `trust_bundle` 之后，配置目录下那份旧 `control-center.pem` 就没人引用了 ——
        // 仍属「中心相关证书」，`--forget-center` 也该一并清掉。
        let dir = temp_dir("forget-stray");
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let state_ca = state_dir.join(state::TRUST_BUNDLE_FILE);
        std::fs::write(&state_ca, "-----BEGIN CERTIFICATE-----\n").expect("state ca");
        let stray = dir.join(state::TRUST_BUNDLE_FILE); // = 配置目录下的那份
        std::fs::write(&stray, "-----BEGIN CERTIFICATE-----\n").expect("stray ca");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"{}\"\nstate_dir = \"{}\"\n",
                state_ca.display(),
                state_dir.display()
            ),
        )
        .expect("config");
        let config = Config::load(&path).expect("load");

        let report = unlink(&path, &config, true, false).expect("unlink");
        assert!(!state_ca.exists(), "trust_bundle 指的那份应删");
        assert!(!stray.exists(), "配置目录下的陈旧副本也应删");
        assert_eq!(
            report.removed_anchors.len(),
            2,
            "{:?}",
            report.removed_anchors
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dry_run_reports_without_touching_anything() {
        let dir = temp_dir("dry-run");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);
        let before = std::fs::read_to_string(&path).expect("read");

        let report = unlink(&path, &config, false, true).expect("dry run");
        assert!(report.dry_run);
        assert_eq!(report.removed_state_files.len(), LINK_STATE_FILES.len());
        assert!(report.link_token_removed);
        // 零副作用。
        for name in LINK_STATE_FILES {
            assert!(config.state_dir.join(name).exists(), "{name} 不应被动");
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_while_the_daemon_holds_the_lock_and_touches_nothing() {
        let dir = temp_dir("running");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);
        let _guard = state::acquire_single_instance_lock(&config.state_dir).expect("lock");

        let err = unlink(&path, &config, false, false).expect_err("必须拒绝");
        assert!(err.contains("正在运行"), "{err}");
        for name in LINK_STATE_FILES {
            assert!(
                config.state_dir.join(name).exists(),
                "{name} 不应被动（拒绝时零副作用）"
            );
        }
        assert!(
            Config::load(&path).expect("reload").link_token.is_some(),
            "拒绝时不应改配置"
        );
        // 干跑不落盘，也就不需要为「正在运行」买单。
        assert!(
            unlink(&path, &config, false, true).is_ok(),
            "dry-run 应仍可预览"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unlink_is_idempotent() {
        let dir = temp_dir("idem");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);

        unlink(&path, &config, false, false).expect("first");
        let report = unlink(&path, &config, false, false).expect("second");
        assert!(report.removed_state_files.is_empty(), "第二次应无文件可删");
        assert!(!report.link_token_removed, "第二次应无券可去");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn does_not_delete_state_when_removing_the_link_token_fails() {
        // 锁住「先去 link_token、再删注册态」的顺序：去券失败必须发生在删注册态**之前**，
        // 否则会停在「证书已删、券还在」的半截态（下次启动会同页面绕过自动重连）。
        let dir = temp_dir("token-first");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);

        // 配置路径不存在 → 去 link_token 必失败。
        let unwritable = dir.join("missing-config.toml");
        let err = unlink(&unwritable, &config, false, false).expect_err("应因配置不可读而失败");
        assert!(err.contains("读取配置失败"), "{err}");
        for name in LINK_STATE_FILES {
            assert!(
                config.state_dir.join(name).exists(),
                "{name} 不应被动：去券失败必须先于删注册态"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_center_removes_an_anchor_inside_the_state_dir() {
        let dir = temp_dir("forget-in-state");
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        let anchor = state_dir.join(state::TRUST_BUNDLE_FILE);
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, None);
        let config = Config::load(&path).expect("load");
        touch_registration_state(&state_dir);

        let report = unlink(&path, &config, true, false).expect("unlink");
        assert_eq!(
            report.removed_anchors.len(),
            1,
            "锚就是 state/ 里那份，去重后只删一次"
        );
        assert!(!anchor.exists(), "forget_center 连 state/ 内的锚也要删");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leaves_unrelated_files_in_the_state_dir_alone() {
        // 只删注册态那几个文件 —— 不整目录清空（所以锚在 state/ 内也无需搬动）。
        let dir = temp_dir("unrelated");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        let path = write_config(&dir, &anchor, Some("link_abc"));
        let config = Config::load(&path).expect("load");
        touch_registration_state(&config.state_dir);
        let lock = config.state_dir.join("gwlinkd.lock");
        std::fs::write(&lock, b"").expect("lock");
        let other = config.state_dir.join("notes.txt");
        std::fs::write(&other, b"keep me").expect("other");

        unlink(&path, &config, false, false).expect("unlink");
        assert!(lock.exists(), "非注册态文件（锁）不应被删");
        assert!(other.exists(), "非注册态文件不应被删");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unlink_on_a_never_linked_home_succeeds() {
        let dir = temp_dir("never-linked");
        let anchor = dir.join("control-center.pem");
        std::fs::write(&anchor, "-----BEGIN CERTIFICATE-----\n").expect("ca");
        // 既没有 link_token，也没有任何注册态文件。
        let path = write_config(&dir, &anchor, None);
        let config = Config::load(&path).expect("load");

        let report = unlink(&path, &config, false, false).expect("unlink");
        assert!(report.removed_state_files.is_empty(), "本就没有注册态");
        assert!(!report.link_token_removed);
        assert!(report.removed_anchors.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
