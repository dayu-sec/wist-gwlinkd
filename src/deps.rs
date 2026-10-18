//! 宿主侧依赖工具（gops / gx）版本探测。
//!
//! 网关栈由 `gops` 部署、升级也经 `gops` 执行；`gx`（Galaxy Flow CLI）是 `gops` 的运行底座。
//! 按交付约定，二者由中心安装脚本装到宿主 **`$HOME/bin`**（脚本里的 `INSTALL_DIR`）——
//! 容器里的网关看不见它们，只有宿主侧常驻 `wist-gwlinkd` 能探测到。
//!
//! 探测结果随心跳推给网关（「网关状态」页展示），并随 `ReportGatewayStatus` 转报中心
//! （中心据此知道每台网关依赖的工具版本）。量不出（未安装 / 执行失败）→ `None`，不假装有值。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 单次 `--version` 探测的超时：工具卡死不能拖住主循环。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 宿主侧依赖工具版本；`None` = 未安装 / 探测失败。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolVersions {
    pub gops: Option<String>,
    pub gx: Option<String>,
}

impl ToolVersions {
    /// 探测 gops / gx 版本。
    ///
    /// `gops_program` 是配置里的升级执行器（通常是 `$HOME/bin/gops` 绝对路径，也可能只是 `gops`），
    /// 优先用它定位 gops 本体；其余按 `$HOME/bin` → PATH 回落。
    ///
    /// 两个探测**并发**：每个都有自己的超时，串行时最坏情况会把一拍心跳拖到 10s；
    /// 并发后仍是单次的 5s 上限。
    pub async fn detect(gops_program: Option<&str>) -> Self {
        let (gops, gx) = tokio::join!(
            probe_first(gops_candidates(gops_program)),
            probe_first(gx_candidates(gops_program)),
        );
        Self { gops, gx }
    }
}

/// `$HOME/bin/<name>`（`HOME` 缺失时跳过）。
fn home_bin(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("bin").join(name))
}

fn gops_candidates(configured: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Candidates::default();
    if let Some(program) = configured
        .map(str::trim)
        .filter(|program| !program.is_empty())
    {
        candidates.push(PathBuf::from(program));
    }
    if let Some(path) = home_bin("gops") {
        candidates.push(path);
    }
    // 裸名交给 `Command` 走 PATH 解析。
    candidates.push(PathBuf::from("gops"));
    candidates.into_vec()
}

fn gx_candidates(gops_program: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Candidates::default();
    // gx 与 gops 通常并排安装：配置里的执行器给的是 `$HOME/bin/gops`，优先取它的同目录。
    // 系统服务的 `$HOME` 未必是安装用户的家目录，不能只靠下面那条。
    if let Some(program) = gops_program
        .map(str::trim)
        .filter(|program| !program.is_empty())
        && let Some(parent) = Path::new(program)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
    {
        candidates.push(parent.join("gx"));
    }
    if let Some(path) = home_bin("gx") {
        candidates.push(path);
    }
    candidates.push(PathBuf::from("gx"));
    candidates.into_vec()
}

/// 去重、保序的候选路径。
#[derive(Default)]
struct Candidates {
    seen: BTreeSet<PathBuf>,
    ordered: Vec<PathBuf>,
}

impl Candidates {
    fn push(&mut self, path: PathBuf) {
        if self.seen.insert(path.clone()) {
            self.ordered.push(path);
        }
    }

    fn into_vec(self) -> Vec<PathBuf> {
        self.ordered
    }
}

async fn probe_first(candidates: Vec<PathBuf>) -> Option<String> {
    for program in candidates {
        if let Some(version) = probe(&program).await {
            return Some(version);
        }
    }
    None
}

/// 执行 `<program> --version` 并解析版本；失败 / 超时 → `None`。
async fn probe(program: &Path) -> Option<String> {
    probe_with_timeout(program, PROBE_TIMEOUT).await
}

/// 同 [`probe`]，超时可注入（测试用）。
async fn probe_with_timeout(program: &Path, timeout: Duration) -> Option<String> {
    let output = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(program)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    // 有的工具把版本写 stderr（或两路都有）：**两路都收**，不能只看其中一路。
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}\n{stderr}");
    parse_version(&text)
}

/// 从 `gops 2.4.0` / `gx 0.17.0` / `2.4.0` 这类输出里取版本号。
///
/// 只认「带点号的数字串」为宜：工具可能先打印一行说明，不能把整行当版本塞进中心。
fn parse_version(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let token = token
            .trim_matches(|ch: char| ch == ',' || ch == ';' || ch == '(' || ch == ')')
            .trim_end_matches('.');
        let digit_start = token.len() - token.trim_start_matches(['v', 'V']).len();
        let body = &token[digit_start..];
        let mut parts = body.split('.');
        let major = parts.next().unwrap_or_default();
        let minor = parts.next().unwrap_or_default();
        if !major.is_empty()
            && major.chars().all(|ch| ch.is_ascii_digit())
            && !minor.is_empty()
            && minor.chars().all(|ch| ch.is_ascii_digit())
        {
            return Some(body.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 CLI 输出：`gops 2.4.0` / `gx 0.17.0`。
    #[test]
    fn parses_the_real_cli_banners() {
        assert_eq!(parse_version("gops 2.4.0\n").as_deref(), Some("2.4.0"));
        assert_eq!(parse_version("gx 0.17.0\n").as_deref(), Some("0.17.0"));
    }

    /// 带 `v` 前缀 / 前后说明文字 / 多余空白也能取到版本。
    #[test]
    fn parses_version_with_prefix_and_noise() {
        assert_eq!(
            parse_version("galaxy-ops version v2.4.0 (build abc)\n").as_deref(),
            Some("2.4.0")
        );
        assert_eq!(parse_version("  0.17.0  \n").as_deref(), Some("0.17.0"));
    }

    /// 没有版本形状的输出 → `None`（不把说明文字当版本）。
    #[test]
    fn rejects_output_without_a_version() {
        assert_eq!(parse_version("usage: gops [options]\n"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("gops 2\n"), None);
        // 日期 / 单词不是版本：段里必须都是数字。
        assert_eq!(parse_version("copyright 2026. All rights reserved"), None);
        assert_eq!(parse_version("version unknown"), None);
    }

    /// 尾随标点 / 四段版本 / 两段版本。
    #[test]
    fn parses_common_version_shapes() {
        assert_eq!(parse_version("gops 2.4.0.").as_deref(), Some("2.4.0"));
        assert_eq!(parse_version("gx 0.17").as_deref(), Some("0.17"));
        assert_eq!(parse_version("gops 1.2.3.4").as_deref(), Some("1.2.3.4"));
    }

    #[cfg(unix)]
    fn script(body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gwlinkd-deps-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("tool.sh");
        std::fs::write(&path, body).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// 探测成功：读 stdout；版本在 stderr / 两路都有时也要读到。
    #[tokio::test]
    #[cfg(unix)]
    async fn probes_the_version_from_stdout_and_stderr() {
        for body in [
            "#!/bin/sh\necho 'gops 2.4.0'\n",
            "#!/bin/sh\necho 'gops 2.4.0' >&2\n",
            // stdout 有说明、版本只在 stderr：只看 stdout 会漏（回归）。
            "#!/bin/sh\necho 'galaxy-ops'\necho 'gops 2.4.0' >&2\n",
        ] {
            let path = script(body);
            assert_eq!(probe(&path).await.as_deref(), Some("2.4.0"), "{body:?}");
            let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
        }
    }

    /// 非零退出 / 不存在 / 非可执行 → `None`（不假装有版本）。
    #[tokio::test]
    #[cfg(unix)]
    async fn probe_failures_are_none() {
        let failing = script("#!/bin/sh\necho 'gops 2.4.0'\nexit 1\n");
        assert_eq!(probe(&failing).await, None);
        let _ = std::fs::remove_dir_all(failing.parent().expect("parent"));

        assert_eq!(probe(Path::new("/definitely/not/here/tool")).await, None);
    }

    /// 工具卡死不能拖住主循环：超时即 `None`，子进程被回收。
    #[tokio::test]
    #[cfg(unix)]
    async fn probe_times_out() {
        let hanging = script("#!/bin/sh\nsleep 5\n");
        let started = std::time::Instant::now();
        assert_eq!(
            probe_with_timeout(&hanging, Duration::from_millis(100)).await,
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "应在超时后立刻返回，实际 {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(hanging.parent().expect("parent"));
    }

    /// `detect` 用配置里的执行器路径定位 gops（优先于 PATH / HOME）。
    #[tokio::test]
    #[cfg(unix)]
    async fn detect_uses_the_configured_program() {
        let path = script("#!/bin/sh\necho 'gops 9.8.7'\n");
        let versions = ToolVersions::detect(Some(path.to_string_lossy().as_ref())).await;
        assert_eq!(versions.gops.as_deref(), Some("9.8.7"));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    /// 候选顺序：配置的执行器同目录的 gx → `$HOME/bin/gx` → PATH 裸名。
    #[test]
    fn gx_candidates_prefer_the_configured_program_directory() {
        let candidates = gx_candidates(Some("/opt/wist/bin/gops"));
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(Path::new("/opt/wist/bin/gx"))
        );
        assert_eq!(candidates.last(), Some(&PathBuf::from("gx")));
    }

    /// 只给了裸名 `gops`：没有可用的父目录，不应把空父目录拼成候选。
    #[test]
    fn gx_candidates_ignore_a_bare_program_name() {
        let candidates = gx_candidates(Some("gops"));
        assert_eq!(candidates.last(), Some(&PathBuf::from("gx")));
        assert!(
            candidates
                .iter()
                .all(|path| path.as_os_str() != std::ffi::OsStr::new("")),
            "不应出现空路径候选: {candidates:?}"
        );
    }

    /// 候选顺序：配置的执行器 → `$HOME/bin/gops` → PATH 裸名。
    #[test]
    fn gops_candidates_prefer_the_configured_program() {
        let candidates = gops_candidates(Some("/opt/wist/bin/gops"));
        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(Path::new("/opt/wist/bin/gops"))
        );
        assert!(candidates.iter().any(|path| path.ends_with("bin/gops")));
        assert_eq!(candidates.last(), Some(&PathBuf::from("gops")));
    }
}
