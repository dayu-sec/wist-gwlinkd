//! `[wparse_relay]`：规则发布命令中继。
//!
//! 链路：Center 命令队列 --mTLS 短轮询--> gwlinkd --HTTPS(本机 admin token)--> wparse。
//! Gateway 不开放入站端口；Center 也不能直连 wparse（管理面只发布在宿主回环、
//! 且证书由 Gateway CA 签发）。
//!
//! 双端白名单：Center 只接受两个 admin 路径；gwlinkd 也独立校验，不把 Center 当可信来源。

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::MissedTickBehavior;
use wist_control::DesiredBusinessIdentity;

use crate::center::{CenterClient, WparseCommand, build_mtls_http_client};
use crate::config::Config;
use crate::state;
use crate::sys_root::control_workdir;
use crate::task_log::{self, TaskLog};
use crate::wparse_env::{is_tag_safe, value_preview};

const STATUS_PATH: &str = "/admin/v1/runtime/status";
const RELOAD_PATH: &str = "/admin/v1/reloads/model";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// 请求体在日志里只留这么多字节的头部预览。
///
/// 发布命令的 body 是整套模型（可能几百 KB）：整段写盘会（1）把 `output.log` 的 4 MiB
/// 预算吃光、（2）每写一次就跑一遍全量脱敏扫描。原文在中心 `gateway_commands.body` 里可取。
const REQUEST_PREVIEW_BYTES: usize = 8 * 1024;

struct UpstreamResult {
    status_code: u16,
    body: String,
    error: Option<String>,
}

/// 常驻：短轮询 Center 命令队列并在本机执行。
///
/// `desired`：状态上报回执带来的**中心权威期望业务身份**。补偿在这里做而不是在状态循环里
/// —— 本任务才是 `.env` / 身份文件的写入者（见 [`apply_business_identity`]）。
pub async fn run(
    config: Config,
    desired: tokio::sync::watch::Receiver<Option<DesiredBusinessIdentity>>,
) {
    let relay = config.wparse_relay.clone();
    if !relay.enabled {
        return;
    }
    if relay.upstream.trim().is_empty() {
        log::warn!("event=WparseRelayDisabled reason=empty_upstream");
        return;
    }
    let Some(token_file) = relay.token_file.clone() else {
        log::warn!("event=WparseRelayDisabled reason=missing_token_file");
        return;
    };

    let poll = Duration::from_millis(relay.poll_interval_ms.unwrap_or(1_500).clamp(500, 30_000));
    let timeout = Duration::from_millis(
        relay
            .request_timeout_ms
            .unwrap_or(20_000)
            .clamp(1_000, 120_000),
    );
    let upstream = relay.upstream.trim_end_matches('/').to_string();
    let allowed = allowed_paths(&relay.allowed_paths);

    let http = match build_upstream_client(relay.ca_file.as_deref(), timeout) {
        Ok(client) => client,
        Err(err) => {
            log::error!("event=WparseRelayDisabled reason=upstream_client error={err}");
            return;
        }
    };

    let trust_bundle = config.trust_bundle.clone();
    let trust = trust_bundle.exists().then_some(trust_bundle.as_path());
    let mut cached_identity: Option<String> = None;
    let mut center: Option<CenterClient> = None;
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut backoff = Duration::ZERO;
    let mut credential_missing_logged = false;
    // 同一期望值失败后不每轮重试（生效命令可能要重启整栈，见 [`COMPENSATION_RETRY`]）。
    let mut last_compensation: Option<(Instant, String, i64)> = None;
    log::info!(
        "event=WparseRelayStarted upstream={} poll_ms={}",
        upstream,
        poll.as_millis()
    );

    loop {
        ticker.tick().await;

        // 期望值补偿：命令队列丢失 / 离线 / 重启后靠状态回执收敛（设计 §11.1）。
        let wanted = desired.borrow().as_ref().cloned();
        compensate_desired_identity(&config, wanted, &mut last_compensation).await;

        // 证书轮换后 `identity_pem` 会变：只在变化时重建 mTLS 客户端。
        let Some(credential) = state::load_credential(config.identity_dir()) else {
            if !credential_missing_logged {
                log::warn!("event=WparseRelayNoCredential");
                credential_missing_logged = true;
            }
            continue;
        };
        credential_missing_logged = false;
        let identity_pem = credential.identity_pem();
        if cached_identity.as_deref() != Some(identity_pem.as_str()) {
            match build_mtls_http_client(trust, &identity_pem) {
                Ok(client) => {
                    center = Some(CenterClient::with_client(
                        config.control_center_endpoint.clone(),
                        client,
                    ));
                    cached_identity = Some(identity_pem);
                }
                Err(err) => {
                    log::warn!("event=WparseRelayCenterClientFailed error={}", err);
                    continue;
                }
            }
        }
        let Some(client) = center.as_ref() else {
            continue;
        };

        let command = match client.poll_wparse_command(&config.gateway_id).await {
            Ok(Some(command)) => {
                backoff = Duration::ZERO;
                command
            }
            Ok(None) => {
                backoff = Duration::ZERO;
                continue;
            }
            Err(err) => {
                backoff = if backoff.is_zero() {
                    poll
                } else {
                    (backoff * 2).min(Duration::from_secs(30))
                };
                log::warn!(
                    "event=WparseRelayPollFailed backoff_ms={} error={}",
                    backoff.as_millis(),
                    err
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
        };

        // 只给**变更**类命令落任务目录：`GET /runtime/status` 是 WarpStation 的高频轮询
        // （几秒一次），全落盘会把真正的发布记录挤出保留窗口（20 个）。
        // 状态查询失败仍会在下面 journald 的 `WparseCommandFailed` 里报出来。
        let task = if should_record(&command) {
            task_log::TaskLog::create(
                &config.state_dir,
                "wparse",
                &command.command_id,
                task_meta(&config.gateway_id, &upstream, &command),
            )
        } else {
            None
        };
        if let Some(task) = &task {
            task.event(
                "run.start",
                Some(&format!("{} {}", command.method, command.path)),
            );
        }

        let result = if command.is_control() {
            execute_control(&config, &command, task.as_deref()).await
        } else {
            execute(
                &http,
                &token_file,
                &upstream,
                &allowed,
                &command,
                task.as_deref(),
            )
            .await
        };
        let (ok, status, mut error) = outcome(&result);
        // 收尾在回执**之前**：失败时要把日志目录带回中心（升级链路同口径）。
        let log_dir = task
            .as_ref()
            .map(|task| task.finish(status, error.as_deref().unwrap_or("")));
        if ok {
            log::info!(
                "event=WparseCommandDone command_id={} kind={} method={} path={} upstream_status={} log_dir={}",
                command.command_id,
                command.kind,
                command.method,
                command.path,
                result.status_code,
                log_dir.as_deref().unwrap_or("-")
            );
        } else {
            log::warn!(
                "event=WparseCommandFailed command_id={} kind={} method={} path={} upstream_status={} log_dir={} error={}",
                command.command_id,
                command.kind,
                command.method,
                command.path,
                result.status_code,
                log_dir.as_deref().unwrap_or("-"),
                error.as_deref().unwrap_or("-")
            );
        }
        error = with_log_dir(error, log_dir.as_deref());
        // 失败时把日志摘要一并回传：中心落库后可直接展示（目录仍留在本机）。
        let log_excerpt = if ok {
            None
        } else {
            task.as_ref()
                .and_then(|task| task.excerpt(task_log::EXCERPT_LIMIT))
        };
        if let Err(err) = client
            .report_wparse_command_result(
                &command.command_id,
                result.status_code,
                &result.body,
                error.as_deref(),
                task.as_ref().map(|task| task.task_id()),
                log_dir.as_deref(),
                log_excerpt.as_deref(),
            )
            .await
        {
            log::warn!(
                "event=WparseRelayResultFailed command_id={} error={}",
                command.command_id,
                err
            );
            // 命令跑完了但回执没到中心：中心那条会停在 leased/expired —— 日志里要留下原因。
            if let Some(task) = &task {
                task.event("report.failed", Some(&err.to_string()));
            }
        }
    }
}

/// `task.json` 的内容：命令身份 + 发布体里的模型标识。
fn task_meta(gateway_id: &str, upstream: &str, command: &WparseCommand) -> serde_json::Value {
    json!({
        "gateway_id": gateway_id,
        "kind": &command.kind,
        "method": &command.method,
        "path": &command.path,
        "upstream": upstream,
        "lease_expires_at": &command.lease_expires_at,
        "request_bytes": command.body.as_ref().map(|body| body.to_string().len()),
        // 排障最常问的就是“当时发的是哪个模型版本”：直接摘出来，不用读 body 预览。
        "model_version": body_field(&command.body, "version"),
        "model_group": body_field(&command.body, "group"),
        "reason": body_field(&command.body, "reason"),
        // `bind-tenant`：绑定内容（tenant / revision）比模型字段更值得直接看到。
        "tenant_id": body_field(&command.body, "tenant_id"),
        "binding_revision": command
            .body
            .as_ref()
            .and_then(|body| body.get("revision"))
            .and_then(|value| value.as_i64()),
    })
}

/// 是否给这条命令落任务目录（只有状态轮询不落；控制类命令一律落）。
fn should_record(command: &WparseCommand) -> bool {
    command.is_control() || !command.method.eq_ignore_ascii_case("GET")
}

/// 从发布体里摘一个字符串字段（缺失 / 非字符串 → `null`）。
fn body_field(body: &Option<serde_json::Value>, key: &str) -> Option<String> {
    body.as_ref()?.get(key)?.as_str().map(str::to_string)
}

/// 由执行结果推出「是否成功 + 终态标签 + 回执用的 error」（**不含**日志目录）。
///
/// 中心只按 `error` 是否为 Some 判 done / failed，**不看状态码**：wparse 回 422 时若不补一句
/// error，这条命令会在中心库里记成“成功”（WarpStation 侧却看到 422，两边对不上）。
fn outcome(result: &UpstreamResult) -> (bool, &'static str, Option<String>) {
    let rejected = result.status_code >= 400;
    let ok = result.error.is_none() && !rejected;
    let error = result
        .error
        .clone()
        .or_else(|| rejected.then(|| format!("wparse 上游返回 {}", result.status_code)));
    (ok, if ok { "ok" } else { "failed" }, error)
}

/// 把日志目录拼进回执错误。
///
/// 只能拼在 `error` 上：成功的 `body` 是给 WarpStation 的机器载荷（拼了就破 JSON），
/// 而成功时 `error` 必须保持 `None`（否则中心会把命令判成 failed）。
fn with_log_dir(error: Option<String>, log_dir: Option<&str>) -> Option<String> {
    match (error, log_dir) {
        (Some(mut err), Some(dir)) => {
            err.push_str(&format!("；日志: {dir}"));
            Some(err)
        }
        (error, _) => error,
    }
}

fn allowed_paths(configured: &[String]) -> Vec<&'static str> {
    let builtin = [STATUS_PATH, RELOAD_PATH];
    if configured.is_empty() {
        return builtin.to_vec();
    }
    // 配置只能**收窄**：只有显式列出的内置路径才放行。
    builtin
        .into_iter()
        .filter(|path| configured.iter().any(|item| item == path))
        .collect()
}

async fn execute(
    http: &reqwest::Client,
    token_file: &Path,
    upstream: &str,
    allowed: &[&str],
    command: &WparseCommand,
    task: Option<&TaskLog>,
) -> UpstreamResult {
    let path = command.path.as_str();
    if !is_allowed(&command.method, path, allowed) {
        let error = format!("path not allowed: {} {}", command.method, path);
        record_event(task, "reject", &error);
        return UpstreamResult {
            status_code: 403,
            body: String::new(),
            error: Some(error),
        };
    }

    let token = match std::fs::read_to_string(token_file) {
        Ok(token) => token.trim().to_string(),
        Err(err) => {
            let error = format!("read admin token {}: {err}", token_file.display());
            record_event(task, "config.error", &error);
            return UpstreamResult {
                status_code: 502,
                body: String::new(),
                error: Some(error),
            };
        }
    };
    if token.is_empty() {
        record_event(task, "config.error", "admin token is empty");
        return UpstreamResult {
            status_code: 502,
            body: String::new(),
            error: Some("admin token is empty".to_string()),
        };
    }

    let url = format!("{upstream}{path}");
    let request = match command.method.as_str() {
        "GET" => http.get(url),
        "POST" => http.post(url),
        _ => unreachable!("method checked above"),
    };
    let request = request
        .bearer_auth(token)
        .header("accept", "application/json")
        // 把 Center 的 command_id 作为 wparse 的 request_id 贯穿：
        // WarpStation 发布后会按它核对 `last_reload_request_id`。
        .header("X-Request-Id", &command.command_id);
    let body_text = command.body.as_ref().map(|body| body.to_string());
    let request = match (&command.method[..], &command.body) {
        ("POST", Some(body)) => request.json(body),
        _ => request,
    };
    // 请求原文进日志：发布出了问题时，要能回答“当时到底发了什么”。
    record_request(task, command, body_text.as_deref());

    let started = Instant::now();
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let body = truncate(text);
            record_response(task, status, started.elapsed(), &body);
            UpstreamResult {
                status_code: status,
                body,
                error: None,
            }
        }
        Err(err) => {
            let error = format!("wparse admin request failed: {err}");
            if let Some(task) = task {
                task.output(&format!(
                    "< transport error ({} ms)\n",
                    started.elapsed().as_millis()
                ));
                task.event("upstream.failed", Some(&error));
            }
            UpstreamResult {
                status_code: 502,
                body: String::new(),
                error: Some(error),
            }
        }
    }
}

/// 控制类命令（`kind != proxy`）：**在本机执行**，不经过 wparse 管理面。
///
/// 目前有 `diagnose` / `restart-gateway` / `restart-gwlinkd` / `bind-tenant`。未知 kind 一律**明确失败**
/// （而不是静默当成功）—— 中心派了新命令但本机 gwlinkd 太旧时，要让人能从回执里看出来。
///
/// **指令规则**：「是否让数据面生效」是**指令的语义**，不是配置项：
/// - `bind-tenant` 的语义就是「让这个租户生效」⇒ 一定写 `.env` 并跑生效动作；
///   `wparse_apply_command` 只能决定**怎么做**（重启方式），关不掉它。
/// - 其它指令不碰数据面重载：`proxy` 类（如 `reloads/model`）由 wparse 自己热加载，
///   `restart-*` 只管进程生命周期。
async fn execute_control(
    config: &Config,
    command: &WparseCommand,
    task: Option<&TaskLog>,
) -> UpstreamResult {
    match command.kind.as_str() {
        "diagnose" => run_diagnose(config, command, task).await,
        "restart-gateway" => run_gateway_restart(config, task).await,
        "restart-gwlinkd" => run_gwlinkd_restart(task).await,
        "bind-tenant" => run_bind_tenant(config, command, task).await,
        other => {
            let error = format!("不支持的控制命令 kind={other}（先升级 gwlinkd 再重派）");
            record_event(task, "reject", &error);
            UpstreamResult {
                status_code: 400,
                body: String::new(),
                error: Some(error),
            }
        }
    }
}

/// `bind-tenant` 载荷：Center 绑定表里的 `customer_id` 在数据面叫 `tenant_id`。
#[derive(serde::Deserialize)]
struct BindTenantPayload {
    tenant_id: String,
    #[serde(default = "default_bind_revision")]
    revision: i64,
}

fn default_bind_revision() -> i64 {
    1
}

/// 绑定业务身份：校验 → 落身份 → 写 `.env` → 重启 wparse（见 [`apply_business_identity`]）。
///
/// 幂等三态：
/// - 更旧的 revision → 200 `ignored: stale`（不覆盖本地）；
/// - 同 revision + 同 tenant + 已生效 → 200 直接返回（**不重复重启**，租约重投时常见）；
/// - 否则应用。
async fn run_bind_tenant(
    config: &Config,
    command: &WparseCommand,
    task: Option<&TaskLog>,
) -> UpstreamResult {
    let Some(body) = command.body.as_ref() else {
        let error = "bind-tenant 缺少 body".to_string();
        record_event(task, "bind.rejected", &error);
        return UpstreamResult {
            status_code: 400,
            body: String::new(),
            error: Some(error),
        };
    };
    let payload: BindTenantPayload = match serde_json::from_value(body.clone()) {
        Ok(payload) => payload,
        Err(err) => {
            let error = format!("bind-tenant body 解析失败：{err}");
            record_event(task, "bind.rejected", &error);
            return UpstreamResult {
                status_code: 400,
                body: String::new(),
                error: Some(error),
            };
        }
    };
    let tenant_id = payload.tenant_id.trim().to_string();
    // 先校验格式：旧 revision 的非法值也要明确拒绝，不能被 stale 分支「成功」掉。
    if !is_tag_safe(&tenant_id) {
        let error = format!(
            "tenant_id 不是 tag-safe（1..=64，仅 [A-Za-z0-9_.:/=@+,-]）：{}",
            value_preview(&tenant_id)
        );
        record_event(task, "bind.rejected", &error);
        return UpstreamResult {
            status_code: 400,
            body: json!({
                "tenant_id": tenant_id,
                "revision": payload.revision,
                "wparse_applied": false,
            })
            .to_string(),
            error: Some(error),
        };
    }
    if let Some(current) = state::load_business_identity(config.identity_dir()) {
        if payload.revision < current.revision {
            let detail = format!(
                "忽略旧命令：payload revision={} < local revision={}",
                payload.revision, current.revision
            );
            record_event(task, "bind.stale", &detail);
            return UpstreamResult {
                status_code: 200,
                body: json!({
                    "requested": { "tenant_id": tenant_id, "revision": payload.revision },
                    "tenant_id": current.tenant_id,
                    "revision": current.revision,
                    "ignored": "stale",
                })
                .to_string(),
                error: None,
            };
        }
        // 同版本同身份且已生效：幂等返回，不重跑生效命令（控制命令租约过期会重投）。
        if payload.revision == current.revision
            && current.tenant_id == tenant_id
            && current.wparse_applied
        {
            record_event(
                task,
                "bind.unchanged",
                &format!("revision={}", current.revision),
            );
            return UpstreamResult {
                status_code: 200,
                body: json!({
                    "tenant_id": tenant_id,
                    "revision": payload.revision,
                    "wparse_applied": true,
                    "apply": "unchanged",
                })
                .to_string(),
                error: None,
            };
        }
    }
    match apply_business_identity(config, &tenant_id, payload.revision, task).await {
        ApplyOutcome::Applied => UpstreamResult {
            status_code: 200,
            body: json!({
                "tenant_id": tenant_id,
                "revision": payload.revision,
                "wparse_applied": true,
            })
            .to_string(),
            error: None,
        },
        ApplyOutcome::NotApplied { reason } => {
            // `.env` 已写、身份已落盘，但这次没生效（`reason` 说明为什么）。
            // 必须把这层意思带给中心：否则回执看着是成功，页面只会停在“已下发”，
            // 而现场已经出现过“绑了但数据一直带 unbound”（2026-10-25）。
            let mut body = json!({
                "tenant_id": tenant_id,
                "revision": payload.revision,
                "wparse_applied": false,
                "apply": "not_configured",
            });
            body["reason"] = json!(reason);
            record_event(task, "bind.not_applied", &reason);
            UpstreamResult {
                status_code: 200,
                body: body.to_string(),
                error: None,
            }
        }
        ApplyOutcome::Failed { status, error } => UpstreamResult {
            status_code: status,
            body: json!({
                "tenant_id": tenant_id,
                "revision": payload.revision,
                "wparse_applied": false,
            })
            .to_string(),
            error: Some(error),
        },
    }
}

/// 应用业务身份的结果：**命令回执**与**状态回执补偿**共用同一条路径。
enum ApplyOutcome {
    /// `.env` 已写 + 身份已落盘 + 生效命令跑成功。
    Applied,
    /// `.env` 已写 + 身份已落盘，但这次没生效（附原因，回执要带给中心）。
    NotApplied { reason: String },
    /// 失败；`status` 用作命令回执的 HTTP 状态码。
    Failed { status: u16, error: String },
}

/// 落 `business-identity.json` → 写 `<工作根>/.env` → 跑生效动作（显式配置或推出来的缺省）。
///
/// 顺序有意为**身份先、`.env` 后**（与设计 §8.2 一致）：`.env` 写失败时本地仍是
/// 「未生效」，不会出现「数据面已是新租户、身份文件还是旧值」的错位（那会被中心判成漂移，
/// 且因 `wparse_applied=true` 永不重试）。
///
/// **只由本模块的 relay 任务调用**（命令处理与状态回执补偿在同一条循环里）：
/// `.env` / 身份文件不会有并发写者。
async fn apply_business_identity(
    config: &Config,
    tenant_id: &str,
    revision: i64,
    task: Option<&TaskLog>,
) -> ApplyOutcome {
    let tenant_id = tenant_id.trim();
    if !is_tag_safe(tenant_id) {
        let error = format!(
            "tenant_id 不是 tag-safe（1..=64，仅 [A-Za-z0-9_.:/=@+,-]）：{}",
            value_preview(tenant_id)
        );
        record_event(task, "bind.rejected", &error);
        return ApplyOutcome::Failed { status: 400, error };
    }
    // 先把**工作根**解析出来：配置类错误（未配工程根 / 多系统歧义 / 工作根不存在）
    // 必须在动盘之前失败，否则会「已落盘但这个错永远重试」。
    let work_dir = match crate::wparse_env::resolve_work_dir(config) {
        Ok(dir) => dir,
        Err(error) => {
            record_event(task, "bind.rejected", &error);
            return ApplyOutcome::Failed {
                status: 500,
                error: format!("定位 wparse 工作根失败：{error}"),
            };
        }
    };
    // 生效动作：没配也能推（缺省见 `wparse_env::resolve_apply_action`）。
    // 推不出来**不是**配置错误：`.env` 照写（下次启动 / 补配置后就生效），
    // 这次只回执「没生效 + 原因」。
    let apply_action = crate::wparse_env::resolve_apply_action(config, &work_dir);
    let mut identity = state::BusinessIdentity {
        tenant_id: tenant_id.to_string(),
        revision,
        wparse_applied: false,
    };
    if let Err(err) = state::save_business_identity(config.identity_dir(), &identity) {
        let error = format!("保存业务身份失败：{err}");
        record_event(task, "bind.failed", &error);
        return ApplyOutcome::Failed { status: 500, error };
    }
    if let Err(error) = crate::wparse_env::write_tenant_id(&work_dir, tenant_id) {
        record_event(task, "bind.env_failed", &error);
        return ApplyOutcome::Failed {
            status: 500,
            error: format!("写 wparse .env 失败：{error}"),
        };
    }
    record_event(
        task,
        "bind.stored",
        &format!(
            "tenant_id={tenant_id} revision={revision} env={}",
            work_dir.join(crate::wparse_env::ENV_FILE).display()
        ),
    );
    // 补偿路径没有 task 日志：这条 info 是唯一的「写到了哪个 .env」证据。
    log::info!(
        "event=TenantBindingStored tenant_id={tenant_id} revision={revision} env={}",
        work_dir.join(crate::wparse_env::ENV_FILE).display()
    );
    let action = match apply_action {
        Ok(action) => action,
        Err(reason) => {
            record_event(task, "bind.not_applied", &reason);
            return ApplyOutcome::NotApplied { reason };
        }
    };
    if action.defaulted {
        record_event(
            task,
            "bind.apply_default",
            &format!("{}（cwd={}）", action.command, action.dir.display()),
        );
    }
    record_event(
        task,
        "bind.apply",
        &format!("{}（cwd={}）", action.command, action.dir.display()),
    );
    match run_shell(
        &action.command,
        &action.dir,
        crate::wparse_env::apply_timeout(config),
    )
    .await
    {
        Ok(output) if output.status.success() => {
            identity.wparse_applied = true;
            if let Err(err) = state::save_business_identity(config.identity_dir(), &identity) {
                log::warn!("event=TenantBindingFlagPersistFailed error={err}");
            }
            record_event(task, "bind.applied", &format!("tenant_id={tenant_id}"));
            ApplyOutcome::Applied
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error = format!(
                "生效命令失败（exit={:?}）：{}",
                output.status.code(),
                stderr_tail(&stderr, 10)
            );
            record_event(task, "bind.apply_failed", &error);
            ApplyOutcome::Failed { status: 500, error }
        }
        Err(error) => {
            record_event(task, "bind.apply_failed", &error);
            ApplyOutcome::Failed { status: 500, error }
        }
    }
}

/// 同一期望值失败后的重试间隔：生效命令可能要重启整栈，不能每轮（1.5s）都试。
const COMPENSATION_RETRY: Duration = Duration::from_secs(60);

/// 本地业务身份是否落后于中心期望值。
fn needs_business_apply(
    local: Option<&state::BusinessIdentity>,
    desired: &DesiredBusinessIdentity,
    can_apply: bool,
) -> bool {
    let Some(local) = local else {
        return true;
    };
    // 回执最多 30s 才刷新一次，**可能比刚执行的命令旧**：更旧的期望值绝不能覆盖本地，
    // 否则一次换绑会被「旧回执 → 回退 → 新回执 → 再改回」来回折腾（每次都是整栈重启）。
    if desired.revision < local.revision {
        return false;
    }
    if local.tenant_id != desired.tenant_id || local.revision < desired.revision {
        return true;
    }
    // 同版本但上次没生效（`.env` 已写、重启没跑成）：配置了生效命令就再试。
    can_apply && !local.wparse_applied
}

/// 状态回执带的**中心权威期望值** → 本地落后就自行补齐（设计 §11.1）。
///
/// 不依赖命令队列：离线 / 队列过期 / gwlinkd 重启都天然收敛；同一期望值失败后
/// 至少隔 [`COMPENSATION_RETRY`] 才重试。
async fn compensate_desired_identity(
    config: &Config,
    desired: Option<DesiredBusinessIdentity>,
    last_attempt: &mut Option<(Instant, String, i64)>,
) {
    let Some(desired) = desired else {
        return;
    };
    // 「能不能生效」看的是**推出来的动作**，不是配置里有没有写
    // —— 缺省生效命令也算（否则没写配置的现场永远不重试）。
    let can_apply = crate::wparse_env::resolve_work_dir(config)
        .map(|work_dir| crate::wparse_env::resolve_apply_action(config, &work_dir).is_ok())
        .unwrap_or(false);
    let local = state::load_business_identity(config.identity_dir());
    if !needs_business_apply(local.as_ref(), &desired, can_apply) {
        return;
    }
    if let Some((at, tenant_id, revision)) = last_attempt
        && *tenant_id == desired.tenant_id
        && *revision == desired.revision
        && at.elapsed() < COMPENSATION_RETRY
    {
        return;
    }
    *last_attempt = Some((Instant::now(), desired.tenant_id.clone(), desired.revision));
    log::info!(
        "event=TenantBindingCompensating tenant_id={} revision={} local_revision={:?}",
        desired.tenant_id,
        desired.revision,
        local.as_ref().map(|identity| identity.revision)
    );
    match apply_business_identity(config, &desired.tenant_id, desired.revision, None).await {
        ApplyOutcome::Applied => log::info!(
            "event=TenantBindingCompensated tenant_id={} revision={}",
            desired.tenant_id,
            desired.revision
        ),
        ApplyOutcome::NotApplied { reason } => log::info!(
            "event=TenantBindingCompensatePending tenant_id={} revision={} reason={reason}",
            desired.tenant_id,
            desired.revision
        ),
        ApplyOutcome::Failed { error, .. } => log::warn!(
            "event=TenantBindingCompensateFailed tenant_id={} revision={} error={error}",
            desired.tenant_id,
            desired.revision
        ),
    }
}

/// 启动自检 + 补偿：让**身份文件**与 `<工作根>/.env` 一致，并按需重跑生效命令。
///
/// 不能只看 `wparse_applied`：`business-identity.json` 随 `contract` 备份、`data-plane-run/`
/// **不进备份**（设计 §8.1/§9.2）—— 换机/恢复后身份回来了、`.env` 没了，只看标志会直接 return，
/// 造成「中心显示已生效、数据面永远是 `unbound`」且无人修复。人工误删/改 `.env` 同理。
pub async fn reconcile_business_identity(config: &Config) {
    let Some(mut identity) = state::load_business_identity(config.identity_dir()) else {
        return;
    };
    let work_dir = match crate::wparse_env::resolve_work_dir(config) {
        Ok(dir) => dir,
        Err(error) => {
            log::warn!("event=TenantBindingReconcileSkipped error={error}");
            return;
        }
    };
    let env_matches = std::fs::read_to_string(work_dir.join(crate::wparse_env::ENV_FILE))
        .map(|text| {
            crate::wparse_env::env_has_value(
                &text,
                crate::wparse_env::TENANT_KEY,
                &identity.tenant_id,
            )
        })
        .unwrap_or(false);
    if identity.wparse_applied && env_matches {
        return;
    }
    if !env_matches {
        log::warn!(
            "event=TenantBindingEnvDrift tenant_id={} revision={} env={}（现场值缺失/不一致，重写并重新生效）",
            identity.tenant_id,
            identity.revision,
            work_dir.join(crate::wparse_env::ENV_FILE).display()
        );
        // 先降级标志再动盘：中途崩溃时下次启动仍会重试。
        identity.wparse_applied = false;
        if let Err(err) = state::save_business_identity(config.identity_dir(), &identity) {
            log::warn!("event=TenantBindingFlagPersistFailed error={err}");
        }
    }
    if let Err(error) = crate::wparse_env::write_tenant_id(&work_dir, &identity.tenant_id) {
        log::warn!("event=TenantBindingReconcileEnvFailed error={error}");
        return;
    }
    let action = match crate::wparse_env::resolve_apply_action(config, &work_dir) {
        Ok(action) => action,
        Err(reason) => {
            log::info!(
                "event=TenantBindingReconcilePending tenant_id={} revision={} reason={reason}",
                identity.tenant_id,
                identity.revision
            );
            return;
        }
    };
    if action.defaulted {
        log::info!(
            "event=TenantBindingApplyDefault command={} cwd={}",
            action.command,
            action.dir.display()
        );
    }
    match run_shell(
        &action.command,
        &action.dir,
        crate::wparse_env::apply_timeout(config),
    )
    .await
    {
        Ok(output) if output.status.success() => {
            identity.wparse_applied = true;
            if let Err(err) = state::save_business_identity(config.identity_dir(), &identity) {
                log::warn!("event=TenantBindingFlagPersistFailed error={err}");
            }
            log::info!(
                "event=TenantBindingReconciled tenant_id={} revision={}",
                identity.tenant_id,
                identity.revision
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "event=TenantBindingReconcileFailed exit={:?} stderr={}",
                output.status.code(),
                stderr_tail(&stderr, 10)
            );
        }
        Err(error) => log::warn!("event=TenantBindingReconcileFailed error={error}"),
    }
}

/// 体检：在**系统根**（见 [`control_workdir`]）跑 `gops run diagnose --json`。
///
/// 回执语义：
/// - **exit 0 / 1 都算“执行成功”**：体检发现问题时 gops 本就退 1，报告里自带
///   `summary.exit_code` —— 不能把“有问题”误报成“命令没跑成”。
/// - exit 2 / 超时 / 输出不是 JSON → `error`，中心会把命令判 failed。
async fn run_diagnose(
    config: &Config,
    command: &WparseCommand,
    task: Option<&TaskLog>,
) -> UpstreamResult {
    let dir = match control_workdir(config) {
        Ok(dir) => dir,
        Err(error) => {
            record_event(task, "config.error", &format!("体检：{error}"));
            return UpstreamResult {
                status_code: 500,
                body: String::new(),
                error: Some(format!("体检：{error}")),
            };
        }
    };
    let program = config
        .diagnose_program
        .clone()
        .or_else(|| config.upgrader_program.clone())
        .unwrap_or_else(|| "gops".to_string());
    let timeout = Duration::from_secs(
        config
            .diagnose_timeout_seconds
            .unwrap_or(120)
            // 上限 300s：中心的 `diagnose` 租约是 360s（`DIAGNOSE_LEASE_SECONDS`），
            // 超时必须短于租约，否则会被判过期重派，同一台机器上跑两份。
            .clamp(1, 300),
    );

    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(["run", "diagnose", "--json", "--no-color"])
        .current_dir(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    record_event(
        task,
        "diagnose.start",
        &format!(
            "{program} run diagnose --json（cwd={}，timeout={}s，command_id={}）",
            dir.display(),
            timeout.as_secs(),
            command.command_id
        ),
    );

    let started = Instant::now();
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            let error = format!("无法执行 {program}：{err}");
            record_event(task, "diagnose.failed", &error);
            return UpstreamResult {
                status_code: 500,
                body: String::new(),
                error: Some(error),
            };
        }
        Err(_) => {
            let error = format!("体检超时（{}s）已被终止", timeout.as_secs());
            record_event(task, "diagnose.timeout", &error);
            return UpstreamResult {
                status_code: 504,
                body: String::new(),
                error: Some(error),
            };
        }
    };

    let code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if let Some(task) = task {
        task.output(&format!(
            "< {program} exit={code:?} ({} ms)\n",
            started.elapsed().as_millis()
        ));
        if !stderr.is_empty() {
            task.output(&format!("{stderr}\n"));
        }
    }

    match code {
        Some(0) | Some(1) => {
            if serde_json::from_str::<serde_json::Value>(&stdout).is_err() {
                let error = format!(
                    "体检输出不是合法 JSON（前 200 字节）：{}",
                    truncate(stdout.clone())
                        .chars()
                        .take(200)
                        .collect::<String>()
                );
                record_event(task, "diagnose.failed", &error);
                return UpstreamResult {
                    status_code: 500,
                    body: stdout,
                    error: Some(error),
                };
            }
            record_event(task, "diagnose.done", &format!("exit={code:?}"));
            UpstreamResult {
                status_code: 200,
                body: stdout,
                error: None,
            }
        }
        other => {
            let tail: String = stderr
                .lines()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            let error = format!("gops run diagnose 失败（exit={other:?}）：{tail}");
            record_event(task, "diagnose.failed", &error);
            UpstreamResult {
                status_code: 500,
                body: stdout,
                error: Some(error),
            }
        }
    }
}

/// 重启**网关栈**：在系统根（见 [`control_workdir`]）依次 `gops run stop` / `gops run start`。
///
/// 两步都带超时；任何一个失败都**明确报错**（尤其 `start` 失败时网关会停在停止态，
/// 回执里必须写出来，不能只报“命令失败”）。
async fn run_gateway_restart(config: &Config, task: Option<&TaskLog>) -> UpstreamResult {
    let dir = match control_workdir(config) {
        Ok(dir) => dir,
        Err(error) => {
            record_event(task, "config.error", &format!("重启：{error}"));
            return UpstreamResult {
                status_code: 500,
                body: String::new(),
                error: Some(format!("重启：{error}")),
            };
        }
    };
    let program = config
        .upgrader_program
        .clone()
        .unwrap_or_else(|| "gops".to_string());
    let timeout = Duration::from_secs(config.restart_timeout_seconds.unwrap_or(120).clamp(1, 300));

    let mut steps: Vec<serde_json::Value> = Vec::new();
    for step in ["stop", "start"] {
        record_event(
            task,
            "restart.step",
            &format!("{program} run {step}（cwd={}）", dir.display()),
        );
        match run_short(&program, &["run", step], &dir, timeout).await {
            Ok(output) if output.status.success() => {
                if let Some(task) = task {
                    task.output(&format!("< {program} run {step} ok\n"));
                }
                steps.push(json!({ "step": step, "exit": 0 }));
            }
            Ok(output) => {
                let code = output.status.code();
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mut error = format!(
                    "{program} run {step} 失败（exit={code:?}）：{}",
                    stderr_tail(&stderr, 10)
                );
                if step == "start" {
                    error.push_str("；网关可能仍处于停止状态，需人工确认");
                }
                if let Some(task) = task {
                    task.output(&format!("{error}\n"));
                }
                record_event(task, "restart.failed", &error);
                return UpstreamResult {
                    status_code: 500,
                    body: json!({ "restart": "gateway", "steps": steps }).to_string(),
                    error: Some(error),
                };
            }
            Err(err) => {
                let error = format!("{program} run {step} 执行失败：{err}");
                record_event(task, "restart.failed", &error);
                return UpstreamResult {
                    status_code: 500,
                    body: json!({ "restart": "gateway", "steps": steps }).to_string(),
                    error: Some(error),
                };
            }
        }
    }

    record_event(task, "restart.done", "stop + start");
    UpstreamResult {
        status_code: 200,
        body: json!({ "restart": "gateway", "steps": steps }).to_string(),
        error: None,
    }
}

/// 重启 **gwlinkd 自己**：先回执「已受理」，再延迟触发服务管理器重启。
///
/// 时序：回执由 relay 循环在 `execute_control` 返回后立即发出（毫秒级），这里 spawn 的任务
/// 先 `sleep 2s` 再调 `systemctl restart` / `launchctl kickstart -k` —— 回执先到，进程后换。
/// 回执语义是「已触发」，不是「已恢复」；恢复由随后的心跳/状态上报佐证。
async fn run_gwlinkd_restart(task: Option<&TaskLog>) -> UpstreamResult {
    let (platform, scope) = match gwlinkd_restart_plan(crate::service::any_installed_definition) {
        Ok(plan) => plan,
        Err(reason) => {
            record_event(task, "restart.rejected", &reason);
            return UpstreamResult {
                status_code: 409,
                body: String::new(),
                error: Some(reason),
            };
        }
    };
    let commands = crate::service::restart_commands(platform, scope);
    // macOS system 域 launchd 要 root 才能 `kickstart`：非 root 时这条路径必失败，
    // 不硬试（试了也是静默失败），直接走“自己退出、交给 KeepAlive 拉起”。
    let via_exit_fallback = crate::service::restart_requires_privilege(platform, scope);
    let supervised = crate::service::supervised_by_init();
    let shown: Vec<String> = if via_exit_fallback {
        vec![
            "self-exit（服务定义 KeepAlive 拉起：launchd system 域需要 root，kickstart 会被拒）"
                .to_string(),
        ]
    } else {
        commands
            .iter()
            .map(|command| command.display_line())
            .collect()
    };
    record_event(task, "restart.scheduled", &shown.join("; "));

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let mut last_error = if via_exit_fallback {
            "launchctl kickstart 需要 root（system 域）".to_string()
        } else {
            String::new()
        };
        if !via_exit_fallback {
            for command in commands {
                let shown = command.display_line();
                match tokio::task::spawn_blocking(move || {
                    crate::service::run_with_retries(&command)
                })
                .await
                {
                    Ok(Ok(outcome)) if outcome.success => {
                        log::info!("event=GwlinkdRestartCommandDone command={shown}");
                        return;
                    }
                    Ok(Ok(outcome)) => {
                        last_error =
                            format!("{shown} 失败（stderr={}）", stderr_tail(&outcome.stderr, 3));
                    }
                    Ok(Err(err)) => last_error = format!("{shown} 无法执行：{err}"),
                    Err(err) => last_error = format!("{shown} 任务加入失败：{err}"),
                }
            }
        }
        // 服务管理器没接受重启命令：自己退出，由 KeepAlive / `Restart=` 拉起（新进程会重读配置）。
        // **只在托管时**这么做：手动 `run` 退出去就再也没人拉起了。
        if supervised {
            log::warn!(
                "event=GwlinkdRestartViaExit reason={last_error}（服务管理器会拉起新进程，新进程重读配置）"
            );
            std::process::exit(0);
        }
        log::error!("event=GwlinkdRestartFailed reason={last_error}（未被服务管理器托管，不自杀）");
    });

    UpstreamResult {
        status_code: 202,
        body: json!({
            "restart": "gwlinkd",
            "scheduled": true,
            "via": shown,
            "restart_requires_privilege": via_exit_fallback,
            "supervised": supervised,
        })
        .to_string(),
        error: None,
    }
}

/// 只做「能不能重启自己」的判定，不执行：没有已装服务定义 = 手动 `run`，重启后没有人拉起。
fn gwlinkd_restart_plan(
    lookup: impl FnOnce() -> Option<crate::service::InstalledDefinition>,
) -> Result<
    (
        crate::service::ServicePlatform,
        crate::service::ServiceScope,
    ),
    String,
> {
    match lookup() {
        Some(definition) => Ok((definition.platform, definition.scope)),
        None => Err(
            "未检测到已安装的服务定义（可能是手动 run）：重启后没有服务管理器拉起，拒绝执行"
                .to_string(),
        ),
    }
}

/// 跑一个短命令（`gops run stop|start`），带超时。
async fn run_short(
    program: &str,
    args: &[&str],
    dir: &Path,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(format!("无法执行 {program}：{err}")),
        Err(_) => Err(format!("{program} 超时（{}s）已被终止", timeout.as_secs())),
    }
}

/// 跑一条 shell 命令（`sh -c`），带超时；用于可配置的 wparse 生效命令。
///
/// 超时要杀**整个进程组**：`gops run stop && gops run start` 里的 `gops` 不是 sh 的
/// 直接子进程（`&&` 会为每段起子 shell），只杀 sh 会让 `gops` 继续跑 —— 下次补偿再起一份，
/// 两份 `stop/start` 并发。
async fn run_shell(
    command: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // 自成进程组（pgid = 子进程 pid），超时时按组杀。
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|err| format!("无法执行生效命令 `{command}`：{err}"))?;
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(format!("无法执行生效命令 `{command}`：{err}")),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid {
                // 负号 = 进程组；组已不存在（ESRCH）也无需处理。
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            Err(format!(
                "生效命令 `{command}` 超时（{}s），已杀整个进程组；网关可能停在**停止态**，请人工确认后重试",
                timeout.as_secs()
            ))
        }
    }
}

/// 取 stderr 的最后 N 行（错误信息里只带尾巴，不把整段日志塞进回执）。
fn stderr_tail(text: &str, lines: usize) -> String {
    let mut tail: Vec<&str> = text.lines().rev().take(lines).collect();
    tail.reverse();
    tail.join("\n")
}

fn record_event(task: Option<&TaskLog>, phase: &str, detail: &str) {
    if let Some(task) = task {
        task.event(phase, Some(detail));
    }
}

/// 把发出去的请求写进 `output.log`（`>` 前缀 = 出站）。
fn record_request(task: Option<&TaskLog>, command: &WparseCommand, body: Option<&str>) {
    let Some(task) = task else {
        return;
    };
    task.output(&format!("> {} {}\n", command.method, command.path));
    let Some(body) = body else {
        return;
    };
    let total = body.len();
    if total <= REQUEST_PREVIEW_BYTES {
        task.output(&format!("> body ({total} bytes):\n{body}\n"));
        return;
    }
    // 切在字符边界：半个 UTF-8 字会让日志不可读。
    let mut cut = REQUEST_PREVIEW_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    task.output(&format!(
        "> body ({total} bytes, 只记前 {cut} 字节):\n{}\n> …\n",
        &body[..cut]
    ));
}

/// 把 wparse 的回应写进 `output.log`（`<` 前缀 = 入站）。
fn record_response(task: Option<&TaskLog>, status: u16, elapsed: Duration, body: &str) {
    let Some(task) = task else {
        return;
    };
    task.output(&format!("< {status} ({} ms)\n", elapsed.as_millis()));
    if !body.is_empty() {
        task.output(&format!("{body}\n"));
    }
}

fn is_allowed(method: &str, path: &str, allowed: &[&str]) -> bool {
    matches!((method, path), ("GET", STATUS_PATH) | ("POST", RELOAD_PATH))
        && allowed.contains(&path)
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_RESPONSE_BYTES {
        // 只在字符边界截断，避免把 UTF-8 切坏。
        let mut end = MAX_RESPONSE_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn build_upstream_client(
    ca_file: Option<&Path>,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(path) = ca_file {
        let pem = std::fs::read(path)
            .map_err(|err| format!("read upstream CA {}: {err}", path.display()))?;
        let certificate = reqwest::Certificate::from_pem(&pem)
            .map_err(|err| format!("parse upstream CA {}: {err}", path.display()))?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|err| format!("build upstream client: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn configured(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn allowed_paths_defaults_to_both_builtin_paths() {
        assert_eq!(
            allowed_paths(&[]),
            vec![STATUS_PATH, RELOAD_PATH],
            "empty config must allow both"
        );
    }

    #[test]
    fn allowed_paths_can_only_narrow() {
        assert_eq!(
            allowed_paths(&configured(&[RELOAD_PATH])),
            vec![RELOAD_PATH]
        );
        assert_eq!(
            allowed_paths(&configured(&[STATUS_PATH, "/etc/passwd"])),
            vec![STATUS_PATH],
            "unknown paths are not forwarded"
        );
        assert!(allowed_paths(&configured(&["/etc/passwd"])).is_empty());
    }

    #[test]
    fn method_path_whitelist_is_exact() {
        let both = [STATUS_PATH, RELOAD_PATH];
        assert!(is_allowed("GET", STATUS_PATH, &both));
        assert!(is_allowed("POST", RELOAD_PATH, &both));
        // 方法 / 路径任一不符都拒绝。
        assert!(!is_allowed("POST", STATUS_PATH, &both));
        assert!(!is_allowed("GET", RELOAD_PATH, &both));
        assert!(!is_allowed(
            "GET",
            "/admin/v1/reloads/model/../status",
            &both
        ));
        assert!(!is_allowed("GET", STATUS_PATH, &[RELOAD_PATH]));
    }

    #[test]
    fn truncate_keeps_utf8_boundaries() {
        let mut text = "a".repeat(MAX_RESPONSE_BYTES - 1);
        text.push('中'); // 3 bytes: 越过上限，必须整体丢掉
        let out = truncate(text);
        assert!(out.len() <= MAX_RESPONSE_BYTES);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.chars().all(|c| c == 'a'));
    }

    #[test]
    fn truncate_leaves_short_bodies_untouched() {
        assert_eq!(truncate("hello".to_string()), "hello");
    }

    #[test]
    fn upstream_client_rejects_a_missing_ca() {
        let err = build_upstream_client(
            Some(Path::new("/definitely/missing-ca.pem")),
            Duration::from_secs(1),
        )
        .expect_err("missing CA must fail closed");
        assert!(err.contains("read upstream CA"), "{err}");
    }

    // ---- 任务日志：一条命令一个目录，失败时要有现场 ----

    fn temp_state(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wist-gwlinkd-relay-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_token(state: &Path) -> PathBuf {
        let path = state.join("admin.token");
        std::fs::write(&path, "secret-admin-token\n").unwrap();
        path
    }

    fn command(method: &str, path: &str) -> WparseCommand {
        WparseCommand {
            command_id: "cmd-1".to_string(),
            kind: "proxy".to_string(),
            method: method.to_string(),
            path: path.to_string(),
            body: Some(json!({"model": "v1"})),
            lease_expires_at: "2026-10-20T12:00:00Z".to_string(),
        }
    }

    // ---- 控制类命令（kind=diagnose）----

    fn diagnose_command() -> WparseCommand {
        WparseCommand {
            command_id: "cmd-diag".to_string(),
            kind: "diagnose".to_string(),
            method: String::new(),
            path: String::new(),
            body: None,
            lease_expires_at: "2026-10-20T12:00:00Z".to_string(),
        }
    }

    #[cfg(unix)]
    fn stub_gops(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("stub-gops");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[cfg(unix)]
    fn diagnose_config(dir: &Path, program: &str, timeout: u64) -> Config {
        // cwd 必须是系统根：这里把同一个目录当作系统根（配了 sys/sys_model.yml）。
        make_system_root(dir);
        let path = dir.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\ndiagnose_program = \"{program}\"\ndiagnose_timeout_seconds = {timeout}\n",
                dir.display(),
                dir.display()
            ),
        )
        .unwrap();
        Config::load(&path).expect("config")
    }

    /// 控制命令的配置：`state_dir` 在 `state`，`upgrade_project_dir` 单独指定（可为工程根）。
    fn control_config(
        state: &Path,
        project_dir: &Path,
        project_name: Option<&str>,
        program: &str,
    ) -> Config {
        let path = state.join("gwlinkd.toml");
        let name = project_name
            .map(|name| format!("upgrade_project_name = \"{name}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\n{name}diagnose_program = \"{program}\"\ndiagnose_timeout_seconds = 10\nupgrader_program = \"{program}\"\n",
                state.display(),
                project_dir.display()
            ),
        )
        .unwrap();
        Config::load(&path).expect("config")
    }

    /// 造一个系统根（默认内收布局 `sys/sys_model.yml`）。
    fn make_system_root(dir: &Path) {
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stack".to_string());
        std::fs::write(
            dir.join("sys").join("sys_model.yml"),
            format!("name: {name}\nkind: docker-compose\n"),
        )
        .unwrap();
    }

    #[test]
    fn a_command_without_kind_is_a_proxy_command() {
        let parsed: WparseCommand = serde_json::from_str(
            r#"{"command_id":"c1","method":"GET","path":"/x","lease_expires_at":"t"}"#,
        )
        .unwrap();
        assert_eq!(parsed.kind, "proxy");
        assert!(!parsed.is_control());
    }

    #[test]
    fn diagnose_kind_is_parsed_from_the_center_payload() {
        let parsed: WparseCommand = serde_json::from_str(
            r#"{"command_id":"c1","kind":"diagnose","method":"","path":"","lease_expires_at":"t"}"#,
        )
        .unwrap();
        assert!(parsed.is_control());
        assert_eq!(parsed.method, "", "控制命令没有 method/path");
    }

    #[test]
    fn diagnose_commands_are_control_commands_and_always_recorded() {
        let command = diagnose_command();
        assert!(command.is_control());
        assert!(should_record(&command), "控制类命令必须落任务目录");
        assert!(!should_record(&super::tests::command("GET", STATUS_PATH)));
    }

    #[test]
    fn an_unknown_control_kind_fails_loudly() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut command = diagnose_command();
        command.kind = "restart-everything".to_string();
        let state = temp_state("unknown-kind");
        let config = diagnose_config(&state, "/bin/false", 10);
        let result = runtime.block_on(execute_control(&config, &command, None));
        assert_eq!(result.status_code, 400);
        assert!(
            result.error.unwrap_or_default().contains("不支持"),
            "要明确报不支持，不能静默成功"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_exit_zero_returns_the_report() {
        let state = temp_state("diag-zero");
        let stub = stub_gops(
            &state,
            "echo '{\"schema_version\":\"1\",\"summary\":{}}'\nexit 0",
        );
        let config = diagnose_config(&state, &stub, 10);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 200);
        assert!(result.error.is_none());
        assert!(result.body.contains("schema_version"), "{}", result.body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_exit_one_is_still_a_successful_command() {
        // gops 发现问题时会退 1；报告本身就是回执内容，不能当“命令没跑成”。
        let state = temp_state("diag-one");
        let stub = stub_gops(
            &state,
            "echo '{\"summary\":{\"exit_code\":1,\"fail\":1}}'\nexit 1",
        );
        let config = diagnose_config(&state, &stub, 10);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 200);
        assert!(result.error.is_none());
        assert!(result.body.contains("\"fail\":1"), "{}", result.body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_exit_two_is_a_failure_with_stderr_tail() {
        let state = temp_state("diag-two");
        let stub = stub_gops(&state, "echo 'compose 不可用' >&2\nexit 2");
        let config = diagnose_config(&state, &stub, 10);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 500);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("exit=Some(2)"), "{error}");
        assert!(error.contains("compose 不可用"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_output_must_be_json() {
        let state = temp_state("diag-badjson");
        let stub = stub_gops(&state, "echo 'not json'\nexit 0");
        let config = diagnose_config(&state, &stub, 10);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 500);
        assert!(
            result.error.unwrap_or_default().contains("不是合法 JSON"),
            "输出不合契约要报错"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_timeout_kills_the_child() {
        let state = temp_state("diag-timeout");
        let stub = stub_gops(&state, "sleep 30");
        let config = diagnose_config(&state, &stub, 1);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 504);
        assert!(result.error.unwrap_or_default().contains("超时"));
    }

    #[test]
    fn diagnose_without_a_project_dir_fails_closed() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let state = temp_state("diag-noproject");
        let path = state.join("gwlinkd.toml");
        std::fs::write(&path, format!("state_dir = \"{}\"\n", state.display())).unwrap();
        let config = Config::load(&path).unwrap();
        let result = runtime.block_on(execute_control(&config, &diagnose_command(), None));
        assert_eq!(result.status_code, 500);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("upgrade_project_dir"),
            "缺工程根要说清楚缺什么"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_invokes_gops_with_the_contract_flags() {
        let state = temp_state("diag-argv");
        let argv = state.join("argv.txt");
        let stub = stub_gops(
            &state,
            &format!(
                "printf '%s ' \"$@\" > {}\necho '{{\"schema_version\":\"1\"}}'\nexit 0",
                argv.display()
            ),
        );
        let config = diagnose_config(&state, &stub, 10);
        // 就算带了 method/path，控制命令也**不能**走 wparse 转发路径。
        let mut command = diagnose_command();
        command.method = "POST".to_string();
        command.path = "/admin/v1/reloads/model".to_string();
        let result = execute_control(&config, &command, None).await;
        assert_eq!(result.status_code, 200);
        let seen = std::fs::read_to_string(&argv).unwrap();
        assert!(seen.contains("run diagnose --json --no-color"), "{seen}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn diagnose_runs_in_the_system_root_not_the_project_root() {
        // 现场回归：工程根跑 diagnose → gx 报“系统未定义 diagnose 流程”（误导）。
        let state = temp_state("diag-cwd");
        let project = state.join("prj");
        let system = project.join("wist-gateway-stack");
        make_system_root(&system);
        std::fs::write(project.join("ops-prj.yml"), "kind: ops\n").unwrap();
        let cwd_file = state.join("cwd.txt");
        let stub = stub_gops(
            &state,
            &format!(
                "pwd > {}\necho '{{\"schema_version\":\"1\"}}'\nexit 0",
                cwd_file.display()
            ),
        );
        let config = control_config(&state, &project, None, &stub);
        let result = execute_control(&config, &diagnose_command(), None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let seen = std::fs::read_to_string(&cwd_file).unwrap();
        assert_eq!(
            std::fs::canonicalize(seen.trim()).unwrap(),
            std::fs::canonicalize(&system).unwrap(),
            "diagnose 的 cwd 必须是系统根"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_uses_the_system_root_as_cwd() {
        // `gops run stop|start` 也是从 cwd 解析系统，cwd 错了同样重启不了。
        let state = temp_state("restart-cwd");
        let project = state.join("prj");
        let system = project.join("wist-gateway-stack");
        make_system_root(&system);
        let cwd_file = state.join("cwd.txt");
        let stub = stub_gops(&state, &format!("pwd >> {}\nexit 0", cwd_file.display()));
        let config = control_config(&state, &project, None, &stub);
        let result = run_gateway_restart(&config, None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let seen = std::fs::read_to_string(&cwd_file).unwrap();
        let lines: Vec<&str> = seen.lines().collect();
        assert_eq!(lines.len(), 2, "stop + start 都要跑：{seen}");
        for line in lines {
            assert_eq!(
                std::fs::canonicalize(line.trim()).unwrap(),
                std::fs::canonicalize(&system).unwrap(),
                "restart 的 cwd 必须是系统根"
            );
        }
    }

    #[test]
    fn control_commands_fail_closed_when_the_system_root_is_ambiguous() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let state = temp_state("cwd-ambiguous");
        let project = state.join("prj");
        make_system_root(&project.join("stack-a"));
        make_system_root(&project.join("stack-b"));
        let config = control_config(&state, &project, None, "/bin/true");
        let diag = runtime.block_on(execute_control(&config, &diagnose_command(), None));
        assert_eq!(diag.status_code, 500);
        assert!(diag.error.unwrap_or_default().contains("多个系统"));
        let restart = runtime.block_on(run_gateway_restart(&config, None));
        assert_eq!(restart.status_code, 500);
        assert!(restart.error.unwrap_or_default().contains("多个系统"));
    }

    #[test]
    fn restart_gateway_is_rejected_without_a_service_definition() {
        // 手动 `run` 时重启后没人拉起 —— 必须拒绝，而不是把进程杀掉。
        let err = gwlinkd_restart_plan(|| None).unwrap_err();
        assert!(err.contains("服务定义"), "{err}");
    }

    #[test]
    fn stderr_tail_keeps_only_the_last_lines() {
        let text = (1..=20)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let tail = stderr_tail(&text, 3);
        assert_eq!(tail, "18\n19\n20");
        assert_eq!(stderr_tail("only", 5), "only");
        assert_eq!(stderr_tail("", 5), "");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_gateway_runs_stop_then_start() {
        let state = temp_state("restart-order");
        let argv = state.join("argv.txt");
        let stub = stub_gops(
            &state,
            &format!("printf '%s\\n' \"$*\" >> {}\nexit 0", argv.display()),
        );
        let mut config = diagnose_config(&state, &stub, 10);
        config.upgrader_program = Some(stub);
        let result = run_gateway_restart(&config, None).await;
        assert_eq!(result.status_code, 200);
        assert!(result.error.is_none());
        let seen = std::fs::read_to_string(&argv).unwrap();
        assert_eq!(seen, "run stop\nrun start\n", "必须是先 stop 再 start");
        assert!(result.body.contains("\"gateway\""), "{}", result.body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_gateway_stops_at_the_failing_step() {
        let state = temp_state("restart-fail");
        let argv = state.join("argv.txt");
        let stub = stub_gops(
            &state,
            &format!(
                "printf '%s\\n' \"$*\" >> {}\necho 'compose 不可用' >&2\nexit 1",
                argv.display()
            ),
        );
        let mut config = diagnose_config(&state, &stub, 10);
        config.upgrader_program = Some(stub);
        let result = run_gateway_restart(&config, None).await;
        assert_eq!(result.status_code, 500);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("run stop"), "{error}");
        assert!(error.contains("compose 不可用"), "{error}");
        let seen = std::fs::read_to_string(&argv).unwrap();
        assert_eq!(seen, "run stop\n", "失败后不能再跑 start");
    }

    #[test]
    fn task_meta_carries_the_command_kind() {
        let meta = task_meta("gw-1", "https://127.0.0.1:19090", &diagnose_command());
        assert_eq!(meta["kind"], json!("diagnose"));
    }

    fn new_task(state: &Path, command: &WparseCommand) -> std::sync::Arc<TaskLog> {
        task_log::TaskLog::create(
            state,
            "wparse",
            &command.command_id,
            task_meta("gw-test", "https://127.0.0.1:19090", command),
        )
        .expect("task log")
    }

    fn read_logs(task: &TaskLog) -> (String, String) {
        let events = std::fs::read_to_string(task.dir().join("events.jsonl")).unwrap_or_default();
        let output = std::fs::read_to_string(task.dir().join("output.log")).unwrap_or_default();
        (events, output)
    }

    /// 收一次请求，回 `status` + `body`，并把请求原文送回来。
    async fn serve_once(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match tokio::time::timeout(Duration::from_millis(200), sock.read(&mut chunk)).await
                {
                    Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                    Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn task_json_carries_the_model_identity() {
        let state = temp_state("identity");
        let mut command = command("POST", RELOAD_PATH);
        command.body = Some(json!({
            "version": "v2026.10.20-1",
            "group": "wist-dev-gateway",
            "reason": "warpstation publish",
            "wait": true,
        }));
        let task = new_task(&state, &command);

        let raw = std::fs::read_to_string(task.dir().join("task.json")).unwrap();
        assert!(raw.contains("v2026.10.20-1"), "{raw}");
        assert!(raw.contains("wist-dev-gateway"), "{raw}");
        assert!(raw.contains("warpstation publish"), "{raw}");
        assert!(raw.contains("\"method\": \"POST\""), "{raw}");
        assert!(raw.contains("gw-test"), "{raw}");
        assert!(raw.contains("127.0.0.1:19090"), "{raw}");
    }

    #[test]
    fn body_fields_are_optional() {
        assert_eq!(body_field(&None, "version"), None);
        assert_eq!(body_field(&Some(json!({})), "version"), None);
        assert_eq!(body_field(&Some(json!({"version": 3})), "version"), None);
        assert_eq!(
            body_field(&Some(json!({"version": "v1"})), "version").as_deref(),
            Some("v1")
        );
    }

    #[test]
    fn should_record_skips_status_polls() {
        assert!(!should_record(&command("GET", STATUS_PATH)));
        assert!(should_record(&command("POST", RELOAD_PATH)));
        // 方法大小写不作为“要不要落盘”的依据。
        assert!(!should_record(&command("get", STATUS_PATH)));
        assert!(should_record(&command("post", RELOAD_PATH)));
    }

    #[test]
    fn http_4xx_is_a_failure_not_a_success() {
        // 中心只看 error：4xx 没有传输错误，但必须被当成失败。
        let result = UpstreamResult {
            status_code: 422,
            body: r#"{"error":"invalid model"}"#.to_string(),
            error: None,
        };
        let (ok, status, error) = outcome(&result);
        assert!(!ok);
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("wparse 上游返回 422"));

        let ok_result = UpstreamResult {
            status_code: 200,
            body: r#"{"accepted":true}"#.to_string(),
            error: None,
        };
        assert_eq!(outcome(&ok_result), (true, "ok", None));

        let transport = UpstreamResult {
            status_code: 502,
            body: String::new(),
            error: Some("wparse admin request failed: refused".to_string()),
        };
        let (ok, status, error) = outcome(&transport);
        assert!(!ok);
        assert_eq!(status, "failed");
        assert!(error.unwrap().contains("refused"));
    }

    #[test]
    fn log_dir_is_only_attached_to_failures() {
        assert_eq!(
            with_log_dir(None, Some("/tmp/x")),
            None,
            "成功时不能带 error"
        );
        assert_eq!(
            with_log_dir(Some("boom".to_string()), Some("/tmp/x")).as_deref(),
            Some("boom；日志: /tmp/x")
        );
        assert_eq!(
            with_log_dir(Some("boom".to_string()), None).as_deref(),
            Some("boom")
        );
    }

    #[test]
    fn request_body_preview_is_bounded_and_utf8_safe() {
        let state = temp_state("preview");
        let command = command("POST", RELOAD_PATH);
        let task = new_task(&state, &command);
        // `字` 占 3 字节，8 KiB 不是 3 的倍数 —— 正好落在字符中间。
        let body = "字".repeat(4000);
        record_request(Some(task.as_ref()), &command, Some(&body));

        let (_, output) = read_logs(&task);
        assert!(output.contains("只记前"), "{output}");
        let preview = output
            .split("只记前")
            .nth(1)
            .and_then(|rest| rest.split(" 字节):\n").nth(1))
            .and_then(|rest| rest.split("\n> …").next())
            .expect("preview block");
        assert!(preview.len() <= REQUEST_PREVIEW_BYTES, "{}", preview.len());
        assert!(preview.is_char_boundary(preview.len()));
        assert!(!preview.contains('\u{fffd}'), "不能写出半个 UTF-8 字");
    }

    #[tokio::test]
    async fn records_the_exchange_and_never_writes_the_token() {
        let state = temp_state("exchange");
        let token = write_token(&state);
        let (base, captured) = serve_once(200, r#"{"success":true,"request_id":"cmd-1"}"#).await;
        let http = build_upstream_client(None, Duration::from_secs(5)).expect("client");
        let command = command("POST", RELOAD_PATH);
        let task = new_task(&state, &command);

        let result = execute(
            &http,
            &token,
            &base,
            &[STATUS_PATH, RELOAD_PATH],
            &command,
            Some(task.as_ref()),
        )
        .await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        assert!(result.error.is_none());
        assert!(result.body.contains("request_id"));

        // 请求真的发出去了：方法 / 路径 / body / request-id 都对。
        let raw = captured.await.expect("captured request");
        assert!(raw.starts_with("POST /admin/v1/reloads/model "), "{raw}");
        assert!(raw.contains("\"model\":\"v1\""), "{raw}");
        assert!(
            raw.to_ascii_lowercase().contains("x-request-id: cmd-1"),
            "{raw}"
        );

        let (events, output) = read_logs(&task);
        assert!(
            output.contains("> POST /admin/v1/reloads/model"),
            "{output}"
        );
        assert!(
            output.contains("\"model\":\"v1\""),
            "请求原文要入日志：{output}"
        );
        assert!(output.contains("< 200"), "{output}");
        assert!(output.contains("request_id"), "响应原文要入日志：{output}");
        assert!(
            !output.contains("secret-admin-token"),
            "admin token 不能进日志：{output}"
        );
        assert!(!events.contains("upstream.failed"), "{events}");
    }

    #[tokio::test]
    async fn rejected_path_is_logged_and_never_sent() {
        let state = temp_state("reject");
        let token = write_token(&state);
        let http = build_upstream_client(None, Duration::from_secs(2)).expect("client");
        let command = command("GET", "/admin/v1/secrets");
        let task = new_task(&state, &command);

        let result = execute(
            &http,
            &token,
            "http://127.0.0.1:1",
            &[STATUS_PATH],
            &command,
            Some(task.as_ref()),
        )
        .await;
        assert_eq!(result.status_code, 403);

        let (events, output) = read_logs(&task);
        assert!(events.contains("\"phase\":\"reject\""), "{events}");
        assert!(events.contains("path not allowed"), "{events}");
        assert!(output.is_empty(), "没发出去就不该有来回记录：{output}");
    }

    #[tokio::test]
    async fn missing_token_is_recorded_as_config_error() {
        let state = temp_state("no-token");
        let missing = state.join("absent.token");
        let http = build_upstream_client(None, Duration::from_secs(2)).expect("client");
        let command = command("POST", RELOAD_PATH);
        let task = new_task(&state, &command);

        let result = execute(
            &http,
            &missing,
            "http://127.0.0.1:1",
            &[RELOAD_PATH],
            &command,
            Some(task.as_ref()),
        )
        .await;
        assert_eq!(result.status_code, 502);

        let (events, _) = read_logs(&task);
        assert!(events.contains("\"phase\":\"config.error\""), "{events}");
        assert!(events.contains("read admin token"), "{events}");
    }

    #[tokio::test]
    async fn transport_error_is_recorded_and_reported_as_502() {
        let state = temp_state("transport");
        let token = write_token(&state);
        // 先占一个端口再释放：保证连上去就是拒绝。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let http = build_upstream_client(None, Duration::from_secs(2)).expect("client");
        let command = command("POST", RELOAD_PATH);
        let task = new_task(&state, &command);

        let result = execute(
            &http,
            &token,
            &format!("http://{addr}"),
            &[RELOAD_PATH],
            &command,
            Some(task.as_ref()),
        )
        .await;
        assert_eq!(result.status_code, 502);

        let (events, output) = read_logs(&task);
        assert!(events.contains("\"phase\":\"upstream.failed\""), "{events}");
        assert!(output.contains("< transport error"), "{output}");
    }

    #[test]
    fn tenant_id_must_be_tag_safe() {
        assert!(is_tag_safe("tenant-acme"));
        assert!(is_tag_safe("t.1/a=b@c+d,e:f"));
        assert!(!is_tag_safe(""));
        assert!(!is_tag_safe("tenant acme"));
        assert!(!is_tag_safe(&"a".repeat(65)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_writes_env_and_marks_applied() {
        let state = temp_state("bind-tenant");
        std::fs::create_dir_all(state.join("sys")).unwrap();
        std::fs::write(
            state.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = state.join("data-plane-run/dev");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join(".env"), "OTHER=keep\n").unwrap();
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\nwparse_work_dir = \"{}\"\nwparse_apply_command = \"true\"\nwparse_apply_timeout_seconds = 5\n",
                state.display(),
                state.display(),
                work.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).expect("config");
        let command = WparseCommand {
            command_id: "cmd-1".into(),
            kind: "bind-tenant".into(),
            method: String::new(),
            path: String::new(),
            body: Some(json!({"tenant_id": "tenant-acme", "revision": 1})),
            lease_expires_at: "2030-01-01T00:00:00Z".into(),
        };

        let result = run_bind_tenant(&config, &command, None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let body: serde_json::Value = serde_json::from_str(&result.body).expect("body");
        assert_eq!(body["wparse_applied"], true);
        let env = std::fs::read_to_string(work.join(".env")).unwrap();
        assert!(env.contains("OTHER=keep"));
        assert!(env.contains("WIST_TENANT_ID=tenant-acme"));
        let identity =
            crate::state::load_business_identity(config.identity_dir()).expect("identity");
        assert_eq!(identity.tenant_id, "tenant-acme");
        assert_eq!(identity.revision, 1);
        assert!(identity.wparse_applied);
        let _ = std::fs::remove_dir_all(state);
    }

    /// 没配置生效命令、也推不出缺省时：`.env` 写了，但必须**明确告诉中心「数据面没生效」**。
    ///
    /// 现场教训（2026-10-25）：回执只有 `wparse_applied:false` 时中心什么也不记，
    /// 页面就停在「已下发」，人只能上机翻 gwlinkd 配置才发现生效动作没跑。
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_without_apply_command_reports_not_configured() {
        let state = temp_state("bind-tenant-no-apply");
        // 工作根刻意不在 `<系统根>/data-plane-run/{dev,work}` 布局下，且没有工程根：
        // 缺省推不出来 —— 这是唯一该走到 `not_configured` 的形态。
        let work = state.join("work-elsewhere");
        std::fs::create_dir_all(&work).unwrap();
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nwparse_work_dir = \"{}\"\n",
                state.display(),
                work.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).expect("config");
        let command = WparseCommand {
            command_id: "cmd-2".into(),
            kind: "bind-tenant".into(),
            method: String::new(),
            path: String::new(),
            body: Some(json!({"tenant_id": "tenant-acme", "revision": 3})),
            lease_expires_at: "2030-01-01T00:00:00Z".into(),
        };

        let result = run_bind_tenant(&config, &command, None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let body: serde_json::Value = serde_json::from_str(&result.body).expect("body");
        assert_eq!(body["wparse_applied"], false);
        assert_eq!(body["apply"], "not_configured");
        assert!(
            body["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("wparse_apply_command")),
            "{body}"
        );
        // `.env` 仍然写了（下次启动就生效），只是这次还没落到数据面。
        let env = std::fs::read_to_string(work.join(".env")).unwrap();
        assert!(env.contains("WIST_TENANT_ID=tenant-acme"));
        let identity =
            crate::state::load_business_identity(config.identity_dir()).expect("identity");
        assert!(!identity.wparse_applied);
        let _ = std::fs::remove_dir_all(state);
    }

    /// 一条配置都不写时也要真的生效：开发态缺省用栈根的 `dev/svc.sh`。
    ///
    /// 现场教训（2026-10-26）：让「生效」只依赖一条手写配置，等于把绑定客户的成败
    /// 挂在一次 `localize` + 一次重启上；缺省必须能从现场自己推得出来。
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_defaults_to_svc_sh_in_the_dev_layout() {
        use std::os::unix::fs::PermissionsExt;

        let state = temp_state("bind-tenant-default-dev");
        std::fs::create_dir_all(state.join("sys")).unwrap();
        std::fs::write(
            state.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        std::fs::create_dir_all(state.join("dev")).unwrap();
        let marker = state.join("svc-args");
        std::fs::write(
            state.join("dev/svc.sh"),
            format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            state.join("dev/svc.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let work = state.join("data-plane-run/dev");
        std::fs::create_dir_all(&work).unwrap();
        // 只配 state_dir 与工作根：生效命令靠缺省。
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nwparse_work_dir = \"{}\"\n",
                state.display(),
                work.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).expect("config");
        let command = WparseCommand {
            command_id: "cmd-default".into(),
            kind: "bind-tenant".into(),
            method: String::new(),
            path: String::new(),
            body: Some(json!({"tenant_id": "tenant-acme", "revision": 1})),
            lease_expires_at: "2030-01-01T00:00:00Z".into(),
        };

        let result = run_bind_tenant(&config, &command, None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let body: serde_json::Value = serde_json::from_str(&result.body).expect("body");
        assert_eq!(body["wparse_applied"], true, "{body}");
        assert_eq!(
            std::fs::read_to_string(&marker).expect("缺省生效动作没跑"),
            "restart wparse --no-build"
        );
        let env = std::fs::read_to_string(work.join(".env")).unwrap();
        assert!(env.contains("WIST_TENANT_ID=tenant-acme"));
        let _ = std::fs::remove_dir_all(state);
    }

    /// 发布态缺省也要真的执行（不只是算出字符串）：`stop` 与 `start` 两段都要跑到，
    /// 且 cwd 是**从工作根反推的系统根**（gops 从 cwd 解析系统）。
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_defaults_to_the_stack_restart_in_the_release_layout() {
        use std::os::unix::fs::PermissionsExt;

        let state = temp_state("bind-tenant-default-release");
        std::fs::create_dir_all(state.join("sys")).unwrap();
        std::fs::write(
            state.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = state.join("data-plane-run/work");
        std::fs::create_dir_all(&work).unwrap();
        // 假执行器：记下每次调用的参数，兼验证 cwd（落文件时用相对路径）。
        let program = state.join("fake-gops");
        std::fs::write(&program, "#!/bin/sh\necho \"$*\" >> calls.log\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nwparse_work_dir = \"{}\"\nupgrader_program = \"{}\"\n",
                state.display(),
                work.display(),
                program.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).expect("config");
        let command = WparseCommand {
            command_id: "cmd-default-release".into(),
            kind: "bind-tenant".into(),
            method: String::new(),
            path: String::new(),
            body: Some(json!({"tenant_id": "tenant-acme", "revision": 2})),
            lease_expires_at: "2030-01-01T00:00:00Z".into(),
        };

        let result = run_bind_tenant(&config, &command, None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let body: serde_json::Value = serde_json::from_str(&result.body).expect("body");
        assert_eq!(body["wparse_applied"], true, "{body}");
        let calls = std::fs::read_to_string(state.join("calls.log"))
            .expect("cwd 不对：假执行器没在系统根落文件");
        assert_eq!(calls, "run stop\nrun start\n", "{calls}");
        let _ = std::fs::remove_dir_all(state);
    }

    fn business_identity(
        tenant_id: &str,
        revision: i64,
        wparse_applied: bool,
    ) -> crate::state::BusinessIdentity {
        crate::state::BusinessIdentity {
            tenant_id: tenant_id.to_string(),
            revision,
            wparse_applied,
        }
    }

    #[test]
    fn needs_business_apply_tracks_tenant_revision_and_pending_apply() {
        let desired = DesiredBusinessIdentity {
            tenant_id: "tenant-acme".into(),
            revision: 2,
        };
        // 从未绑定过 → 要应用。
        assert!(needs_business_apply(None, &desired, false));
        // 换绑（tenant 不同）→ 要应用。
        assert!(needs_business_apply(
            Some(&business_identity("tenant-old", 2, true)),
            &desired,
            false
        ));
        // 本地版本旧 → 要应用。
        assert!(needs_business_apply(
            Some(&business_identity("tenant-acme", 1, true)),
            &desired,
            false
        ));
        // 一致且已生效 → 不动。
        assert!(!needs_business_apply(
            Some(&business_identity("tenant-acme", 2, true)),
            &desired,
            true
        ));
        // 一致但上次没跑成：配置了生效命令才重试。
        assert!(needs_business_apply(
            Some(&business_identity("tenant-acme", 2, false)),
            &desired,
            true
        ));
        assert!(!needs_business_apply(
            Some(&business_identity("tenant-acme", 2, false)),
            &desired,
            false
        ));
        // 本地版本更新（回执最多 30s 才刷新，可能比刚执行的命令旧）→ **绝不回退**，
        // 哪怕 tenant 不同：否则一次换绑会被旧回执来回覆盖（每次都整栈重启）。
        assert!(!needs_business_apply(
            Some(&business_identity("tenant-acme", 3, true)),
            &desired,
            true
        ));
        assert!(!needs_business_apply(
            Some(&business_identity("tenant-newer", 3, true)),
            &desired,
            true
        ));
    }

    /// 离线补偿：状态回执带中心期望值 → relay 写 `.env` + 落身份 + 跑生效命令。
    #[cfg(unix)]
    #[tokio::test]
    async fn compensate_desired_identity_writes_env_and_marks_applied() {
        let state = temp_state("compensate");
        std::fs::create_dir_all(state.join("sys")).unwrap();
        std::fs::write(
            state.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        let work = state.join("data-plane-run/dev");
        std::fs::create_dir_all(&work).unwrap();
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\nwparse_work_dir = \"{}\"\nwparse_apply_command = \"true\"\nwparse_apply_timeout_seconds = 5\n",
                state.display(),
                state.display(),
                work.display()
            ),
        )
        .unwrap();
        let config = Config::load(&path).expect("config");
        let mut last_attempt = None;

        compensate_desired_identity(
            &config,
            Some(DesiredBusinessIdentity {
                tenant_id: "tenant-acme".into(),
                revision: 4,
            }),
            &mut last_attempt,
        )
        .await;

        let env = std::fs::read_to_string(work.join(".env")).unwrap();
        assert!(env.contains("WIST_TENANT_ID=tenant-acme"), "{env}");
        let identity =
            crate::state::load_business_identity(config.identity_dir()).expect("identity");
        assert_eq!(identity.tenant_id, "tenant-acme");
        assert_eq!(identity.revision, 4);
        assert!(identity.wparse_applied);
        assert!(last_attempt.is_some());
        let _ = std::fs::remove_dir_all(state);
    }

    /// bind 场景的配置：系统根 + 工作根 + 可配置生效命令。
    fn bind_config(state: &Path, work: &Path, apply_command: &str) -> Config {
        std::fs::create_dir_all(state.join("sys")).unwrap();
        std::fs::write(
            state.join("sys/sys_model.yml"),
            "name: test\nkind: docker-compose\n",
        )
        .unwrap();
        std::fs::create_dir_all(work).unwrap();
        let path = state.join("gwlinkd.toml");
        std::fs::write(
            &path,
            format!(
                "state_dir = \"{}\"\nupgrade_project_dir = \"{}\"\nwparse_work_dir = \"{}\"\nwparse_apply_command = \"{}\"\nwparse_apply_timeout_seconds = 5\n",
                state.display(),
                state.display(),
                work.display(),
                apply_command
            ),
        )
        .unwrap();
        Config::load(&path).expect("config")
    }

    fn bind_command(tenant_id: &str, revision: i64) -> WparseCommand {
        WparseCommand {
            command_id: "cmd-bind".into(),
            kind: "bind-tenant".into(),
            method: String::new(),
            path: String::new(),
            body: Some(json!({ "tenant_id": tenant_id, "revision": revision })),
            lease_expires_at: "2030-01-01T00:00:00Z".into(),
        }
    }

    /// 同 revision / 同 tenant 且已生效：幂等返回，**不重跑生效命令**（命令租约过期会重投）。
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_is_idempotent_and_does_not_restart_twice() {
        let state = temp_state("bind-idempotent");
        let work = state.join("data-plane-run/dev");
        let marker = state.join("apply-ran");
        let command = format!("echo ran >> {}", marker.display());
        let config = bind_config(&state, &work, &command);
        crate::state::save_business_identity(
            config.identity_dir(),
            &business_identity("tenant-acme", 1, true),
        )
        .expect("seed identity");

        let result = run_bind_tenant(&config, &bind_command("tenant-acme", 1), None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        let body: serde_json::Value = serde_json::from_str(&result.body).expect("body");
        assert_eq!(body["apply"], "unchanged");
        assert_eq!(body["wparse_applied"], true);
        assert!(!marker.exists(), "幂等返回不应重生效");

        // 换绑（revision 递增）→ 才真正重生效。
        let result = run_bind_tenant(&config, &bind_command("tenant-acme", 2), None).await;
        assert_eq!(result.status_code, 200, "{:?}", result.error);
        assert!(marker.exists(), "新 revision 应该重生效");
        let _ = std::fs::remove_dir_all(state);
    }

    /// 旧 revision + 非法 tenant：必须 400（不能因为 stale 就“成功”掉）。
    #[cfg(unix)]
    #[tokio::test]
    async fn bind_tenant_rejects_an_unsafe_tenant_even_when_the_command_is_stale() {
        let state = temp_state("bind-unsafe-stale");
        let work = state.join("data-plane-run/dev");
        let config = bind_config(&state, &work, "true");
        crate::state::save_business_identity(
            config.identity_dir(),
            &business_identity("tenant-acme", 5, true),
        )
        .expect("seed identity");

        let result = run_bind_tenant(&config, &bind_command("bad value", 1), None).await;
        assert_eq!(result.status_code, 400, "{:?}", result.error);
        let env = std::fs::read_to_string(work.join(".env")).unwrap_or_default();
        assert!(!env.contains("bad value"), "非法值不得写入 .env：{env}");
        let _ = std::fs::remove_dir_all(state);
    }

    /// 恢复/误删 `.env`：身份文件说「已生效」也要重新校验现场值（否则数据面永远 unbound）。
    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_rewrites_the_env_when_it_is_missing_or_stale() {
        let state = temp_state("reconcile-env");
        let work = state.join("data-plane-run/dev");
        let config = bind_config(&state, &work, "true");
        crate::state::save_business_identity(
            config.identity_dir(),
            &business_identity("tenant-acme", 2, true),
        )
        .expect("seed identity");
        std::fs::write(work.join(".env"), "OTHER=keep\nWIST_TENANT_ID=tenant-old\n").unwrap();

        reconcile_business_identity(&config).await;

        let env = std::fs::read_to_string(work.join(".env")).unwrap();
        assert!(env.contains("OTHER=keep"), "其它键要保留：{env}");
        assert!(env.contains("WIST_TENANT_ID=tenant-acme"), "{env}");
        let identity =
            crate::state::load_business_identity(config.identity_dir()).expect("identity");
        assert!(identity.wparse_applied, "重写后应重新标成已生效");
        let _ = std::fs::remove_dir_all(state);
    }
}
