//! `wist-gwlinkd` 本机配置：**主配置**（`gwlinkd.toml`）+ **现场态**（`gwlinkd-pub.toml` + `.env`）。
//!
//! 三类内容分文件（与 `wist-gateway` 的 impl/pub 同口径）：
//!   - **主配置**是**交付物**：模板渲染、升级该被覆盖（`state_dir` / 回环自述面 / 投放目录 /
//!     升级参数 / 日志）；
//!   - **现场态声明**（`gwlinkd-pub.toml`）**只写引用**：
//!     `control_center_endpoint = "${CENTER_ENDPOINT}"` —— 它不含任何现场值，因此**可以入库、可以随版本覆盖**；
//!   - **现场值**（与声明同目录的 `.env`，docker-compose 口径）：真值写在这里，**唯一写者**。
//!
//! 加载时（由共享 crate `wist-config` 实现）：读主配置 + 读现场态声明 → 用「**进程环境 > `.env`**」
//! 插值 `${VAR}`（未定义且无 `:-默认值` → **拒启**，fail-closed）→ 两份合并（段可重合、**叶子键不许重复**）。
//!
//! 另有一层**身份目录**（[`Config::identity_dir`]）：不可再生的身份材料（客户端证书 + 私钥、
//! 中心信任锚）与 `state_dir`（锁 / 簿记 / 临时券，可删可重建）**分开** —— 前者要进
//! `sys-prj.yml` 的 `contract`，后者不该。

use std::path::{Path, PathBuf};

use orion_error::prelude::*;
use wist_config::LayerSpec;

use crate::error::{ConfigReason, ConfigResult, lift_config_error};

/// 指向**现场态声明**（接口层配置）的环境变量：显式设了就用它（相对路径按主配置目录解析）；
/// 未设则取**主配置同目录**的约定文件名（存在才用）。
pub const PUB_CONFIG_ENV: &str = "WIST_GWLINKD_PUB_CONFIG";
/// 现场态**声明**的约定文件名（与主配置同目录）：
/// 只写「哪个配置键由哪个环境变量提供」，**不含现场值**。
pub const PUB_CONFIG_FILE_NAME: &str = "gwlinkd-pub.toml";
/// 现场态**值**的约定文件名：与现场态声明**同目录**（docker-compose 的 `.env` 口径）。
pub const PUB_ENV_FILE_NAME: &str = ".env";
/// 指向现场态 `.env` 的环境变量（docker-compose 的 `--env-file` 对应物）：
/// 显式设了就用它（相对路径按现场态声明目录解析）；未设 = 声明旁边的 `.env`。
pub const PUB_ENV_FILE_ENV: &str = "WIST_GWLINKD_ENV_FILE";

/// 本组件的「分层配置」约定 —— 交给共享 crate `wist-config` 干活
/// （主配置 + 现场态声明 `${VAR}` 插值 + 同目录 `.env` 合并 / 回写）。
const LAYER: LayerSpec = LayerSpec {
    overlay_file: PUB_CONFIG_FILE_NAME,
    overlay_env: PUB_CONFIG_ENV,
    values_file: PUB_ENV_FILE_NAME,
    values_env: PUB_ENV_FILE_ENV,
};

/// 本机配置。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    /// 控制中心 endpoint（https，形如 `https://center.example`，可带尾斜杠）。
    ///
    /// **现场态**：由现场态声明（[`PUB_CONFIG_FILE_NAME`]）里的 `${VAR}` 引用，
    /// 真值来自进程环境或同目录的 `.env`（[`PUB_ENV_FILE_NAME`]）；空 = 尚未接入。
    #[serde(default)]
    pub control_center_endpoint: String,
    /// 控制中心信任锚（PEM 路径）。**现场态**，同上；空 = 回落系统根（公网中心）。
    #[serde(default)]
    pub trust_bundle: PathBuf,
    /// 状态目录（锁 / 升级簿记 / 临时注册券 —— **可删可重建**；身份材料见 [`Self::identity_dir`]）。
    pub state_dir: PathBuf,
    /// **身份目录**：不可再生的身份材料落这里 —— 客户端证书 + 私钥（`credential.json`）
    /// 与中心信任锚的落盘副本。与 `state_dir` 分开的理由：这一份要进 `sys-prj.yml` 的 `contract`
    /// （不进包 / 不重渲 / 必进备份），而 `state_dir` 里的锁与簿记**不该**被当成现场事实。
    ///
    /// 缺省 = `state_dir`（向后兼容：不配时行为与旧版完全一致）。与 `state_dir` 同语义：
    /// 相对路径按进程 cwd 解析，**系统服务部署必须给绝对路径**。
    #[serde(default)]
    pub identity_dir: Option<PathBuf>,
    /// 本网关在中心侧的标识（admin 创建实例时确定）。**现场态**（`gwlinkd-pub.toml`）。
    #[serde(default)]
    pub gateway_id: String,
    /// 一次性接入券（Center「生成/轮换接入券」给出）：首跑（本机还没有客户端证书）时
    /// 用它做 link-upstream → register，**被消费即废**。
    ///
    /// 放在这里 = 「什么都不做就自联上」：写好这份配置、起 gwlinkd 即完成接入。
    /// 已注册后不再使用（要重接：改这项，或清 `state_dir` 重新注册）。
    /// 缺省（空 / 不写）= 本机没有待接入的券。
    #[serde(default)]
    pub link_token: Option<String>,
    /// 本机网关容器**自述面** endpoint（如 `https://127.0.0.1:3000`；缺省则不消费自述面）。
    #[serde(default)]
    pub gateway_self_endpoint: Option<String>,
    /// 网关容器 loopback 面（self-state / link-request）的**信任锚**（PEM 路径）。
    /// 网关以自签证书提供 HTTPS 时需要；缺省 = 用系统根。
    #[serde(default)]
    pub gateway_self_ca: Option<PathBuf>,
    /// 发布 ②「Agent 包交付」的**投放目录**（**宿主侧**，绝对路径）：收到中心的 agentd 包后，
    /// gwlinkd 先把它落成文件放这里，再环回把**本机路径**交给网关托管（取包由 gwlinkd、托管在网关）。
    /// 与网关容器的挂载点（[`Self::agent_package_container_dir`]，典型 `/packages`）指向同一份目录。
    /// 缺省 = 未启用 ②（收到 `push-agent-package` 计划会明确报错）。
    #[serde(default)]
    pub agent_package_drop_dir: Option<PathBuf>,
    /// 上面那份投放目录在**网关容器里**的路径（发给网关的 `package_url` 前缀，如 `/packages`）：
    /// 宿主与容器是同一目录、路径写法不同。缺省 = 未启用 ②。
    #[serde(default)]
    pub agent_package_container_dir: Option<String>,
    /// 投放目录**保留份数**：交付成功后按 mtime 保留最新 N 份，其余清掉（`0` = 不清理）。
    /// 缺省 12 —— 投放目录是交付中转区，不应无界增长。见 [`Self::agent_package_drop_dir`]。
    #[serde(default)]
    pub agent_package_drop_keep: Option<usize>,
    /// 客户端证书轮换提前量（秒，缺省 3600）。
    #[serde(default)]
    pub renew_lead_seconds: Option<i64>,
    /// 升级执行器程序（缺省 `gops`）。
    #[serde(default)]
    pub upgrader_program: Option<String>,
    /// 升级失败处置（`rollback-all` | `halt`；缺省 `rollback-all`）。
    #[serde(default)]
    pub upgrade_on_failure: Option<String>,
    /// 栈外健康检查命令（给 gops `--health-cmd`；给 `sh -c`）：**给了才让 gops 据健康判定回滚**。
    #[serde(default)]
    pub upgrade_health_cmd: Option<String>,
    /// 健康检查超时秒数（给 gops `--health-timeout`；缺省由 gops 定）。
    #[serde(default)]
    pub upgrade_health_timeout_seconds: Option<u64>,
    /// 升级成功**佐证**的观测窗口秒数（执行器报成后，等网关自述面恢复健康的最长时间；缺省 300）。
    #[serde(default)]
    pub upgrade_verify_timeout_seconds: Option<u64>,
    /// **执行器工作目录**（cwd）：驱动会把它交给执行器当 cwd（见 `executor::GopsExecutor::command`）。
    /// 通用要求只是「配了就得拿它当 cwd，所以它得是**已存在的目录**」。
    /// `ops-prj.yml` 是 **gops 特有**的要求（`gops prj upgrade` 从 cwd 解析工程、**无**「指定工程」旗标）：
    /// 用 gops 执行器且缺 marker 时，运行期会在发执行器前被拒（`executor::GopsExecutor::preflight`），
    /// `diagnose` 的 `upgrade.project` 只给 **WARN** 提示。
    #[serde(default)]
    pub upgrade_project_dir: Option<PathBuf>,
    /// 只升级该系统（gops 位置参数 NAME；缺省 = 工程里全部已导入系统）。
    #[serde(default)]
    pub upgrade_project_name: Option<String>,
    /// 升级被判死（心跳陈旧）后，是否自动清游标重驱同一计划（缺省 `true`）。
    ///
    /// - `true`：常驻自愈（可能重驱一个已被中断、实际还在跑的计划；靠 gops 工程锁串行化）；
    /// - `false`：仅报告，交由**管理面重派**（更保守，但机器会停在中间态直到人工介入）。
    #[serde(default)]
    pub upgrade_retry_on_dead: Option<bool>,
    /// 无状态工具（`tool-copy`）安装前是否**要求制品架构可校验且与本机一致**（缺省 `true`）。
    ///
    /// - `true`（缺省）：制品读不出 target-triple、或与本机架构 / 操作系统不符 → **拒装**
    ///   （宁可失败，也不用错架构的二进制覆盖本机工具 —— 那会静默报废工具）；
    /// - `false`：只放宽「读不出架构」这一种（记一笔事件后放行）；**已识别出的架构不符仍拒**。
    #[serde(default)]
    pub upgrade_tool_require_arch: Option<bool>,
    /// `[upgrade]` 段：本机**组件目录**（其余扁平 `upgrade_*` 键保持原样）。
    #[serde(default, rename = "upgrade")]
    pub upgrade: UpgradeSection,
    /// `[log]` 段：**本进程自己**的运行日志（级别 / 格式 / 落点）。缺省 `info` / `text` / stderr。
    /// 与网关侧的 `[logs]`（被采集 agent 日志的落盘保留）无关。
    #[serde(default)]
    pub log: wist_log::LogSection,
    /// **加载时**记下的现场态配置落点（不从文件读 —— `load` 填）。
    /// 供 `diagnose` / `unlink` 定位「现场接线与一次性券写在哪」。
    #[serde(skip)]
    pub pub_config_path: Option<PathBuf>,
}

/// `[upgrade]` 段。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct UpgradeSection {
    /// 本机组件目录（`[[upgrade.component]]`）：计划里的组件名 → 本机安装机制。
    #[serde(default)]
    pub component: Vec<UpgradeComponentConfig>,
}

/// 一个本机组件的安装口径。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UpgradeComponentConfig {
    /// 计划里的组件名（与中心发布的 release `component` 对齐）。
    pub name: String,
    /// 安装机制；缺省 `gops-project`（向后兼容：不在目录里的组件也走 gops）。
    #[serde(default)]
    pub install: UpgradeInstall,
    /// 二进制名（`tool-copy` 用）：据此在 `PATH` 上定位**原位置**（如 `galaxy-ops` → `gops`）。
    #[serde(default)]
    pub binary: Option<String>,
}

/// 组件安装机制。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpgradeInstall {
    /// 交给 `gops prj upgrade`（缺省）—— 需要 `upgrade_project_dir` 工程根。
    #[default]
    GopsProject,
    /// **无状态工具**：解包制品后把二进制**复制覆盖**到它在 `PATH` 上的原位置（旧版备份），不经 gops。
    ToolCopy,
}

impl Config {
    /// 从 TOML 文件读配置：主配置 + （存在的话）**现场态**配置，两份合并后再解析。
    ///
    /// 两份的键**不相交** —— 重复即拒（见 [`merge_disjoint`]）：
    /// “两处都写了、到底哪份说了算”不靠加载顺序，靠声明。
    pub fn load(path: &Path) -> ConfigResult<Self> {
        // 主配置 +（存在的话）现场态声明 → 「进程环境 > 同目录 `.env`」插值 `${VAR}` → 无重复叶子合并。
        // 这一段已抽到共享 crate `wist-config`（见其 crate 文档）；这里只把它解析成本结构。
        let doc = wist_config::load_layered_toml(path, &LAYER).map_err(lift_config_error)?;
        let mut config: Self = doc.try_into().source_raw_err(
            ConfigReason::ParseToml,
            format!("解析配置失败 {}", path.display()),
        )?;
        // 现场态配置的落点：即使当前**不存在**也记下来（接入时就是由它创建）。
        config.pub_config_path = Some(pub_config_path(path));
        // `[log] file` 的相对路径按**配置文件所在目录**解析（与 `wist-gateway` 同一口径），
        // 而非进程 cwd —— systemd / launchd 可能以任意 cwd 拉起，相对 cwd 会落到意外位置。
        if let Some(file) = config.log.file.take() {
            config.log.file = Some(if file.is_absolute() {
                file
            } else {
                path.parent().unwrap_or_else(|| Path::new(".")).join(file)
            });
        }
        Ok(config)
    }

    /// 身份材料目录（未配 `identity_dir` 时回落 `state_dir`，向后兼容）。
    pub fn identity_dir(&self) -> &Path {
        self.identity_dir.as_deref().unwrap_or(&self.state_dir)
    }
}

/// 现场态**声明**的**落点**：显式 [`PUB_CONFIG_ENV`] 优先（相对路径按主配置目录解析），
/// 否则取主配置同目录的约定文件名。**不看文件是否存在** —— 靠它对现场态声明寻址
/// （做诊断 / 回写时都要算它）。
///
/// 具体解析在共享 crate `wist-config`（[`wist_config::overlay_path`]）。
pub fn pub_config_path(config_path: &Path) -> PathBuf {
    wist_config::overlay_path(config_path, &LAYER)
}

/// 现场态**值**（`.env`）的落点：与现场态**声明同目录**（docker-compose 口径：`.env` 挨着声明）；
/// 可用 [`PUB_ENV_FILE_ENV`] 显式指定。
///
/// 具体解析在共享 crate `wist-config`（[`wist_config::values_path`]）。
pub fn pub_env_path(pub_path: &Path) -> PathBuf {
    wist_config::values_path(pub_path, &LAYER)
}

/// 把页面提交的接入物落到**现场态的值文件**（`.env`，与现场态声明同目录）。
///
/// **写哪个变量名**由**现场态声明**（`gwlinkd-pub.toml`）决定：
/// 声明里 `control_center_endpoint = "${GWLINKD_CENTER_ENDPOINT}"` ⇒ 写 `GWLINKD_CENTER_ENDPOINT=…`。
/// 如此“配置键 → 变量名”的映射只有一处（声明），部署方可自由选名（如 `GWLINKD_TRUST_BUNDLE`）。
///
/// 为什么写文件：链接关系是**部署接线**，落成 `.env` 即「持久记录」—— 这正是
/// 「在页面那一下 action 里改写」的落点（不落 DB）。而且它**不在声明文件里**，
/// 所以声明随版本重渲 / 覆盖也动不到它。
/// 空 `gateway_id` / 空 `link_token` 不覆盖已存值（页面允许留空）；中心地址**总是**写（空 = 尚未接入）。
pub fn upsert_link_settings(
    pub_path: &Path,
    center_endpoint: &str,
    gateway_id: &str,
    link_token: &str,
    trust_bundle: Option<&Path>,
) -> ConfigResult<()> {
    let ca = trust_bundle.map(|path| path.display().to_string());
    let mut kv: Vec<(&str, &str)> = vec![("control_center_endpoint", center_endpoint.trim())];
    if !gateway_id.trim().is_empty() {
        kv.push(("gateway_id", gateway_id.trim()));
    }
    if !link_token.trim().is_empty() {
        kv.push(("link_token", link_token.trim()));
    }
    if let Some(ca) = ca.as_deref() {
        kv.push(("trust_bundle", ca));
    }
    let undeclared =
        wist_config::upsert_declared_vars(pub_path, &LAYER, &kv).map_err(lift_config_error)?;
    for key in undeclared {
        log::warn!(
            "event=LinkSettingNotDeclared key={key}（现场态声明 {} 里没有该键的 `${{VAR}}` 引用，写 .env 不会生效）",
            pub_path.display()
        );
    }
    Ok(())
}

/// 从现场态**值文件**（`.env`）里**去掉**一次性接入券：被消费 / 解除接入后它就没用了，
/// 留着会让下次首跑**绕过页面自动重连**。幂等：没有该行（含**文件不存在**）不动文件、返回 `false`。
pub fn remove_link_token(pub_path: &Path) -> ConfigResult<bool> {
    wist_config::remove_declared_var(pub_path, &LAYER, "link_token").map_err(lift_config_error)
}

/// `wist-gwlinkd init-config` 写出去的默认**主配置**文本（带注释的 `gwlinkd.toml` 骶架）。
///
/// 由程序生成（与 `wist-gateway init-config` 同一套路）：**只含交付物类键**；现场选择
/// （中心地址 / 网关标识 / 信任锚 / 接入券）在同目录的现场态配置里，见 [`default_pub_config_text`]。
/// 生成的文本必须能被 [`Config::load`] 解析（有测试钉着）—— 否则现场 `init-config` 出来的配置一启动就报错。
pub fn default_config_text() -> String {
    DEFAULT_CONFIG_TEMPLATE.to_string()
}

/// `wist-gwlinkd init-config` 一并写出的默认**现场态声明**文本（`gwlinkd-pub.toml` 骶架）。
pub fn default_pub_config_text() -> String {
    DEFAULT_PUB_CONFIG_TEMPLATE.to_string()
}

/// `wist-gwlinkd init-config` 一并写出的默认**现场态值**文本（`.env` 骶架）。
///
/// 为什么初始化也写它：声明里的 `${VAR}` 是 fail-closed 的（未定义即拒启）—— 先把变量名摆上、
/// 值留空，现场就有一个明确要填的文件，而不会“一启动就报未定义”。
pub fn default_env_text() -> String {
    DEFAULT_ENV_TEMPLATE.to_string()
}

/// 默认配置路径（供 `init-config` 不传参数时用）。
pub fn default_config_path() -> PathBuf {
    PathBuf::from(crate::DEFAULT_CONFIG_PATH)
}

/// 默认主配置模板（由 [`default_config_text`] 返回）。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# wist-gwlinkd 主配置（由 `wist-gwlinkd init-config` 生成）。
#
# **本文件是交付物**：模板渲染，升级 / 重渲会覆盖它 —— 只放「随部署环境渲染」的项。
# 现场选择（上级中心地址 / 网关标识 / 中心信任锚 / 一次性接入券）在同目录的 `gwlinkd-pub.toml`
# （现场态，**永不重渲**）。

# 状态目录（单实例锁 / 升级簿记 / 待消费注册券 —— 可删可重建；**不是**身份材料）
state_dir = "state"

# 身份目录：不可再生的身份材料（客户端证书 + 私钥、中心信任锚的落盘副本）。
# 与 state_dir 分开的理由：这一份要进 sys-prj.yml 的 `contract`（不进包 / 不重渲 / 必进备份），
# 而 state_dir 里的锁与簿记不该被当成现场事实。
# 缺省 = state_dir（不配即与旧版行为一致）。系统服务部署请给**绝对路径**。
# identity_dir = "/var/lib/wist-gwlinkd/identity"

# 可选：本机网关自述面 / 环回面（「链接上级」页面路才需要）
# gateway_self_endpoint = "https://127.0.0.1:3000"
# gateway_self_ca = "identity/public/gateway-ca.crt.pem"

# 可选：发布 ②「Agent 包交付」（gwlinkd 取包、网关托管）。
# 收到中心派下的 agent 包后，gwlinkd 先把包落到宿主投放目录，再环回把**本机路径**交给网关。
# 下面两项指向**同一份**目录（宿主写法 vs 容器写法）：
#   agent_package_drop_dir      = "packages"            # 宿主侧目录（应与网关容器挂载的宿主机目录一致）
#   agent_package_container_dir = "/packages"           # 该目录在网关容器里的路径（发给网关的 package_url 前缀）
#   agent_package_drop_keep     = 12                     # 交付成功后保留最新 N 份（0 = 不清理；缺省 12）

# 可选：本进程自己的运行日志（缺省 info / text / stderr；RUST_LOG 环境变量优先）
# [log]
# level  = "info"                        # 或 "wist_gwlinkd=debug,hyper=warn"
# format = "json"                        # 缺省 text；只有要 JSON 行才需写
# file   = "/var/log/wist-gwlinkd.log"   # 给了就写文件（相对路径按配置文件目录解析；自动建父目录）；缺省写 stderr
# max_bytes       = 67108864             # 单文件上限（字节），写满轮转成 file.1/file.2/…（缺省 64 MiB）
# keep_files      = 4                    # 保留的历史分卷个数（0 = 不留历史；缺省 4）
# max_seconds     = 604800               # 历史分卷保留时长（秒）；0 = 不按时间清（缺省 7 天）
# 注意：轮转由 gwlinkd 进程自己做；**不要再给这个文件挂 logrotate**，两套轮转会互相打架。
"#;

/// 默认**现场态声明**模板（由 [`default_pub_config_text`] 返回）。
///
/// **只写引用，不含现场值** —— 所以本文件随版本走、可被覆盖重渲；真值在同目录的 `.env`。
///
/// 变量名统一用 `GWLINKD_*` 前缀（与网关栈 `sys/setting/vars.yml` / `.env` 同一套名）——
/// 两处叫法不同会出「死键」（写在一个名下、声明引用另一个名，看着配了实际没生效）。
const DEFAULT_PUB_CONFIG_TEMPLATE: &str = r#"# wist-gwlinkd 现场态（接口层）**声明** —— 只写 `${VAR}` 引用，**不含现场值**。
#
# 本文件随版本走（可入库 / 重渲覆盖也动不到现场）：真值在同目录的 `.env`
# （docker-compose 口径：`.env` 挨着本文件；**进程环境优先**）。
# `${VAR}` 未定义且没给 `:-默认值` → **拒启**（fail-closed，不让“忘了写 .env”静默变空）。
#
# 左侧是 gwlinkd 的配置键；右侧变量名**由部署方自定**，本仓默认统一 `GWLINKD_*`
# （网关栈里是 GWLINKD_CENTER_ENDPOINT / GWLINKD_GATEWAY_ID / GWLINKD_TRUST_BUNDLE）。
# 网关「链接上级」页接入时，gwlinkd 会把值写进 `.env` 里**本文件声明的那个变量名**。

# 上级控制中心地址（空 = 尚未接入）
control_center_endpoint = "${GWLINKD_CENTER_ENDPOINT}"

# 本网关在中心侧的标识（须与中心实例名一致）
gateway_id = "${GWLINKD_GATEWAY_ID}"

# 中心 CA-S 信任锚路径（自签中心必需；公网中心可留空 → 回落系统根）
trust_bundle = "${GWLINKD_TRUST_BUNDLE:-}"

# 一次性接入券（由「链接上级」页写入 `.env`；接入成功后 gwlinkd 会删掉那一行）
link_token = "${GWLINKD_LINK_TOKEN:-}"
"#;

/// 默认**现场态值**骶架（由 [`default_env_text`] 返回）。
const DEFAULT_ENV_TEMPLATE: &str = r#"# wist-gwlinkd 现场态（接口层）**值** —— 与同目录 gwlinkd-pub.toml 里的 `${VAR}` 一一对应。
#
# 本文件是**现场值**：升级 / 重渲不动它（声明文件可随时被重渲，会动值）。
# 键名由声明里的 `${VAR}` 决定 —— 本仓默认统一 `GWLINKD_*`（部署方可自定）。
# 由 `wist-gwlinkd init-config` / 安装脚本按缺补齐（键不存在或值为空才写，非空一律保留）。

# 上级控制中心地址（空 = 尚未接入）
GWLINKD_CENTER_ENDPOINT=

# 本网关在中心侧的标识（须与中心实例名一致）
GWLINKD_GATEWAY_ID=gw-local

# 中心 CA-S 信任锚路径（自签中心必需；空 = 回落系统根）
GWLINKD_TRUST_BUNDLE=
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwlinkd-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn loads_required_fields_and_defaults_the_optional_ones() {
        let dir = temp_dir("ok");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.control_center_endpoint, "https://c");
        assert_eq!(config.gateway_id, "gw-1");
        assert!(config.link_token.is_none());
        assert!(config.gateway_self_endpoint.is_none());
        assert!(config.gateway_self_ca.is_none());
        assert!(config.agent_package_drop_dir.is_none());
        assert!(config.agent_package_container_dir.is_none());
        assert!(config.agent_package_drop_keep.is_none());
        assert!(config.renew_lead_seconds.is_none());
        assert!(config.upgrader_program.is_none());
        assert!(config.upgrade_on_failure.is_none());
        assert!(config.upgrade_project_dir.is_none());
        assert!(config.upgrade_project_name.is_none());
        assert!(config.upgrade_retry_on_dead.is_none());
        assert!(config.upgrade_tool_require_arch.is_none());
        assert!(config.upgrade.component.is_empty());
        assert!(config.log.level.is_none());
        assert_eq!(config.log.format, wist_log::LogFormat::Text);
        assert!(config.log.file.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_section_parses_when_present() {
        let dir = temp_dir("log");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n[log]\nlevel = \"warn\"\nformat = \"json\"\nfile = \"/tmp/gwlinkd.log\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.log.level.as_deref(), Some("warn"));
        assert_eq!(config.log.format, wist_log::LogFormat::Json);
        assert_eq!(
            config.log.file.as_deref(),
            Some(Path::new("/tmp/gwlinkd.log"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn log_file_relative_path_resolves_against_config_dir() {
        let dir = temp_dir("log-rel");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n[log]\nfile = \"logs/gwlinkd.log\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        let file = config.log.file.expect("resolved");
        assert!(file.is_absolute(), "should be absolutized: {file:?}");
        assert!(file.ends_with("logs/gwlinkd.log"), "{file:?}");
        assert!(
            file.starts_with(&dir),
            "{file:?} should be under {}",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_token_parses_when_present() {
        let dir = temp_dir("link-token");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nlink_token = \"link_abc\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.link_token.as_deref(), Some("link_abc"));
        let _ = std::fs::remove_dir_all(dir);
    }

    // ─────────── 现场态：声明（gwlinkd-pub.toml）+ 值（.env） ───────────

    /// 四个现场键都用变量引用的声明（与内置骶架同形）。
    const DECLARATION: &str = "control_center_endpoint = \"${CENTER_ENDPOINT}\"\ngateway_id = \"${GATEWAY_ID}\"\ntrust_bundle = \"${TRUST_BUNDLE:-}\"\nlink_token = \"${LINK_TOKEN:-}\"\n";

    fn write_declaration(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(PUB_CONFIG_FILE_NAME);
        std::fs::write(&path, body).expect("write declaration");
        path
    }

    #[test]
    fn upsert_writes_the_declared_variables_into_the_env_file() {
        let dir = temp_dir("upsert-env");
        let pub_path = write_declaration(&dir, DECLARATION);
        upsert_link_settings(
            &pub_path,
            "https://center.example",
            "gw-9",
            "link_abc",
            Some(Path::new("/ca.pem")),
        )
        .expect("upsert");

        let env_text = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert!(
            env_text.contains("CENTER_ENDPOINT=https://center.example"),
            "{env_text}"
        );
        assert!(env_text.contains("GATEWAY_ID=gw-9"), "{env_text}");
        assert!(env_text.contains("LINK_TOKEN=link_abc"), "{env_text}");
        assert!(env_text.contains("TRUST_BUNDLE=/ca.pem"), "{env_text}");
        // 声明文件**不被改**（它不含现场值）—— 这正是“重渲也动不到现场”的来源。
        let decl = std::fs::read_to_string(&pub_path).expect("read declaration");
        assert_eq!(decl, DECLARATION, "声明不应被写入现场值");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_follows_the_declared_variable_names() {
        // 变量名由部署方定：声明写 `${GW_CTR_ENDPOINT}`，就写这个名。
        let dir = temp_dir("upsert-name");
        let pub_path = write_declaration(
            &dir,
            "control_center_endpoint = \"${GW_CTR_ENDPOINT}\"\nlink_token = \"${GW_LINK_TOKEN:-}\"\n",
        );
        upsert_link_settings(&pub_path, "https://c", "", "link_x", None).expect("upsert");
        let env_text = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert!(env_text.contains("GW_CTR_ENDPOINT=https://c"), "{env_text}");
        assert!(env_text.contains("GW_LINK_TOKEN=link_x"), "{env_text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_preserves_other_env_lines_and_blanks() {
        let dir = temp_dir("upsert-keep");
        let pub_path = write_declaration(&dir, DECLARATION);
        std::fs::write(
            pub_env_path(&pub_path),
            "# 现场自注\nGATEWAY_ID=gw-old\nOTHER=keep-me\n",
        )
        .expect("write .env");
        // 空 gateway_id / 空券 → 不覆盖已存值；中心地址总是写。
        upsert_link_settings(&pub_path, "https://c", "", "", None).expect("upsert");
        let env_text = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert!(env_text.contains("# 现场自注"), "{env_text}");
        assert!(
            env_text.contains("GATEWAY_ID=gw-old"),
            "不覆盖空值：{env_text}"
        );
        assert!(env_text.contains("OTHER=keep-me"), "{env_text}");
        assert!(env_text.contains("CENTER_ENDPOINT=https://c"), "{env_text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_quotes_values_that_need_it_and_is_idempotent() {
        let dir = temp_dir("upsert-quote");
        let pub_path = write_declaration(&dir, DECLARATION);
        upsert_link_settings(&pub_path, "https://c", "gw 1", "", None).expect("first");
        let once = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert!(
            once.contains("GATEWAY_ID=\"gw 1\""),
            "含空白应加引号：{once}"
        );
        upsert_link_settings(&pub_path, "https://c", "gw 1", "", None).expect("second");
        let twice = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert_eq!(once, twice, "重复调用结果应稳定");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upsert_requires_the_declaration_file() {
        // 没有声明就不知道写哪个变量名 —— 报错要可执行（而不是静默写一个无用键）。
        let dir = temp_dir("upsert-no-decl");
        let err = upsert_link_settings(&dir.join(PUB_CONFIG_FILE_NAME), "https://c", "", "", None)
            .expect_err("缺声明必须报错");
        assert!(err.to_string().contains("现场态声明缺失"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_link_token_deletes_the_env_line() {
        let dir = temp_dir("remove-token");
        let pub_path = write_declaration(&dir, DECLARATION);
        std::fs::write(
            pub_env_path(&pub_path),
            "# 头部\nCENTER_ENDPOINT=https://c\nLINK_TOKEN=link_abc\nOTHER=keep\n",
        )
        .expect("write .env");
        assert!(remove_link_token(&pub_path).expect("remove"));
        let env_text = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read .env");
        assert!(!env_text.contains("link_abc"), "应去掉券：{env_text}");
        assert!(env_text.contains("# 头部"), "注释应保留：{env_text}");
        assert!(env_text.contains("OTHER=keep"), "{env_text}");
        assert!(!remove_link_token(&pub_path).expect("again"), "幂等");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_link_token_uses_the_declared_name_and_tolerates_a_missing_env() {
        let dir = temp_dir("remove-token-name");
        let pub_path = write_declaration(&dir, "link_token = \"${GW_LINK_TOKEN:-}\"\n");
        // `.env` 不存在 → 幂等 false。
        assert!(!remove_link_token(&pub_path).expect("missing env"));
        std::fs::write(pub_env_path(&pub_path), "# 头\nGW_LINK_TOKEN=link_abc\n").expect("write");
        assert!(remove_link_token(&pub_path).expect("remove"));
        let env_text = std::fs::read_to_string(pub_env_path(&pub_path)).expect("read");
        assert!(!env_text.contains("link_abc"), "{env_text}");
        assert!(env_text.contains("# 头"), "{env_text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_config_text_is_a_loadable_config() {
        // init-config 生成的主配置必须能被解析 —— 否则现场一启动就报错。
        // 现场选择（中心地址 / 网关标识 / 信任锚）已移出现场态配置，故主配置里缺这三项也算合法（default）。
        let parsed: Config = toml::from_str(&default_config_text()).expect("默认配置必须可解析");
        assert_eq!(parsed.gateway_id, "");
        assert_eq!(parsed.control_center_endpoint, "");
        assert!(parsed.link_token.is_none());
    }

    #[test]
    fn default_pub_config_text_declares_the_site_keys_as_references() {
        // 骨架**只写引用**（不含现场值）—— 所以它可以随版本走、被重渲也动不到现场。
        let skeleton = default_pub_config_text();
        assert!(
            skeleton.contains("${GWLINKD_CENTER_ENDPOINT}"),
            "{skeleton}"
        );
        assert!(skeleton.contains("${GWLINKD_LINK_TOKEN:-}"), "{skeleton}");
        assert!(
            !skeleton.contains("https://"),
            "骨架不应含现场值：{skeleton}"
        );
        // 与值骶架同一套变量名：两处叫法不同会出「死键」。
        let env_skeleton = default_env_text();
        for name in [
            "GWLINKD_CENTER_ENDPOINT",
            "GWLINKD_GATEWAY_ID",
            "GWLINKD_TRUST_BUNDLE",
        ] {
            assert!(
                skeleton.contains(&format!("${{{name}")) && env_skeleton.contains(name),
                "{name} 必须在声明与值两份骶架里同名：{skeleton}\n{env_skeleton}"
            );
        }

        // 配上 `.env` 后能真正加载。
        let dir = temp_dir("pub-skeleton");
        let main = write_split(
            &dir,
            "",
            Some(&skeleton),
            Some("GWLINKD_CENTER_ENDPOINT=https://c\nGWLINKD_GATEWAY_ID=gw-s\n"),
        );
        let parsed = Config::load(&main).expect("骨架 + .env 必须可解析");
        assert_eq!(parsed.control_center_endpoint, "https://c");
        assert_eq!(parsed.gateway_id, "gw-s");
        // `:-` 默认值：没给券就是空（= 无券）。
        assert!(parsed.link_token.as_deref().unwrap_or("").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upgrade_component_catalog_parses_with_a_default_install() {
        let dir = temp_dir("components");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"galaxy-ops\"\ninstall = \"tool-copy\"\nbinary = \"gops\"\n\n\
             [[upgrade.component]]\nname = \"galaxy-flow\"\ninstall = \"tool-copy\"\nbinary = \"gx\"\n\n\
             [[upgrade.component]]\nname = \"wist-gateway-stack\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade.component.len(), 3);
        assert_eq!(config.upgrade.component[0].name, "galaxy-ops");
        assert_eq!(
            config.upgrade.component[0].install,
            UpgradeInstall::ToolCopy
        );
        assert_eq!(config.upgrade.component[0].binary.as_deref(), Some("gops"));
        assert_eq!(config.upgrade.component[1].binary.as_deref(), Some("gx"));
        // 不写 install → 缺省 gops-project（向后兼容）。
        assert_eq!(
            config.upgrade.component[2].install,
            UpgradeInstall::GopsProject
        );
        assert!(config.upgrade.component[2].binary.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn install_defaults_to_gops_project_and_parses_kebab_case() {
        assert_eq!(UpgradeInstall::default(), UpgradeInstall::GopsProject);

        let dir = temp_dir("install-values");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"a\"\ninstall = \"gops-project\"\n\n\
             [[upgrade.component]]\nname = \"b\"\ninstall = \"tool-copy\"\nbinary = \"gx\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(
            config.upgrade.component[0].install,
            UpgradeInstall::GopsProject
        );
        assert_eq!(
            config.upgrade.component[1].install,
            UpgradeInstall::ToolCopy
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unknown_install_value_is_rejected() {
        let dir = temp_dir("install-bogus");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\n\n\
             [[upgrade.component]]\nname = \"a\"\ninstall = \"bogus\"\n",
        )
        .expect("write");
        let err = Config::load(&path).expect_err("unknown install must be rejected");
        assert!(err.to_string().contains("配置"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gateway_self_ca_parses_when_present() {
        let dir = temp_dir("self-ca");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\ngateway_self_endpoint = \"https://127.0.0.1:3000\"\ngateway_self_ca = \"/self-ca.pem\"\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(
            config.gateway_self_endpoint.as_deref(),
            Some("https://127.0.0.1:3000")
        );
        assert_eq!(
            config.gateway_self_ca.as_deref(),
            Some(std::path::Path::new("/self-ca.pem"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_required_field_is_an_error() {
        let dir = temp_dir("bad");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(&path, "control_center_endpoint = \"https://c\"\n").expect("write");
        // `state_dir` 是主配置里唯一必填的（其余现场选择键已默认）。
        assert!(Config::load(&path).is_err(), "缺 state_dir 必须报错");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn optional_upgrade_knobs_parse() {
        let dir = temp_dir("upgrade-knobs");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nupgrade_on_failure = \"halt\"\nupgrade_health_cmd = \"curl -fsS http://127.0.0.1:3000/health\"\nupgrade_health_timeout_seconds = 45\nupgrade_verify_timeout_seconds = 7\nupgrade_project_dir = \"/opt/prj\"\nupgrade_project_name = \"wist-gateway\"\nupgrade_retry_on_dead = false\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade_on_failure.as_deref(), Some("halt"));
        assert_eq!(
            config.upgrade_health_cmd.as_deref(),
            Some("curl -fsS http://127.0.0.1:3000/health")
        );
        assert_eq!(config.upgrade_health_timeout_seconds, Some(45));
        assert_eq!(config.upgrade_verify_timeout_seconds, Some(7));
        assert_eq!(
            config.upgrade_project_dir.as_deref(),
            Some(std::path::Path::new("/opt/prj"))
        );
        assert_eq!(config.upgrade_project_name.as_deref(), Some("wist-gateway"));
        assert_eq!(config.upgrade_retry_on_dead, Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_arch_requirement_parses_when_present() {
        let dir = temp_dir("tool-arch-knob");
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            "control_center_endpoint = \"https://c\"\ngateway_id = \"gw-1\"\ntrust_bundle = \"/ca.pem\"\nstate_dir = \"/s\"\nupgrade_tool_require_arch = false\n",
        )
        .expect("write");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.upgrade_tool_require_arch, Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unreadable_path_is_a_clear_error() {
        let dir = temp_dir("missing");
        assert!(Config::load(&dir.join("nope.toml")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    // ─────────── 主配置 + 现场态（声明 → `.env` 插值） ───────────

    /// 写主配置 + （可选）现场态声明 + （可选）`.env`；返回**主配置路径**。
    fn write_split(
        dir: &Path,
        main_extra: &str,
        declaration: Option<&str>,
        env_lines: Option<&str>,
    ) -> PathBuf {
        let main = dir.join("gwlinkd.toml");
        std::fs::write(&main, format!("state_dir = \"/s\"\n{main_extra}")).expect("write main");
        if let Some(body) = declaration {
            std::fs::write(dir.join(PUB_CONFIG_FILE_NAME), body).expect("write declaration");
        }
        if let Some(lines) = env_lines {
            std::fs::write(dir.join(PUB_ENV_FILE_NAME), lines).expect("write .env");
        }
        main
    }

    #[test]
    fn loads_the_site_values_through_the_declaration_and_the_env_file() {
        let dir = temp_dir("split-ok");
        let main = write_split(
            &dir,
            "gateway_self_endpoint = \"https://127.0.0.1\"\n",
            Some(DECLARATION),
            Some("CENTER_ENDPOINT=https://center.example\nGATEWAY_ID=gw-9\nTRUST_BUNDLE=/ca.pem\n"),
        );
        let config = Config::load(&main).expect("load");
        assert_eq!(config.control_center_endpoint, "https://center.example");
        assert_eq!(config.gateway_id, "gw-9");
        assert_eq!(
            config.trust_bundle.as_path(),
            std::path::Path::new("/ca.pem")
        );
        assert_eq!(
            config.gateway_self_endpoint.as_deref(),
            Some("https://127.0.0.1")
        );
        let expected_pub = dir.join(PUB_CONFIG_FILE_NAME);
        assert_eq!(
            config.pub_config_path.as_deref(),
            Some(expected_pub.as_path())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_absent_declaration_leaves_the_site_values_defaulted() {
        let dir = temp_dir("split-none");
        let main = write_split(&dir, "", None, None);
        let config = Config::load(&main).expect("load");
        assert_eq!(config.control_center_endpoint, "");
        assert_eq!(config.gateway_id, "");
        assert!(config.pub_config_path.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_env_file_only_works_through_the_defaults() {
        // `:-` 默认值让“还没写 .env”也能起（可选键）；无默认值的键则必须给值。
        let dir = temp_dir("split-defaults");
        let main = write_split(
            &dir,
            "",
            Some("trust_bundle = \"${TRUST_BUNDLE:-}\"\n"),
            None,
        );
        let config = Config::load(&main).expect("load");
        assert_eq!(config.trust_bundle.as_path(), std::path::Path::new(""));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_undefined_variable_without_a_default_is_rejected() {
        // fail-closed：不让“忘了写 .env”静默变空。
        let dir = temp_dir("split-undefined");
        let main = write_split(
            &dir,
            "",
            Some("control_center_endpoint = \"${GWLINKD_TEST_UNDEFINED_VAR}\"\n"),
            Some("OTHER=1\n"),
        );
        let err = Config::load(&main).expect_err("未定义变量必须报错");
        assert!(err.to_string().contains("未定义的环境变量"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_process_env_wins_over_the_env_file() {
        // docker-compose 口径：shell 里已有的变量优先于 `.env`。
        let dir = temp_dir("split-priority");
        let main = write_split(
            &dir,
            "",
            Some("gateway_id = \"${GWLINKD_TEST_PRIORITY}\"\n"),
            Some("GWLINKD_TEST_PRIORITY=from-env-file\n"),
        );
        // SAFETY: 单测内设置一个**专用**变量名，不与其它测试的变量相交。
        unsafe { std::env::set_var("GWLINKD_TEST_PRIORITY", "from-process-env") };
        let config = Config::load(&main).expect("load");
        unsafe { std::env::remove_var("GWLINKD_TEST_PRIORITY") };
        assert_eq!(config.gateway_id, "from-process-env");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_dot_env_file_lives_beside_the_declaration() {
        let dir = temp_dir("split-dotenv-path");
        let pub_path = dir.join(PUB_CONFIG_FILE_NAME);
        assert_eq!(pub_env_path(&pub_path), dir.join(PUB_ENV_FILE_NAME));
        // 显式指定时跟显式的走。
        unsafe { std::env::set_var(PUB_ENV_FILE_ENV, "/tmp/other.env") };
        let explicit = pub_env_path(&pub_path);
        unsafe { std::env::remove_var(PUB_ENV_FILE_ENV) };
        assert_eq!(explicit, std::path::PathBuf::from("/tmp/other.env"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_same_leaf_key_in_both_files_is_rejected() {
        let dir = temp_dir("split-dup");
        let main = write_split(
            &dir,
            "gateway_self_endpoint = \"https://a\"\n",
            Some("gateway_self_endpoint = \"https://b\"\n"),
            None,
        );
        let err = Config::load(&main).expect_err("同一叶子键出现在两份里必须报错");
        assert!(err.to_string().contains("同一个键只能有一处"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_declared_key_that_is_a_literal_is_loaded_verbatim() {
        // 声明里也可以写**字面量**（不需要变量）—— 那种键就不参与 `.env` 回写。
        let dir = temp_dir("split-literal");
        let main = write_split(
            &dir,
            "",
            Some("gateway_id = \"gw-literal\"\ntrust_bundle = \"/ca.pem\"\n"),
            None,
        );
        let config = Config::load(&main).expect("load");
        assert_eq!(config.gateway_id, "gw-literal");
        assert_eq!(
            config.trust_bundle.as_path(),
            std::path::Path::new("/ca.pem")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn identity_dir_falls_back_to_state_dir() {
        let dir = temp_dir("identity-dir");
        let main = write_split(&dir, "", None, None);
        let config = Config::load(&main).expect("load");
        assert_eq!(config.identity_dir(), std::path::Path::new("/s"));

        std::fs::write(&main, "state_dir = \"/s\"\nidentity_dir = \"/id\"\n").expect("write");
        let config = Config::load(&main).expect("load");
        assert_eq!(config.identity_dir(), std::path::Path::new("/id"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
