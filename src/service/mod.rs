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

use orion_error::prelude::*;

use crate::error::{ServiceReason, ServiceResult};

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
    pub fn resolve(platform: ServicePlatform, scope: ServiceScope) -> ServiceResult<Self> {
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
    /// **现场态**配置的绝对路径（`gwlinkd-pub.toml`）：`Some` 时写进
    /// `WIST_GWLINKD_PUB_CONFIG`。
    ///
    /// 为什么要在 unit 里显式给：现场接线（中心地址 / 网关标识 / 信任锚）落在**主配置之外**
    /// （主配置是渲染物，重渲会把它抹掉），而它不一定与主配置同目录（栈布局下在
    /// `identity/gwlinkd/`）。不给就只能靠同目录约定名 —— 那条在栈布局下找不到。
    pub pub_config: Option<PathBuf>,
    /// `system` 作用域下**以该用户身份运行**（systemd `User=` / launchd `UserName`）。
    /// `None` = 沿用默认（system 作用域 = root）。
    /// 为什么要它：gwlinkd 会回写 `gwlinkd.toml`（接入券）、在 `state_dir` 落身份/凭据，
    /// 且 `upgrader` 会在 `upgrade_project_dir` 里跑 `gops prj upgrade` —— 这些都应是**部署用户**
    /// 的属主，不能是 root。`user` 作用域无此字段（那本就以本人运行）。
    pub run_as: Option<String>,
    /// 与 [`Self::run_as`] 配套的组（systemd `Group=` / launchd `GroupName`）；`None` = 用户主组。
    pub run_as_group: Option<String>,
}

impl ServiceSpec {
    pub fn new(scope: ServiceScope, bin: PathBuf, config_path: PathBuf) -> Self {
        Self {
            scope,
            bin,
            config_path,
            pub_config: None,
            run_as: None,
            run_as_group: None,
        }
    }

    /// 设置**现场态**配置路径（`--pub-config`）：写进 unit / plist 的环境变量。
    pub fn with_pub_config(mut self, pub_config: Option<PathBuf>) -> Self {
        self.pub_config = pub_config;
        self
    }

    /// 设置 `system` 作用域的运行身份（`--run-as` / `--run-as-group`）。
    pub fn with_run_as(mut self, run_as: Option<String>, run_as_group: Option<String>) -> Self {
        self.run_as = run_as;
        self.run_as_group = run_as_group;
        self
    }

    /// 运行身份是否完整可用（systemd `User=` 才认；user 作用域忽略该字段）。
    pub fn run_as_identity(&self) -> Option<&str> {
        match self.scope {
            ServiceScope::System => self.run_as.as_deref(),
            ServiceScope::User => None,
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
pub fn home_dir() -> ServiceResult<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ServiceReason::MissingEnv.err("HOME 未设置；请显式给 --config / --bin"))
}

/// 服务进程的 PATH：系统目录 + 运行用户的 `~/bin` / `~/.local/bin`。
///
/// 为什么必须带后两者：安装脚本把 `gops` / `gx` 装到 `$HOME/bin`，而 launchd / systemd 的
/// 默认 PATH 只有系统目录。gwlinkd 的升级执行器（`gops`）以及 gops 拉起的 gx 流程都靠 PATH
/// 解析；不带就会在 spawn 时以 `os error 2` 失败（相对名 `gops` 配了也没用）。
pub(crate) fn service_path(spec: &ServiceSpec) -> String {
    let mut parts = vec![
        // 与 systemd 的默认 PATH 保持同一组系统目录（launchd 旧版少了 sbin，补回来不丢东西）。
        "/usr/local/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/sbin".to_string(),
        "/usr/bin".to_string(),
        "/sbin".to_string(),
        "/bin".to_string(),
    ];
    if let Some(home) = run_as_home(spec) {
        parts.push(home.join("bin").display().to_string());
        parts.push(home.join(".local/bin").display().to_string());
    }
    parts.join(":")
}

/// 服务实际运行身份的家目录：user 作用域 = 本人；system + `--run-as` = 目标用户；
/// system 无 `--run-as` = root（不额外加用户目录）。
fn run_as_home(spec: &ServiceSpec) -> Option<PathBuf> {
    if spec.scope == ServiceScope::User {
        return home_dir().ok();
    }
    spec.run_as.as_deref().and_then(user_home)
}

#[cfg(unix)]
fn user_home(user: &str) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};

    let user_c = CString::new(user).ok()?;
    // 用重入版：`getpwnam` 返回静态缓冲，并行单测 / 多线程渲染会互相踩。
    // `getpwnam_r` 把结果写进调用方提供的 `passwd` + 缓冲区，`pw_dir` 指向缓冲区，
    // 所以下面必须先复制成 `PathBuf`，再让缓冲区离开作用域。
    let mut buf = vec![0_u8; 16 * 1024];
    loop {
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                user_c.as_ptr(),
                &mut passwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1024 * 1024 {
            let next = buf.len() * 2;
            buf.resize(next, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        let home = passwd.pw_dir;
        if home.is_null() {
            return None;
        }
        let home = unsafe { CStr::from_ptr(home) }.to_str().ok()?;
        return (!home.is_empty()).then(|| PathBuf::from(home));
    }
}

#[cfg(not(unix))]
fn user_home(_user: &str) -> Option<PathBuf> {
    None
}

/// 作用域的默认配置目录：system → `/etc/wist-gwlinkd`；user → `$HOME/.wist-gwlinkd`。
pub fn default_config_dir(scope: ServiceScope) -> ServiceResult<PathBuf> {
    match scope {
        ServiceScope::System => Ok(PathBuf::from(SYSTEM_CONFIG_DIR)),
        ServiceScope::User => Ok(home_dir()?.join(USER_CONFIG_DIR_NAME)),
    }
}

/// 作用域默认配置**文件**路径（`<config_dir>/gwlinkd.toml`）。
pub fn default_config_path(scope: ServiceScope) -> ServiceResult<PathBuf> {
    Ok(default_config_dir(scope)?.join(CONFIG_FILE_NAME))
}

/// 当前可执行文件的规范化绝对路径，作为 `--bin` 的默认值。
pub fn default_bin() -> ServiceResult<PathBuf> {
    let exe = std::env::current_exe().source_err(
        ServiceReason::Io,
        "解析当前可执行文件失败（service install）",
    )?;
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
) -> ServiceResult<InstallReport> {
    reject_unrepresentable_path(spec)?;

    let definition_path = layout.definition_path.clone();
    let existed = definition_path.exists();
    if existed && !force {
        return Err(ServiceReason::InvalidArgs.err(format!(
            "服务定义已存在：{}（要覆盖请加 --force）",
            definition_path.display()
        )));
    }

    let parent = definition_path.parent().ok_or_else(|| {
        ServiceReason::InvalidArgs.err(format!("{} 没有父目录", definition_path.display()))
    })?;
    fs::create_dir_all(parent).source_err(
        ServiceReason::Io,
        format!("建目录 {} 失败", parent.display()),
    )?;

    // 日志目录（launchd 系统级还要交给运行身份）—— 放在写定义**之前**：
    // `--run-as` 查不到用户 / 目录建不出来时，就该在这里失败，别留个「定义写好了但起不来」的现场。
    if let Some(log_dir) = layout.log_dir.as_ref() {
        fs::create_dir_all(log_dir).source_err(
            ServiceReason::Io,
            format!("建日志目录 {} 失败", log_dir.display()),
        )?;
        // launchd 会用 **`UserName`** 去打开 stdio 文件，而这里是以 root 建的目录（0755 root:wheel）——
        // 交给运行身份，否则带 `--run-as` 的 LaunchDaemon 根本拉不起来（`posix_spawn` EACCES，
        // 且**应用日志永远是空的**）。
        if layout.platform == ServicePlatform::Launchd {
            launchd::hand_log_dir_to_run_as(log_dir, spec)?;
        }
    }

    // 原子写：先写临时文件再 rename，避免留下半截的服务定义。
    let text = render(layout, spec);
    write_atomic(&definition_path, text.as_bytes()).map_err(|err| {
        ServiceReason::Io.err(format!(
            "写服务定义 {} 失败: {err}",
            definition_path.display()
        ))
    })?;

    Ok(InstallReport {
        platform: layout.platform,
        definition_path,
        overwritten: existed,
        bin_present: spec.bin.is_file(),
        log_dir: layout.log_dir.clone(),
    })
}

/// 删除服务定义（不做任何服务管理器调用），返回是否真的删掉了文件。
pub fn remove(layout: &ServiceLayout) -> ServiceResult<bool> {
    match fs::remove_file(&layout.definition_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ServiceReason::Io.err(format!(
            "删除服务定义 {} 失败: {err}",
            layout.definition_path.display()
        ))),
    }
}

/// 服务定义是**行/XML 结构**：含换行的路径无法安全表达（会被当成下一条指令），直接拒绝。
fn reject_unrepresentable_path(spec: &ServiceSpec) -> ServiceResult<()> {
    for (label, value) in [
        ("binary", spec.bin.display().to_string()),
        ("config path", spec.config_path.display().to_string()),
    ] {
        if value.contains('\n') || value.contains('\r') {
            return Err(ServiceReason::InvalidArgs
                .err(format!("{label} 含换行，无法写进服务定义：{value:?}")));
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
pub fn status(layout: &ServiceLayout, spec: &ServiceSpec) -> ServiceResult<ServiceStatus> {
    let config_present = spec.config_path.is_file();
    let (config, config_error) = if config_present {
        match crate::config::Config::load(&spec.config_path) {
            Ok(config) => (Some(config), None),
            Err(err) => (None, Some(err.to_string())),
        }
    } else {
        (None, None)
    };
    let state_dir = config.as_ref().map(|config| config.state_dir.clone());
    let running = state_dir
        .as_deref()
        .map(crate::state::is_running)
        .transpose()
        .map_err(|err| ServiceReason::Io.err(err.to_string()))?;

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
pub fn log_hint(layout: &ServiceLayout) -> ServiceResult<String> {
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
                .ok_or_else(|| ServiceReason::MissingEnv.err("launchd 缺日志目录"))?;
            Ok(format!(
                "tail -f {}",
                log_dir.join(launchd::STDERR_FILE).display()
            ))
        }
    }
}

/// 执行一条服务管理器命令，返回是否成功（含 stdout / stderr 供 CLI 回显）。
pub fn run(command: &ServiceCommand) -> ServiceResult<ServiceCommandOutcome> {
    let output = std::process::Command::new(&command.program)
        .args(&command.args)
        .output()
        .source_err(
            ServiceReason::Command,
            format!("执行 `{}` 失败", command.display_line()),
        )?;

    Ok(ServiceCommandOutcome {
        command: command.clone(),
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// 按 `command.retries` 重试执行：成功立刻返回，失败才等待后重试，返回**最后一次**结果。
pub fn run_with_retries(command: &ServiceCommand) -> ServiceResult<ServiceCommandOutcome> {
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

/// 把 `--run-as` / `--run-as-group` 的**用户名 / 组名**解析成 `(uid, gid)`。
///
/// 名字进 unit / plist 是对的（launchd / systemd 都按名字解析），但本进程要把目录**交出去**，
/// 就得自己查一遍 —— 查不到（用户/组不存在）在这里直接报错，别留个「定义写好了但起不来」的现场。
#[cfg(unix)]
fn resolve_user(user: &str, group: Option<&str>) -> ServiceResult<(u32, u32)> {
    let user_c = std::ffi::CString::new(user)
        .map_err(|_| ServiceReason::InvalidArgs.err(format!("用户名非法：{user:?}")))?;
    // `getpwnam` / `getgrnam` 返回**静态缓冲**、非线程安全。本仓只有本处（`install` 的准备阶段）
    // 调它，没有并发查询 —— 若将来要在并发路径上查用户/组，得换成 `getpwnam_r` / `getgrnam_r`。
    let passwd = unsafe { libc::getpwnam(user_c.as_ptr()) };
    if passwd.is_null() {
        return Err(ServiceReason::InvalidArgs.err(format!("查不到用户：{user}（--run-as）")));
    }
    let (uid, default_gid) = unsafe { ((*passwd).pw_uid, (*passwd).pw_gid) };
    let gid = match group {
        Some(group) => resolve_group(group)?,
        None => default_gid,
    };
    Ok((uid, gid))
}

#[cfg(unix)]
fn resolve_group(group: &str) -> ServiceResult<u32> {
    let group_c = std::ffi::CString::new(group)
        .map_err(|_| ServiceReason::InvalidArgs.err(format!("组名非法：{group:?}")))?;
    let entry = unsafe { libc::getgrnam(group_c.as_ptr()) };
    if entry.is_null() {
        return Err(
            ServiceReason::InvalidArgs.err(format!("查不到用户组：{group}（--run-as-group）"))
        );
    }
    Ok(unsafe { (*entry).gr_gid })
}

#[cfg(not(unix))]
fn resolve_user(_user: &str, _group: Option<&str>) -> ServiceResult<(u32, u32)> {
    Err(ServiceReason::InvalidArgs.err("--run-as 只支持 Unix 平台"))
}

/// 把某个路径的属主改成 `(uid, gid)`（`what` 只用于报错时说清是什么东西）。
///
/// 用 `chown`（**跟随**符号链接）而不是 `lchown`：我们要的是**被打开的那个文件**可写 ——
/// launchd 写 `StandardOutPath` 时会跟随链接，只改链接本身的属主等于没改。前提也成立：
/// 这些目录是 `service install`（root）建的、不是人人可写，不存在被塞符号链接的余地。
#[cfg(unix)]
fn chown_path(path: &Path, identity: (u32, u32), what: &str) -> ServiceResult<()> {
    use std::os::unix::ffi::OsStrExt;

    let (uid, gid) = identity;
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| ServiceReason::InvalidArgs.err(format!("路径含 NUL：{}", path.display())))?;
    let rc = unsafe { libc::chown(path_c.as_ptr(), uid, gid) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(ServiceReason::Io.err(format!(
            "把{what} {} 交给运行身份（uid={uid}, gid={gid}）失败：{err}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn chown_path(_path: &Path, _identity: (u32, u32), _what: &str) -> ServiceResult<()> {
    Ok(())
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

// ────────────────────── 已装服务定义回落（`--conf` 缺省的只读子命令） ──────────────────────

/// 服务定义里带的落点（能读出几样是几样）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefinitionPaths {
    pub bin: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub pub_config: Option<PathBuf>,
}

/// 已装的服务定义 + 它带的落点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDefinition {
    pub definition_path: PathBuf,
    pub scope: ServiceScope,
    pub platform: ServicePlatform,
    pub paths: DefinitionPaths,
}

/// 读指定作用域的**已装服务定义**（定义不存在 / 读不了 → `None`）。
///
/// 为什么要它：gwlinkd 是 **host 侧的常驻**，它跑的那份配置就写在服务定义里（`WIST_GWLINKD_CONFIG` /
/// `WIST_GWLINKD_PUB_CONFIG` 两个环境变量）。不带这个回落，手动跑一次 `diagnose` 就得把
/// `/Users/…/wist-gateway-stack/configs/gwlinkd/gwlinkd.toml` 这种超长路径敲两遍 ——
/// 而敲错的名字会被静默丢掉、回落到 `./gwlinkd.toml`，报出来是「读取配置失败 gwlinkd.toml」，
/// 看着像配置坏了（2026-10-15 实际踩过）。
///
/// **只读回落**：`run` 之类不落到这里（行为太隐晦）；写类子命令（`install` / `uninstall`）也不用。
pub fn installed_definition(scope: ServiceScope) -> Option<InstalledDefinition> {
    let platform = ServicePlatform::current()?;
    let layout = ServiceLayout::resolve(platform, scope).ok()?;
    let paths = read_definition_paths(platform, &layout.definition_path)?;
    Some(InstalledDefinition {
        definition_path: layout.definition_path,
        scope,
        platform,
        paths,
    })
}

/// 读**一个具体路径**的服务定义并按平台解析（读不了 → `None`）。
///
/// 「读不了」的两种情形都当没有：定义不存在；或存在但读不到（如别的用户的 `0600` 定义 ——
/// agentd 在 macOS 上的 plist 就是 `-rw------- root`）。
fn read_definition_paths(
    platform: ServicePlatform,
    definition_path: &Path,
) -> Option<DefinitionPaths> {
    let text = fs::read_to_string(definition_path).ok()?;
    Some(match platform {
        ServicePlatform::Systemd => systemd::parse_definition(&text),
        ServicePlatform::Launchd => launchd::parse_definition(&text),
    })
}

/// 系统级优先、其次用户级：**已装的那一份**服务定义。
pub fn any_installed_definition() -> Option<InstalledDefinition> {
    installed_definition(ServiceScope::System).or_else(|| installed_definition(ServiceScope::User))
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
    fn systemd_render_sets_run_as_user_and_group_for_system_scope() {
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Systemd,
            ServiceScope::System,
            PathBuf::from("/etc/systemd/system/wist-gwlinkd.service"),
            None,
        );
        let spec = spec(ServiceScope::System)
            .with_run_as(Some("deploy".to_string()), Some("deploy".to_string()));
        let text = render(&layout, &spec);
        assert!(text.contains("User=deploy"), "{text}");
        assert!(text.contains("Group=deploy"), "{text}");
        // 仍是一个 system 级 unit（而非 user 级）。
        assert!(text.contains("WantedBy=multi-user.target"), "{text}");
    }

    #[test]
    fn systemd_render_omits_run_as_for_user_scope() {
        // user 作用域本就以本人运行，不接受 User=（会与 user manager 冲突）。
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Systemd,
            ServiceScope::User,
            PathBuf::from("/home/deploy/.config/systemd/user/wist-gwlinkd.service"),
            None,
        );
        let spec = spec(ServiceScope::User).with_run_as(Some("deploy".to_string()), None);
        let text = render(&layout, &spec);
        assert!(!text.contains("User="), "{text}");
    }

    #[test]
    fn launchd_render_sets_user_name_for_system_scope() {
        let layout = ServiceLayout::for_paths(
            ServicePlatform::Launchd,
            ServiceScope::System,
            PathBuf::from("/Library/LaunchDaemons/com.dayu-sec.wist-gwlinkd.plist"),
            Some(PathBuf::from("/var/log/wist-gwlinkd")),
        );
        let spec = spec(ServiceScope::System).with_run_as(Some("deploy".to_string()), None);
        let text = render(&layout, &spec);
        assert!(text.contains("<key>UserName</key>"), "{text}");
        assert!(text.contains("<string>deploy</string>"), "{text}");
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

    #[cfg(unix)]
    #[test]
    fn resolve_user_reads_identity_from_passwd() {
        // root 一定存在，且 uid / 主组都是 0（root / wheel）—— 两个平台都成立。
        assert_eq!(resolve_user("root", None).expect("root"), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_user_rejects_unknown_user_or_group() {
        assert!(resolve_user("gwlinkd-no-such-user-zzz", None).is_err());
        assert!(resolve_user("root", Some("gwlinkd-no-such-group-zzz")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn chown_path_accepts_current_identity() {
        // 把属主改成**自己**（同 uid/gid）是允许的 —— 所以这条能在普通用户下跑、真过一遍 syscall
        // （能捉到 CString / OsStrExt 那段的错）。
        let dir = std::env::temp_dir().join(format!("gwlinkd-svc-chown-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("gwlinkd.err");
        fs::write(&file, b"").expect("file");

        let me = (current_uid(), unsafe { libc::getgid() });
        chown_path(&dir, me, "测试目录").expect("chown dir");
        chown_path(&file, me, "测试文件").expect("chown file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_log_dir_skips_without_run_as_or_for_user_scope() {
        // 目录**故意不存在**：这两种情形必须在真正碰文件系统之前就返回。
        let missing = std::env::temp_dir().join("gwlinkd-svc-logdir-does-not-exist");
        assert!(launchd::hand_log_dir_to_run_as(&missing, &spec(ServiceScope::System)).is_ok());
        let user_scope = spec(ServiceScope::User).with_run_as(Some("root".to_string()), None);
        assert!(launchd::hand_log_dir_to_run_as(&missing, &user_scope).is_ok());
    }

    #[test]
    fn read_definition_paths_dispatches_by_platform_and_tolerates_missing_files() {
        let dir = std::env::temp_dir().join(format!("gwlinkd-svc-defs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");

        // 用渲染器写出两种定义，再读回来 —— 覆盖「读文件 + 按平台分派」这层胶水。
        let spec = spec(ServiceScope::System)
            .with_pub_config(Some(PathBuf::from("/etc/wist-gwlinkd/gwlinkd-pub.toml")));
        let unit = dir.join("wist-gwlinkd.service");
        let plist = dir.join("com.dayu-sec.wist-gwlinkd.plist");
        fs::write(&unit, systemd::unit_text(&spec)).expect("unit");
        fs::write(&plist, launchd::plist_text(&spec, None)).expect("plist");

        for (platform, path) in [
            (ServicePlatform::Systemd, unit),
            (ServicePlatform::Launchd, plist),
        ] {
            let paths = read_definition_paths(platform, &path).expect("解析");
            assert_eq!(
                paths.bin,
                Some(PathBuf::from("/usr/local/bin/wist-gwlinkd")),
                "{path:?}"
            );
            assert_eq!(
                paths.config_path,
                Some(PathBuf::from("/etc/wist-gwlinkd/gwlinkd.toml")),
                "{path:?}"
            );
            assert_eq!(
                paths.pub_config,
                Some(PathBuf::from("/etc/wist-gwlinkd/gwlinkd-pub.toml")),
                "{path:?}"
            );
        }

        // 文件不存在 → `None`（不是「空落点」——调用方靠这个 `None` 决定不回落）。
        let missing = dir.join("nope.service");
        assert!(read_definition_paths(ServicePlatform::Systemd, &missing).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_log_dir_rejects_unknown_run_as_before_touching_the_fs() {
        // 查不到用户要**前置失败**（别留「定义写好了但起不来」的现场）—— 所以目录故意不存在：
        // 在 chown 之前就该返回 Err，才不会建出一个谁都写不了的目录。
        let missing = std::env::temp_dir().join("gwlinkd-svc-unknown-user-dir");
        let spec = spec(ServiceScope::System)
            .with_run_as(Some("gwlinkd-no-such-user-zzz".to_string()), None);
        assert!(launchd::hand_log_dir_to_run_as(&missing, &spec).is_err());
        assert!(!missing.exists(), "前置失败不该建目录");
    }

    #[cfg(unix)]
    #[test]
    fn hand_log_dir_yields_dir_and_stdio_files_to_run_as() {
        // 把自己当运行身份：chown 到同 uid/gid 是允许的 —— 于是整条路径（含两个 stdio 文件）
        // 能在普通用户下真跑一遍。拿不到用户名（精简环境 / CI）就跳过。
        let Some(me) = std::env::var("USER").ok().filter(|value| !value.is_empty()) else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("gwlinkd-svc-hand-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join(launchd::STDERR_FILE), b"").expect("stderr file");
        fs::write(dir.join(launchd::STDOUT_FILE), b"").expect("stdout file");

        let spec = spec(ServiceScope::System).with_run_as(Some(me), None);
        launchd::hand_log_dir_to_run_as(&dir, &spec).expect("交给运行身份");

        let _ = fs::remove_dir_all(&dir);
    }
}
