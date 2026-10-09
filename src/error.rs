//! Structured error surface for the host-side gateway-link daemon, built on `orion-error`.
//!
//! Each domain owns a small reason enum (derived with [`OrionError`]) carrying only
//! stable, unit-shaped business identities. Dynamic diagnostics (paths, raw backend
//! messages, response bodies, ...) live on the [`StructError`] carrier as detail,
//! context, or source — never inside the reason value.
//!
//! The crate-level [`GwlinkdReason`] lifts the domain reasons at the process boundary
//! (`run` / CLI commands). A single reason becomes an error with
//! [`ToStructError::to_err`]; a normal `Result<T, E>` enters the structured system the
//! first time with [`SourceErr::source_err`] (supported std/feature sources such as
//! `io` / `serde_json`) or [`SourceRawErr::source_raw_err`] (third-party `StdError`
//! without a bridge, e.g. `toml` / `reqwest`). Lower structured errors that only change
//! the reason namespace go through [`ConvErr::conv_err`].
//!
//! See the shared target design (`wist-design/doc/design/foundation/error-handling-system.md`).

use orion_error::{
    conversion::ToStructError,
    prelude::*,
    reason::DomainReason,
    runtime::{AutoLogGuard, OperationContext},
};

/// Error carrier for config loading and validation.
pub type ConfigError = StructError<ConfigReason>;
pub type ConfigResult<T> = Result<T, ConfigError>;

/// Error carrier for the top-level daemon / CLI boundary.
pub type GwlinkdError = StructError<GwlinkdReason>;
pub type GwlinkdResult<T> = Result<T, GwlinkdError>;

/// Reason namespace for config loading and validation.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum ConfigReason {
    #[orion_error(identity = "conf.warp.gwlinkd.config.io")]
    Io,
    #[orion_error(identity = "conf.warp.gwlinkd.config.parse_toml")]
    ParseToml,
    #[orion_error(identity = "conf.warp.gwlinkd.config.validation")]
    Validation,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

impl ConfigReason {
    /// 把一个 reason 落到配置错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> ConfigError {
        self.to_err().with_detail(detail)
    }
}

/// Reason namespace for the top-level daemon / CLI boundary.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum GwlinkdReason {
    #[orion_error(identity = "biz.warp.gwlinkd.cli.invalid_args")]
    InvalidArgs,
    /// 单实例锁已被同机另一个常驻持有。
    #[orion_error(identity = "biz.warp.gwlinkd.runtime.already_running")]
    AlreadyRunning,
    /// 本地长期身份（客户端证书 / 私钥 / 实例 id）缺失或损坏。
    #[orion_error(identity = "biz.warp.gwlinkd.identity")]
    Identity,
    /// 首跑置备 / 注册（link-upstream → register）。
    #[orion_error(identity = "biz.warp.gwlinkd.enrollment")]
    Enrollment,
    /// 中心拒绝了本机客户端证书（401 / 403）：要退避 + 由管理员在中心重置。
    #[orion_error(identity = "biz.warp.gwlinkd.center.unauthorized")]
    CenterUnauthorized,
    /// 与中心的其它调用错误（网络 / 5xx / 解析）。
    #[orion_error(identity = "sys.warp.gwlinkd.center")]
    Center,
    /// 升级驱动 / 无状态工具安装。
    #[orion_error(identity = "sys.warp.gwlinkd.upgrade")]
    Upgrade,
    /// 断开与中心的接入（`unlink`）。
    #[orion_error(identity = "biz.warp.gwlinkd.unlink")]
    Unlink,
    /// OS 服务管理器托管（`service`）。
    #[orion_error(identity = "sys.warp.gwlinkd.service")]
    Service,
    /// 发布 ②：把中心派下的 agent 包环回推进网关包管理。
    #[orion_error(identity = "biz.warp.gwlinkd.agent_package")]
    AgentPackage,
    /// 配置加载 / 校验（lift [`ConfigReason`]）。
    #[orion_error(identity = "biz.warp.gwlinkd.config")]
    Config,
    /// 本地状态读写（lift [`StateReason`]）。
    #[orion_error(identity = "biz.warp.gwlinkd.state")]
    State,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

impl GwlinkdReason {
    /// 把一个 reason 落到错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> GwlinkdError {
        self.to_err().with_detail(detail)
    }
}

impl From<ConfigReason> for GwlinkdReason {
    fn from(value: ConfigReason) -> Self {
        match value {
            ConfigReason::General(reason) => GwlinkdReason::General(reason),
            _ => GwlinkdReason::Config,
        }
    }
}

/// Error carrier for calls to `WistCenter`.
pub type CenterResult<T> = Result<T, CenterError>;

/// Reason namespace for center calls.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum CenterReason {
    /// 读信任锚 / 本地证书材料失败。
    #[orion_error(identity = "sys.warp.gwlinkd.center.io")]
    Io,
    /// 请求发送 / 读响应体失败，或非 2xx（非 401/403）。
    #[orion_error(identity = "sys.warp.gwlinkd.center.http")]
    Http,
    /// 响应体 JSON 解析失败。
    #[orion_error(identity = "sys.warp.gwlinkd.center.decode")]
    Decode,
    /// 401 / 403：中心拒绝本机凭据（要退避 + 由管理员在中心重置）。
    #[orion_error(identity = "biz.warp.gwlinkd.center.unauthorized")]
    Unauthorized,
    /// 本地准备失败（密钥对生成 / 落盘），发生在注册流程里。
    #[orion_error(identity = "sys.warp.gwlinkd.center.local")]
    Local,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

/// Newtype over [`StructError<CenterReason>`] so callers keep a typed
/// [`CenterError::is_unauthorized`] / `Display` and `?` / `From` work across the
/// center seam.
///
/// It exists because a blanket `From<StructError<R1>> for StructError<R2>` is
/// blocked by the orphan rule: wrapping gives a local `Self` type so the boundary
/// `From<CenterError> for GwlinkdError` (and `From<StructError<CenterReason>>`)
/// can be implemented.
#[derive(Debug, Clone)]
pub struct CenterError(StructError<CenterReason>);

impl CenterError {
    /// 是否 401 / 403（凭据被拒）。
    pub fn is_unauthorized(&self) -> bool {
        self.0.reason() == &CenterReason::Unauthorized
    }

    /// detail 是否含给定片段（识别 401 的特定子原因，如 `certificate_required`）。
    pub fn detail_contains(&self, needle: &str) -> bool {
        self.0
            .detail()
            .as_deref()
            .is_some_and(|detail| detail.contains(needle))
    }

    /// 渲染后的完整文案是否含给定片段（诊断 / 断言的便捷入口）。
    pub fn contains(&self, needle: &str) -> bool {
        self.0.to_string().contains(needle)
    }

    pub fn into_inner(self) -> StructError<CenterReason> {
        self.0
    }
}

impl CenterReason {
    /// 把一个 reason 落到中心错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> CenterError {
        CenterError(self.to_err().with_detail(detail))
    }
}

impl From<StructError<CenterReason>> for CenterError {
    fn from(err: StructError<CenterReason>) -> Self {
        Self(err)
    }
}

impl From<CenterError> for String {
    fn from(err: CenterError) -> Self {
        err.0.to_string()
    }
}

impl std::fmt::Display for CenterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CenterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source_ref()
    }
}

/// Error carrier for the local state store (identity / credential / lock / upgrade files).
pub type StateResult<T> = Result<T, StateError>;

/// Reason namespace for the local state store.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum StateReason {
    #[orion_error(identity = "sys.warp.gwlinkd.state.io")]
    Io,
    #[orion_error(identity = "sys.warp.gwlinkd.state.json")]
    Json,
    /// 文件在但内容不成形 / 不可解析（**不**当成缺失静默重建）。
    #[orion_error(identity = "biz.warp.gwlinkd.state.corrupt")]
    Corrupt,
    /// 单实例锁已被同机另一个常驻持有。
    #[orion_error(identity = "biz.warp.gwlinkd.state.already_running")]
    AlreadyRunning,
    /// 读系统随机源失败。
    #[orion_error(identity = "sys.warp.gwlinkd.state.random")]
    Random,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

/// Newtype over [`StructError<StateReason>`].
///
/// Besides the orphan-rule reason for wrapping, it carries `From<StateError> for String`
/// so `?` keeps working in callers that still return `Result<_, String>` during the
/// staged migration (hot path migrates first).
#[derive(Debug, Clone)]
pub struct StateError(StructError<StateReason>);

impl StateError {
    pub fn reason(&self) -> &StateReason {
        self.0.reason()
    }

    /// 渲染后的完整文案是否含给定片段（诊断 / 断言的便捷入口）。
    pub fn contains(&self, needle: &str) -> bool {
        self.0.to_string().contains(needle)
    }

    pub fn into_inner(self) -> StructError<StateReason> {
        self.0
    }
}

impl StateReason {
    /// 把一个 reason 落到状态错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> StateError {
        StateError(self.to_err().with_detail(detail))
    }
}

impl From<StructError<StateReason>> for StateError {
    fn from(err: StructError<StateReason>) -> Self {
        Self(err)
    }
}

impl From<StateError> for String {
    fn from(err: StateError) -> Self {
        err.0.to_string()
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source_ref()
    }
}

/// Error carrier for OS service-manager integration (`service` subcommand).
pub type ServiceResult<T> = Result<T, ServiceError>;

/// Reason namespace for OS service-manager integration.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum ServiceReason {
    #[orion_error(identity = "sys.warp.gwlinkd.service.io")]
    Io,
    #[orion_error(identity = "biz.warp.gwlinkd.service.invalid_args")]
    InvalidArgs,
    #[orion_error(identity = "conf.warp.gwlinkd.service.missing_env")]
    MissingEnv,
    #[orion_error(identity = "sys.warp.gwlinkd.service.command_failed")]
    Command,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

/// Newtype over [`StructError<ServiceReason>`]; see [`StateError`] for why
/// `From<ServiceError> for String` keeps staged `?` working.
#[derive(Debug, Clone)]
pub struct ServiceError(StructError<ServiceReason>);

impl ServiceError {
    pub fn reason(&self) -> &ServiceReason {
        self.0.reason()
    }

    /// 渲染后的完整文案是否含给定片段（诊断 / 断言的便捷入口）。
    pub fn contains(&self, needle: &str) -> bool {
        self.0.to_string().contains(needle)
    }

    pub fn into_inner(self) -> StructError<ServiceReason> {
        self.0
    }
}

impl ServiceReason {
    /// 把一个 reason 落到服务错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> ServiceError {
        ServiceError(self.to_err().with_detail(detail))
    }
}

impl From<StructError<ServiceReason>> for ServiceError {
    fn from(err: StructError<ServiceReason>) -> Self {
        Self(err)
    }
}

impl From<ServiceError> for String {
    fn from(err: ServiceError) -> Self {
        err.0.to_string()
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source_ref()
    }
}

/// Error carrier for the upgrade domain (driver + in-process tool install).
pub type UpgradeResult<T> = Result<T, UpgradeError>;

/// Reason namespace for the upgrade domain.
#[derive(Debug, Clone, PartialEq, OrionError)]
pub enum UpgradeReason {
    #[orion_error(identity = "sys.warp.gwlinkd.upgrade.io")]
    Io,
    /// 执行器缺失 / 启动失败。
    #[orion_error(identity = "sys.warp.gwlinkd.upgrade.executor")]
    Executor,
    /// 前置校验失败（组件不在目录 / PATH 找不到原位置 / 制品地址不可取）。
    #[orion_error(identity = "biz.warp.gwlinkd.upgrade.preflight")]
    Preflight,
    /// 取件 / 解包 / 摘要校验失败。
    #[orion_error(identity = "sys.warp.gwlinkd.upgrade.artifact")]
    Artifact,
    /// 架构不符 / 不可校验。
    #[orion_error(identity = "sys.warp.gwlinkd.upgrade.arch")]
    Arch,
    /// 升级后恢复佐证失败。
    #[orion_error(identity = "biz.warp.gwlinkd.upgrade.recovery")]
    Recovery,
    #[orion_error(transparent)]
    General(UnifiedReason),
}

/// Newtype over [`StructError<UpgradeReason>`]; see [`StateError`] for why
/// `From<UpgradeError> for String` keeps staged `?` working.
#[derive(Debug, Clone)]
pub struct UpgradeError(StructError<UpgradeReason>);

impl UpgradeError {
    pub fn reason(&self) -> &UpgradeReason {
        self.0.reason()
    }

    /// 渲染后的完整文案是否含给定片段（诊断 / 断言的便捷入口）。
    pub fn contains(&self, needle: &str) -> bool {
        self.0.to_string().contains(needle)
    }

    pub fn into_inner(self) -> StructError<UpgradeReason> {
        self.0
    }
}

impl UpgradeReason {
    /// 把一个 reason 落到升级错误载体上（detail 进载体，不进 reason）。
    pub fn err(self, detail: impl Into<String>) -> UpgradeError {
        UpgradeError(self.to_err().with_detail(detail))
    }
}

impl From<StructError<UpgradeReason>> for UpgradeError {
    fn from(err: StructError<UpgradeReason>) -> Self {
        Self(err)
    }
}

impl From<UpgradeError> for String {
    fn from(err: UpgradeError) -> Self {
        err.0.to_string()
    }
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UpgradeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source_ref()
    }
}

/// Record one fallible operation using `orion-error`'s auto-log guard idiom.
///
/// `mod_path` should be the call-site `module_path!()` so the log target points at the
/// real module; `fields` are structured context entries (gateway_id / url / status …).
///
/// The context is wrapped in an [`AutoLogGuard`] (orion-error ≥ 0.9), which writes the
/// lifecycle entry **exactly once** on drop and **at the failure point** — success →
/// `suc!`; failure → `fail!` carrying the full cause chain (folded into a `cause=` field,
/// single line). The context is also attached to the returned error as pure data, so a
/// later `display_chain()` at the boundary shows the same action + fields. Because the
/// guard is never cloned into the error, the failure log is neither duplicated nor
/// deferred (issue galaxio-labs/orion-error#64).
///
/// Level is controlled by `RUST_LOG`. Use it only on **low-frequency** operations — it
/// logs on success too, so wrapping a 30s tick would spam the log.
pub fn logged_op<E, T>(
    mod_path: &str,
    action: &str,
    fields: &[(&str, String)],
    result: Result<T, E>,
) -> Result<T, E>
where
    E: OpLoggable,
{
    // 纯数据装配字段后武装成 guard（默认失败）：不 `mark_success` 就记 `fail!`。
    let mut guard = OperationContext::doing(action)
        .with_mod_path(mod_path)
        .with_auto_log();
    for (key, value) in fields {
        guard = guard.with_field(*key, value.clone());
    }

    match result {
        Ok(value) => {
            guard.mark_success(); // guard 于函数返回时 drop → `suc!`
            Ok(value)
        }
        Err(err) => {
            // 单行化：把完整因果链（含 source）折进 `cause` 字段，Drop 时的 `fail!`
            // 一行看全；同时以纯数据副本附到错误，供边界 `display_chain()` 渲染。
            let chain = err
                .op_display_chain()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            guard = guard.with_field("cause", chain);
            Err(err.op_attach(&guard)) // 借用：只复制数据，不打日志（guard 仍是唯一日志所有者）
        }
    }
}

/// Adapter so [`logged_op`] works over both `StructError<R>` and the per-domain newtype
/// carriers (`CenterError` / `StateError` / `ServiceError` / `UpgradeError`).
pub trait OpLoggable: Sized {
    /// Full cause chain for the failure log line.
    fn op_display_chain(&self) -> String;
    /// Attach the operation data to the error. Takes the guard by shared reference so
    /// only the data is copied; the guard stays the single owner of the drop-time log.
    fn op_attach(self, guard: &AutoLogGuard) -> Self;
}

impl<R: DomainReason> OpLoggable for StructError<R> {
    fn op_display_chain(&self) -> String {
        self.display_chain()
    }

    fn op_attach(self, guard: &AutoLogGuard) -> Self {
        self.with_context(guard)
    }
}

impl OpLoggable for CenterError {
    fn op_display_chain(&self) -> String {
        self.0.display_chain()
    }

    fn op_attach(self, guard: &AutoLogGuard) -> Self {
        Self(self.0.with_context(guard))
    }
}

impl OpLoggable for StateError {
    fn op_display_chain(&self) -> String {
        self.0.display_chain()
    }

    fn op_attach(self, guard: &AutoLogGuard) -> Self {
        Self(self.0.with_context(guard))
    }
}

impl OpLoggable for ServiceError {
    fn op_display_chain(&self) -> String {
        self.0.display_chain()
    }

    fn op_attach(self, guard: &AutoLogGuard) -> Self {
        Self(self.0.with_context(guard))
    }
}

impl OpLoggable for UpgradeError {
    fn op_display_chain(&self) -> String {
        self.0.display_chain()
    }

    fn op_attach(self, guard: &AutoLogGuard) -> Self {
        Self(self.0.with_context(guard))
    }
}

/// 叶子错误（无 orion 因果链可走）：日志取 `Display`，也不附 context。
///
/// 覆盖事件日志里会出现的普通错误（`io::Error` / `JoinError`）；对它们 `display_chain()`
/// 与 `Display` 等价。
impl OpLoggable for std::io::Error {
    fn op_display_chain(&self) -> String {
        self.to_string()
    }

    fn op_attach(self, _guard: &AutoLogGuard) -> Self {
        self
    }
}

impl OpLoggable for tokio::task::JoinError {
    fn op_display_chain(&self) -> String {
        self.to_string()
    }

    fn op_attach(self, _guard: &AutoLogGuard) -> Self {
        self
    }
}

/// 把一次错误的**完整因果链**折成单行，供事件日志（`log::*`）。
///
/// 直接用 `{err}`（`Display`）会丢掉 `orion-error` 的 `Caused by` 帧（原始 io / HTTP 错误），
/// 事件日志就看不出根因；本函数把 `display_chain()` 折成单行以保持「一行一事件」便于 grep。
pub fn chain_one_line<E: OpLoggable>(err: &E) -> String {
    err.op_display_chain()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lift local state failures (record / heartbeat writes) into the upgrade boundary.
impl From<StateError> for UpgradeError {
    fn from(err: StateError) -> Self {
        UpgradeReason::Io.err(err.op_display_chain())
    }
}

/// Lift the center client's typed error into the daemon boundary, preserving the
/// 401/403 "credential rejected" distinction as a stable reason identity.
impl From<CenterError> for GwlinkdError {
    fn from(err: CenterError) -> Self {
        let reason = if err.is_unauthorized() {
            GwlinkdReason::CenterUnauthorized
        } else {
            GwlinkdReason::Center
        };
        reason.err(err.op_display_chain())
    }
}

/// Lift local state failures into the daemon boundary.
impl From<StateError> for GwlinkdError {
    fn from(err: StateError) -> Self {
        GwlinkdReason::State.err(err.op_display_chain())
    }
}

/// Lift service-manager failures into the daemon boundary.
impl From<ServiceError> for GwlinkdError {
    fn from(err: ServiceError) -> Self {
        GwlinkdReason::Service.err(err.op_display_chain())
    }
}

/// Lift upgrade failures into the daemon boundary.
impl From<UpgradeError> for GwlinkdError {
    fn from(err: UpgradeError) -> Self {
        GwlinkdReason::Upgrade.err(err.op_display_chain())
    }
}

// Lift a config-domain error into the daemon boundary, preserving detail /
// context / source chains while only remapping the reason namespace.
//
// Note: the blanket `From<StructError<R1>> for StructError<R2>` is blocked by
// the orphan rule, so callers convert a `ConfigResult<T>` into a
// `GwlinkdResult<T>` with `ConvErr::conv_err` (which uses
// `From<ConfigReason> for GwlinkdReason` above).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_op_attaches_context_to_the_failure() {
        // 失败：`logged_op` 经 guard 的 Drop 记一条 `fail!`（带 cause），并把上下文
        // 作为纯数据附到错误上 —— 边界打印的 `display_chain()` / Display 因此能看到
        // action + 字段，且日志不重复、不延迟。
        let result: CenterResult<()> = Err(CenterReason::Http.err("boom"));
        let err = logged_op(
            module_path!(),
            "center renew credential",
            &[("gateway_id", "gw-1".to_string())],
            result,
        )
        .expect_err("must fail");
        let rendered = err.to_string();
        assert!(rendered.contains("center renew credential"), "{rendered}");
        assert!(rendered.contains("gw-1"), "{rendered}");
        // 原始原因仍在链上。
        assert!(err.op_display_chain().contains("boom"), "{err}");
    }

    #[test]
    fn logged_op_passes_success_through() {
        let result: CenterResult<u8> = Ok(7);
        let value = logged_op(module_path!(), "center noop", &[], result).expect("ok");
        assert_eq!(value, 7);
    }

    /// 跨域 lift（`From<CenterError> for GwlinkdError`）必须**保留** source chain。
    ///
    /// 曾经用 `err.to_string()`（`Display`，不含 `Caused by`），把原始 io / HTTP 错丢掉；
    /// 于是连边界 `display_chain()` 也看不到根因。现走 `op_display_chain()`。
    #[test]
    fn lifting_a_domain_error_preserves_the_source_chain() {
        let center: CenterError = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))
        .source_raw_err(CenterReason::Http, "link-upstream 请求失败")
        .map_err(CenterError::from)
        .expect_err("must fail");

        let lifted: GwlinkdError = center.into();
        let chain = lifted.display_chain();
        assert!(chain.contains("connection refused"), "{chain}");
        assert!(chain.contains("link-upstream 请求失败"), "{chain}");
    }

    /// 事件日志入口：`chain_one_line` 把链折成单行（供 `log::*`），且**不丢**根因。
    #[test]
    fn chain_one_line_keeps_the_root_cause_on_one_line() {
        let err: CenterError = Err::<(), _>(std::io::Error::other("no route to host"))
            .source_raw_err(CenterReason::Http, "status 请求失败")
            .map_err(CenterError::from)
            .expect_err("must fail");
        let line = chain_one_line(&err);
        assert!(!line.contains('\n'), "must be single-line: {line}");
        assert!(line.contains("status 请求失败"), "{line}");
        assert!(line.contains("no route to host"), "{line}");
    }
}
