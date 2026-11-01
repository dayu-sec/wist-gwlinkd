//! 环回拉取网关的「Agent 明细」（机队运行态），供 gwlinkd 转报中心。
//!
//! 背景（CR-003 的延伸）：中心「网关详情 → Agent 状态」页要看到**每台 agent 的运行态**，而中心
//! 连不到 NAT 后的网关。故网关以**环回**面暴露机队明细（`GET /api/v1/gateway/agents`），本常驻拉取
//! 后以 mTLS 转报中心 `POST /api/v1/gateway/agents/status`（契约 `wist_control::ReportAgentStatus`）。
//! 与 self-state / link-request / linkd-status / agent-package 同一形态：gwlinkd 仍**纯出站**，
//! 本通道是它出站轮询的一站。
//!
//! 字段用 **snake_case**，与网关环回面输出、契约 `AgentRuntimeStatus` 一致。
//! 网关侧对应 `wist-gateway/src/api/gateway_agents.rs`。

use orion_error::prelude::*;
use wist_control::{AgentRuntimeStatus, ReportAgentStatus};

use crate::error::{CenterReason, CenterResult};

/// 环回读取器（形如 `https://127.0.0.1:3000`，与 self / link-request 面同一 endpoint + 信任锚）。
#[derive(Debug, Clone)]
pub struct GatewayAgentsClient {
    endpoint: String,
    http: reqwest::Client,
}

impl GatewayAgentsClient {
    /// 以自述面 endpoint 建客户端（`endpoint` 形如 `https://127.0.0.1:3000`）。
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(crate::center::HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// 同 [`Self::new`]，但额外挂**信任锚**（PEM）：网关以自签证书提供环回 HTTPS 时必需。
    /// 缺省（`None`）= 系统根。读 / 解析失败即报错，**不静默回落**。
    pub fn with_trust(
        endpoint: impl Into<String>,
        trust: Option<&std::path::Path>,
    ) -> CenterResult<Self> {
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: crate::center::build_http_client(trust)?,
        })
    }

    /// 拉取机队明细：`GET /api/v1/gateway/agents?gateway_id=`（环回，loopback-only）。
    ///
    /// 返回[`AgentRuntimeStatus`] 列表（上报契约的一份）；`gateway_id` 由调用方以自身配置覆写后再上报
    /// —— 环回面回显的是调用方给的值，不以它为准，避免网关侧 misconfig 时张冠李戴。
    pub async fn fetch(&self, gateway_id: &str) -> CenterResult<Vec<AgentRuntimeStatus>> {
        let url = format!("{}/api/v1/gateway/agents", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .header("accept", "application/json")
            .send()
            .await
            .source_raw_err(CenterReason::Http, "gateway agents 请求失败")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .source_raw_err(CenterReason::Http, "读取 gateway agents 响应体失败")?;
        if !status.is_success() {
            return Err(CenterReason::Http.err(format!("gateway agents 失败（{status}）：{body}")));
        }
        // 环回响应与上报契约**同形**（`gateway_id` + `agents`），故直接按契约解析；只取 `agents`。
        let report: ReportAgentStatus = serde_json::from_str(&body).map_err(|err| {
            CenterReason::Decode.err(format!("解析 gateway agents 响应失败: {err}；原文：{body}"))
        })?;
        Ok(report.agents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 一次性 HTTP 服务器：把收到的原始请求回传，并回固定响应体。
    async fn one_shot_capturing(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0_u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// 网关环回面输出的**契约 fixture**（snake_case）：gwlinkd 侧钉住**解析**。
    /// 网关侧有同一形状的**序列化**测试（`serializes_the_gateway_agents_contract_keys`）——
    /// 任一侧改名即在此处爆掉（防拷贝漂移）。
    const GATEWAY_AGENTS_JSON: &str = r#"{"gateway_id":"gw-1","agents":[{"version":"0.4.0","health":"healthy","instance_id":"inst-1","agent_id":"agent-1","status":"online","last_seen_at":"2026-10-04T00:00:00Z","memory_bytes":104857600,"cpu_percent":1.5,"admin_latency_ms":8}]}"#;

    #[test]
    fn parses_the_gateway_agents_contract() {
        let report: ReportAgentStatus =
            serde_json::from_str(GATEWAY_AGENTS_JSON).expect("parse gateway contract");
        assert_eq!(report.gateway_id, "gw-1");
        assert_eq!(report.agents.len(), 1);
        let agent = &report.agents[0];
        assert_eq!(agent.agent_id, "agent-1");
        assert_eq!(agent.status, "online");
        assert_eq!(agent.memory_bytes, Some(104_857_600));
    }

    /// 老 / 空机队：`agents` 为空数组仍应可解（不是解析失败）。
    #[test]
    fn parses_an_empty_fleet() {
        let report: ReportAgentStatus =
            serde_json::from_str(r#"{"gateway_id":"gw-1","agents":[]}"#).expect("empty fleet");
        assert!(report.agents.is_empty());
    }

    #[tokio::test]
    async fn fetch_gets_the_loopback_endpoint_and_returns_agents() {
        let (endpoint, captured) = one_shot_capturing("200 OK", GATEWAY_AGENTS_JSON).await;
        let client = GatewayAgentsClient::new(endpoint);
        let agents = client.fetch("gw-1").await.expect("fetch ok");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].agent_id, "agent-1");

        let request = captured.await.expect("request captured");
        assert!(
            request.starts_with("GET /api/v1/gateway/agents?gateway_id=gw-1 "),
            "{request}"
        );
    }

    /// 非 2xx → `Err`（含响应体，便于诊断）。
    #[tokio::test]
    async fn a_non_success_response_is_an_error() {
        let (endpoint, _captured) =
            one_shot_capturing("403 Forbidden", "gateway agents is loopback-only").await;
        let client = GatewayAgentsClient::new(endpoint);
        let err = client.fetch("gw-1").await.expect_err("must fail");
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("loopback-only"), "{err}");
    }

    #[test]
    fn with_trust_rejects_a_missing_ca() {
        let err = GatewayAgentsClient::with_trust(
            "https://127.0.0.1:3000",
            Some(std::path::Path::new("/definitely/not/here.pem")),
        )
        .expect_err("missing CA must error（不静默回落）");
        assert!(err.contains("读取信任锚失败"), "{err}");
    }
}
