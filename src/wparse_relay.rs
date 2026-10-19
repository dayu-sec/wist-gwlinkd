//! `[wparse_relay]`：规则发布命令中继。
//!
//! 链路：Center 命令队列 --mTLS 短轮询--> gwlinkd --HTTPS(本机 admin token)--> wparse。
//! Gateway 不开放入站端口；Center 也不能直连 wparse（管理面只发布在宿主回环、
//! 且证书由 Gateway CA 签发）。
//!
//! 双端白名单：Center 只接受两个 admin 路径；gwlinkd 也独立校验，不把 Center 当可信来源。

use std::path::Path;
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use crate::center::{CenterClient, WparseCommand, build_mtls_http_client};
use crate::config::Config;
use crate::state;

const STATUS_PATH: &str = "/admin/v1/runtime/status";
const RELOAD_PATH: &str = "/admin/v1/reloads/model";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

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

        let result = execute(&http, &token_file, &upstream, &allowed, &command).await;
        if let Err(err) = client
            .report_wparse_command_result(
                &command.command_id,
                result.status_code,
                &result.body,
                result.error.as_deref(),
            )
            .await
        {
            log::warn!(
                "event=WparseRelayResultFailed command_id={} error={}",
                command.command_id,
                err
            );
        }
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
) -> UpstreamResult {
    let path = command.path.as_str();
    if !is_allowed(&command.method, path, allowed) {
        return UpstreamResult {
            status_code: 403,
            body: String::new(),
            error: Some(format!("path not allowed: {} {}", command.method, path)),
        };
    }

    let token = match std::fs::read_to_string(token_file) {
        Ok(token) => token.trim().to_string(),
        Err(err) => {
            return UpstreamResult {
                status_code: 502,
                body: String::new(),
                error: Some(format!("read admin token {}: {err}", token_file.display())),
            };
        }
    };
    if token.is_empty() {
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
    let request = match (&command.method[..], &command.body) {
        ("POST", Some(body)) => request.json(body),
        _ => request,
    };

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let body = truncate(text);
            UpstreamResult {
                status_code: status,
                body,
                error: None,
            }
        }
        Err(err) => UpstreamResult {
            status_code: 502,
            body: String::new(),
            error: Some(format!("wparse admin request failed: {err}")),
        },
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
}
