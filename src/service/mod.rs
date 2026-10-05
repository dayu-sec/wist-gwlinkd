//! 常驻服务集成：把 `wist-gwlinkd` 交给 OS 服务管理器**长期托管**（开机自启 / 崩溃拉起 / 退出重启）。
//!
//! - Linux：systemd unit（`Restart=always`，日志进 journald）。
//! - macOS：launchd plist（`KeepAlive`，日志进 `/var/log/wist-gwlinkd` 或 `~/Library/Logs/wist-gwlinkd`）。
//!
//! gwlinkd 自身**只前台运行**（不 fork / 不 daemonize）；自启、拉起、重启全部交给服务管理器，
//! 重复实例由 state 目录下的 flock（[`crate::state::acquire_single_instance_lock`]）兜底 ——
//! 服务管理器重启期间若前任未退出，新进程会以「已有网关常驻在跑」快速失败，再由 `Restart` /
//! `KeepAlive` 重试。
//!
//! 与 `wist-agentd/src/service` **有意同形**（便于两处对照）。差异：gwlinkd 无 sibling 执行器，
//! 且配置是**单个绝对文件路径**（[`CONFIG_ENV`] = `gwlinkd.toml` 的绝对路径），而不是 `--config-dir`。
//!
//! 本模块只做三件事：渲染服务定义、落盘/删除定义、给出服务管理器操作命令。真正的
//! `systemctl` / `launchctl` 调用在 CLI 层执行，便于单测只覆盖可确定的部分。

use std::fs;
use std::path::{Path, PathBuf};

pub mod launchd;
pub mod systemd;

/// 服务实例名（systemd unit 名 / 进程显示名）。
pub const SERVICE_NAME: &str = "wist-gwlinkd";
/// macOS launchd label（反向域名，全机唯一）。
pub const LAUNCHD_LABEL: &str = "com.dayu-sec.wist-gwlinkd";
/// user 作用域默认配置目录的目录名（挂在 `$HOME` 下）。
pub const USER_CONFIG_DIR_NAME: &str = ".wist-gwlinkd";
/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "gwlinkd.toml";
/// 指向配置文件**绝对路径**的环境变量（`run` 读取；见 `main::config_path`）。
pub const CONFIG_ENV: &str = "WIST_GWLINKD_CONFIG";
/// system 作用域默认配置目录。
pub const SYSTEM_CONFIG_DIR: &str = "/etc/wist-gwlinkd";
/// system 作用域日志目录（launchd 落盘用；systemd 走 journald）。
pub const SYSTEM_LOG_DIR: &str = "/var/log/wist-gwlinkd";

/// 安装作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceScope {
    /// 系统级：开机即起（systemd system / LaunchDaemon）。
    System,
    /// 用户级：登录后启动，以当前用户身份运行。
    User,
}

impl ServiceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceScope::System => "system",
            ServiceScope::User => "user",
        }
    }
}

/// 服务管理器形态（由编译目标决定；渲染函数显式接收，便于测试两个平台）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePlatform {
    Systemd,
    Launchd,
}

impl ServicePlatform {
    /// 当前目标平台使用的服务管理器；非 Linux / macOS 返回 `None`。
    pub fn current() -> Option<ServicePlatform> {
        if cfg!(target_os = "linux") {
            Some(ServicePlatform::Systemd)
        } else if cfg!(target_os = "macos") {
            Some(ServicePlatform::Launchd)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ServicePlatform::Systemd => "systemd",
            ServicePlatform::Launchd => "launchd",
        }
    }
}

/// 服务定义与日志的落盘位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLayout {
    pub platform: ServicePlatform,
    pub scope: ServiceScope,
    /// service 定义文件（`*.service` / `*.plist`）。
    pub definition_path: PathBuf,
    /// launchd 标准输出/错误目录；systemd 走 journald，故为 `None`。
    pub log_dir: Option<PathBuf>,
}

impl ServiceLayout {
    /// 按平台/作用域解析标准安装位置。
    pub fn resolve(platform: ServicePlatform, scope: ServiceScope) -> Result<Self, String> {
        let (definition_path, log_dir) = match platform {
            ServicePlatform::Systemd => (systemd::unit_path(scope)?, None),
            ServicePlatform::Launchd => {
                (launchd::plist_path(scope)?, Some(launchd::log_dir(scope)?))
            }
        };
        Ok(Self {
            platform,
            scope,
            definition_path,
            log_dir,
        })
    }

    /// 使用显式路径构造（测试 / 非标准布局）。
    pub fn for_paths(
        platform: ServicePlatform,
        scope: ServiceScope,
        definition_path: PathBuf,
        log_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            platform,
            scope,
            definition_path,
            log_dir,
        }
    }
}

/// 一条服务定义（渲染 systemd unit 或 launchd plist 所需的全部信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub scope: ServiceScope,
    /// 已安装的 `wist-gwlinkd` 绝对路径。
    pub bin: PathBuf,
    /// **绝对**配置路径（`gwlinkd.toml`；服务启动时工作目录不确定）。
    pub config_path: PathBuf,
}

impl ServiceSpec {
    pub fn new(scope: ServiceScope, bin: PathBuf, config_path: PathBuf) -> Self {
        Self {
            scope,
            bin,
            config_path,
        }
    }

    /// systemd 服务管理器操作的 scope 参数（user 作用域需要 `--user`）。
    pub fn systemctl_scope_args(&self) -> Vec<String> {
        match self.scope {
            ServiceScope::System => Vec::new(),
            ServiceScope::User => vec!["--user".to_string()],
        }
    }

    /// launchd 的 domain target（`system` 或 `gui/<uid>`）。
    pub fn launchd_domain(&self) -> String {
        match self.scope {
            ServiceScope::System => "system".to_string(),
            ServiceScope::User => format!("gui/{}", current_uid()),
        }
    }

    /// launchd 运维 target（`<domain>/<label>`）。
    pub fn launchd_target(&self) -> String {
        format!("{}/{}", self.launchd_domain(), LAUNCHD_LABEL)
    }
}

/// 渲染服务定义文本。
pub fn render(layout: &ServiceLayout, spec: &ServiceSpec) -> String {
    match layout.platform {
        ServicePlatform::Systemd => systemd::unit_text(spec),
        ServicePlatform::Launchd => launchd::plist_text(spec, layout.log_dir.as_deref()),
    }
}

/// `$HOME` 目录（user 作用域的所有路径都挂在它下面）。
pub fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME 未设置；请显式给 --config / --bin".to_string())
}

/// 作用域的默认配置目录：system → `/etc/wist-gwlinkd`；user → `$HOME/.wist-gwlinkd`。
pub fn default_config_dir(scope: ServiceScope) -> Result<PathBuf, String> {
    match scope {
        ServiceScope::System => Ok(PathBuf::from(SYSTEM_CONFIG_DIR)),
        ServiceScope::User => Ok(home_dir()?.join(USER_CONFIG_DIR_NAME)),
    }
}

/// 作用域默认配置**文件**路径（`<config_dir>/gwlinkd.toml`）。
pub fn default_config_path(scope: ServiceScope) -> Result<PathBuf, String> {
    Ok(default_config_dir(scope)?.join(CONFIG_FILE_NAME))
}

/// 当前可执行文件的规范化绝对路径，作为 `--bin` 的默认值。
pub fn default_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("解析当前可执行文件失败（service install）: {err}"))?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// 安装结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub platform: ServicePlatform,
    pub definition_path: PathBuf,
    pub overwritten: bool,
    pub bin_present: bool,
    pub log_dir: Option<PathBuf>,
}

/// 落盘服务定义（不做任何服务管理器调用）。
pub fn install(
    layout: &ServiceLayout,
    spec: &ServiceSpec,
    force: bool,
) -> Result<InstallReport, String> {
    reject_unrepresentable_path(spec)?;

    let definition_path = layout.definition_path.clone();
    let existed = definition_path.exists();
    if existed && !force {
        return Err(format!(
            "服务定义已存在：{}（要覆盖请加 --force）",
            definition_path.display()
        ));
    }

    let parent = definition_path
        .parent()
        .ok_or_else(|| format!("{} 没有父目录", definition_path.display()))?;
    fs::create_dir_all(parent).map_err(|err| format!("建目录 {} 失败: {err}", parent.display()))?;

    // 原子写：先写临时文件再 rename，避免留下半截的服务定义。
    let text = render(layout, spec);
    write_atomic(&definition_path, text.as_bytes())
        .map_err(|err| format!("写服务定义 {} 失败: {err}", definition_path.display()))?;

    if let Some(log_dir) = layout.log_dir.as_ref() {
        fs::create_dir_all(log_dir)
            .map_err(|err| format!("建日志目录 {} 失败: {err}", log_dir.display()))?;
    }

    Ok(InstallReport {
        platform: layout.platform,
        definition_path,
        overwritten: existed,
        bin_present: spec.bin.is_file(),
        log_dir: layout.log_dir.clone(),
    })
}

/// 删除服务定义（不做任何服务管理器调用），返回是否真的删掉了文件。
pub fn remove(layout: &ServiceLayout) -> Result<bool, String> {
    match fs::remove_file(&layout.definition_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!(
            "删除服务定义 {} 失败: {err}",
            layout.definition_path.display()
        )),
    }
}

/// 服务定义是**行/XML 结构**：含换行的路径无法安全表达（会被当成下一条指令），直接拒绝。
fn reject_unrepresentable_path(spec: &ServiceSpec) -> Result<(), String> {
    for (label, value) in [
        ("binary", spec.bin.display().to_string()),
        ("config path", spec.config_path.display().to_string()),
    ] {
        if value.contains('\n') || value.contains('\r') {
            return Err(format!("{label} 含换行，无法写进服务定义：{value:?}"));
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// 服务管理器**拆除是异步的**（launchd 的 bootout 返回后进程仍可能在退出中），
/// 紧随其后的加载可能拿到瞬时错误，因此留一小段重试窗口。
pub const ACTIVATE_RETRIES: u32 = 5;
pub const ACTIVATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// 一条服务管理器命令。`ignore_failure` 用于幂等前置步骤（如先 bootout 再 bootstrap）。
/// `retries` 是失败后**额外**重试的次数（0 = 不重试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCommand {
    pub program: String,
    pub args: Vec<String>,
    pub ignore_failure: bool,
    pub retries: u32,
}

impl ServiceCommand {
    fn new(program: &str, args: Vec<String>, ignore_failure: bool) -> Self {
        Self {
            program: program.to_string(),
            args,
            ignore_failure,
            retries: 0,
        }
    }

    /// 标记为「可重试」：只在失败时生效（成功立刻返回，不引入额外等待）。
    fn retryable(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    /// 可直接粘到终端复现的命令行。
    pub fn display_line(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(shell_quote(&self.program));
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

/// 让服务管理器加载并启动服务（幂等）。
pub fn activate_commands(layout: &ServiceLayout, spec: &ServiceSpec) -> Vec<ServiceCommand> {
    match layout.platform {
        ServicePlatform::Systemd => {
            let mut reload_args = spec.systemctl_scope_args();
            reload_args.push("daemon-reload".to_string());

            let mut enable_args = spec.systemctl_scope_args();
            enable_args.push("enable".to_string());
            enable_args.push(SERVICE_NAME.to_string());

            // 必须是 `restart` 而不是 `enable --now`：`--now` 在**已 active** 的 unit 上
            // 等同 start，是 no-op —— `install --force`（换二进制 / 改定义）会留着旧进程跑旧二进制。
            // `restart` 在「首次安装（未启动）」时也会拉起来，两种情形都正确。
            let mut restart_args = spec.systemctl_scope_args();
            restart_args.push("restart".to_string());
            restart_args.push(SERVICE_NAME.to_string());

            vec![
                ServiceCommand::new("systemctl", reload_args, false),
                ServiceCommand::new("systemctl", enable_args, false),
                ServiceCommand::new("systemctl", restart_args, false).retryable(ACTIVATE_RETRIES),
            ]
        }
        ServicePlatform::Launchd => {
            let domain = spec.launchd_domain();
            vec![
                // 已加载时 bootstrap 会失败，先无脑 bootout 一次（失败可忽略）。
                ServiceCommand::new(
                    "launchctl",
                    vec!["bootout".to_string(), spec.launchd_target()],
                    true,
                ),
                // bootout 返回 ≠ 旧进程已退出；紧随其后的 bootstrap 可能拿到瞬时错误，允许重试。
                ServiceCommand::new(
                    "launchctl",
                    vec![
                        "bootstrap".to_string(),
                        domain,
                        layout.definition_path.display().to_string(),
                    ],
                    false,
                )
                .retryable(ACTIVATE_RETRIES),
                ServiceCommand::new(
                    "launchctl",
                    vec!["enable".to_string(), spec.launchd_target()],
                    false,
                ),
            ]
        }
    }
}

/// 停止并取消托管（卸载前调用，幂等）。
pub fn deactivate_commands(platform: ServicePlatform, spec: &ServiceSpec) -> Vec<ServiceCommand> {
    match platform {
        ServicePlatform::Systemd => {
            let mut disable_args = spec.systemctl_scope_args();
            disable_args.push("disable".to_string());
            disable_args.push("--now".to_string());
            disable_args.push(SERVICE_NAME.to_string());

            let mut reload_args = spec.systemctl_scope_args();
            reload_args.push("daemon-reload".to_string());

            vec![
                ServiceCommand::new("systemctl", disable_args, true),
                ServiceCommand::new("systemctl", reload_args, false),
            ]
        }
        ServicePlatform::Launchd => vec![ServiceCommand::new(
            "launchctl",
            vec!["bootout".to_string(), spec.launchd_target()],
            true,
        )],
    }
}

/// 运维检查命令（只打印，不执行）。
pub fn inspect_commands(platform: ServicePlatform, spec: &ServiceSpec) -> Vec<ServiceCommand> {
    match platform {
        ServicePlatform::Systemd => {
            let mut status_args = spec.systemctl_scope_args();
            status_args.push("status".to_string());
            status_args.push(SERVICE_NAME.to_string());

            let mut log_args = spec.systemctl_scope_args();
            log_args.push("-u".to_string());
            log_args.push(SERVICE_NAME.to_string());
            log_args.push("-f".to_string());

            vec![
                ServiceCommand::new("systemctl", status_args, false),
                ServiceCommand::new("journalctl", log_args, false),
            ]
        }
        ServicePlatform::Launchd => vec![ServiceCommand::new(
            "launchctl",
            vec!["print".to_string(), spec.launchd_target()],
            false,
        )],
    }
}

/// 服务状态自检结果（只读：不创建目录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub platform: ServicePlatform,
    pub scope: ServiceScope,
    pub definition_path: PathBuf,
    pub definition_present: bool,
    pub bin: PathBuf,
    pub bin_present: bool,
    pub config_path: PathBuf,
    pub config_present: bool,
    /// 配置读不到时的原因，供运维定位「为什么落点都是 `-`」。
    pub config_error: Option<String>,
    pub state_dir: Option<PathBuf>,
    /// 单实例锁是否已被持有（= 已有 gwlinkd 在跑）；`None` = 无法判定。
    pub running: Option<bool>,
    pub log_hint: String,
}

/// 采集服务状态。
pub fn status(layout: &ServiceLayout, spec: &ServiceSpec) -> Result<ServiceStatus, String> {
    let config_present = spec.config_path.is_file();
    let (config, config_error) = if config_present {
        match crate::config::Config::load(&spec.config_path) {
            Ok(config) => (Some(config), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (None, None)
    };
    let state_dir = config.as_ref().map(|config| config.state_dir.clone());
    let running = state_dir
        .as_deref()
        .map(crate::state::is_running)
        .transpose()?;

    Ok(ServiceStatus {
        platform: layout.platform,
        scope: layout.scope,
        definition_present: layout.definition_path.is_file(),
        definition_path: layout.definition_path.clone(),
        bin_present: spec.bin.is_file(),
        bin: spec.bin.clone(),
        config_present,
        config_path: spec.config_path.clone(),
        config_error,
        state_dir,
        running,
        log_hint: log_hint(layout)?,
    })
}

/// 日志查看提示（systemd 走 journald，launchd 走落盘文件）。
pub fn log_hint(layout: &ServiceLayout) -> Result<String, String> {
    match layout.platform {
        ServicePlatform::Systemd => {
            let scope = match layout.scope {
                ServiceScope::System => "",
                ServiceScope::User => "--user ",
            };
            Ok(format!("journalctl {scope}-u {SERVICE_NAME} -f"))
        }
        ServicePlatform::Launchd => {
            let log_dir = layout
                .log_dir
                .clone()
                .ok_or_else(|| "launchd 缺日志目录".to_string())?;
            Ok(format!(
                "tail -f {}",
                log_dir.join(launchd::STDERR_FILE).display()
            ))
        }
    }
}

/// 执行一条服务管理器命令，返回是否成功（含 stdout / stderr 供 CLI 回显）。
pub fn run(command: &ServiceCommand) -> Result<ServiceCommandOutcome, String> {
    let output = std::process::Command::new(&command.program)
        .args(&command.args)
        .output()
        .map_err(|err| format!("执行 `{}` 失败: {err}", command.display_line()))?;

    Ok(ServiceCommandOutcome {
        command: command.clone(),
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// 按 `command.retries` 重试执行：成功立刻返回，失败才等待后重试，返回**最后一次**结果。
pub fn run_with_retries(command: &ServiceCommand) -> Result<ServiceCommandOutcome, String> {
    let mut attempt = 0;
    loop {
        let outcome = run(command)?;
        if outcome.success || attempt >= command.retries {
            return Ok(outcome);
        }
        attempt += 1;
        std::thread::sleep(ACTIVATE_RETRY_DELAY);
    }
}

/// 一条命令的执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCommandOutcome {
    pub command: ServiceCommand,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// 当前进程 uid（launchd 的 `gui/<uid>` domain 需要）。
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// 只读探测某个路径是否存在（供状态输出使用）。
pub fn path_state(path: &Path) -> String {
    if path.exists() {
        format!("{} (present)", path.display())
    } else {
        format!("{} (missing)", path.display())
    }
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '-' | '_' | '=' | ':' | '@')
        })
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scope: ServiceScope) -> ServiceSpec {
        ServiceSpec::new(
            scope,
            PathBuf::from("/usr/local/bin/wist-gwlinkd"),
            PathBuf::from("/etc/wist-gwlinkd/gwlinkd.toml"),
        )
    }

    #[test]
    fn systemd_render_sets_restart_and_config_env() {
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Systemd,
            ServiceScope::System,
            PathBuf::from("/etc/systemd/system/wist-gwlinkd.service"),
            None,
        );
        let text = render(&layout, &spec(ServiceScope::System));
        assert!(text.contains("Restart=always"), "{text}");
        assert!(
            text.contains("ExecStart=/usr/local/bin/wist-gwlinkd run"),
            "{text}"
        );
        assert!(
            text.contains("Environment=WIST_GWLINKD_CONFIG=/etc/wist-gwlinkd/gwlinkd.toml"),
            "{text}"
        );
        assert!(text.contains("WantedBy=multi-user.target"), "{text}");
    }

    #[test]
    fn launchd_render_keeps_alive_and_config_env() {
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Launchd,
            ServiceScope::System,
            PathBuf::from("/Library/LaunchDaemons/com.dayu-sec.wist-gwlinkd.plist"),
            Some(PathBuf::from("/var/log/wist-gwlinkd")),
        );
        let text = render(&layout, &spec(ServiceScope::System));
        assert!(text.contains("<key>KeepAlive</key>"), "{text}");
        assert!(text.contains("<key>RunAtLoad</key>"), "{text}");
        assert!(text.contains("com.dayu-sec.wist-gwlinkd"), "{text}");
        assert!(text.contains("<key>WIST_GWLINKD_CONFIG</key>"), "{text}");
    }

    #[test]
    fn install_refuses_to_overwrite_without_force() {
        let dir = std::env::temp_dir().join(format!("gwlinkd-svc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let def = dir.join("wist-gwlinkd.service");
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Systemd,
            ServiceScope::System,
            def.clone(),
            None,
        );
        let spec = spec(ServiceScope::System);
        install(&layout, &spec, false).expect("first install");
        assert!(def.is_file());
        assert!(install(&layout, &spec, false).is_err(), "第二次应拒绝");
        assert!(install(&layout, &spec, true).is_ok(), "--force 应允许覆盖");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_paths_are_scoped() {
        assert_eq!(
            default_config_path(ServiceScope::System).expect("cfg"),
            PathBuf::from("/etc/wist-gwlinkd/gwlinkd.toml")
        );
        assert_eq!(
            default_config_dir(ServiceScope::System).expect("cfg dir"),
            PathBuf::from("/etc/wist-gwlinkd")
        );
    }
}
