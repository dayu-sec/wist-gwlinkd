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

use crate::center::{CenterClient, WparseCommand, build_mtls_http_client};
use crate::config::Config;
use crate::state;
use crate::task_log::{self, TaskLog};

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
pub async fn run(config: Config) {
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
    log::info!(
        "event=WparseRelayStarted upstream={} poll_ms={}",
        upstream,
        poll.as_millis()
    );

    loop {
        ticker.tick().await;

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

        let result = execute(
            &http,
            &token_file,
            &upstream,
            &allowed,
            &command,
            task.as_deref(),
        )
        .await;
        let (ok, status, mut error) = outcome(&result);
        // 收尾在回执**之前**：失败时要把日志目录带回中心（升级链路同口径）。
        let log_dir = task
            .as_ref()
            .map(|task| task.finish(status, error.as_deref().unwrap_or("")));
        if ok {
            log::info!(
                "event=WparseCommandDone command_id={} method={} path={} upstream_status={} log_dir={}",
                command.command_id,
                command.method,
                command.path,
                result.status_code,
                log_dir.as_deref().unwrap_or("-")
            );
        } else {
            log::warn!(
                "event=WparseCommandFailed command_id={} method={} path={} upstream_status={} log_dir={} error={}",
                command.command_id,
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
        "method": &command.method,
        "path": &command.path,
        "upstream": upstream,
        "lease_expires_at": &command.lease_expires_at,
        "request_bytes": command.body.as_ref().map(|body| body.to_string().len()),
        // 排障最常问的就是“当时发的是哪个模型版本”：直接摘出来，不用读 body 预览。
        "model_version": body_field(&command.body, "version"),
        "model_group": body_field(&command.body, "group"),
        "reason": body_field(&command.body, "reason"),
    })
}

/// 是否给这条命令落任务目录（只有状态轮询不落）。
fn should_record(command: &WparseCommand) -> bool {
    !command.method.eq_ignore_ascii_case("GET")
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
            method: method.to_string(),
            path: path.to_string(),
            body: Some(json!({"model": "v1"})),
            lease_expires_at: "2026-10-20T12:00:00Z".to_string(),
        }
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
}
