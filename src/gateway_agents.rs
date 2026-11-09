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

    /// 发起一次调查：`POST /api/v1/gateway/agents/{agent_id}/investigations`（环回，mTLS）。
    ///
    /// 返回网关的**原始状态码与响应体**（含 4xx）：回执要把网关的话原样带给中心，
    /// 中心不靠 gwlinkd 翻译第二遍。只有传输层失败才是 `Err`。
    pub async fn launch_investigation(
        &self,
        payload: &InvestigateAgentPayload,
    ) -> CenterResult<(u16, String)> {
        let url = format!(
            "{}/api/v1/gateway/agents/{}/investigations",
            self.endpoint,
            payload.agent_id.trim()
        );
        let response = self
            .http
            .post(url)
            .json(&payload.forward_body())
            .header("accept", "application/json")
            .send()
            .await
            .source_raw_err(CenterReason::Http, "gateway investigation 请求失败")?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .source_raw_err(CenterReason::Http, "读取 gateway investigation 响应体失败")?;
        Ok((status, body))
    }
}

/// 中心 `gateway.agent-investigate` 的 payload。
///
/// 与网关 gateway-plane 请求体同形，多一个 `agent_id`（它进 URL，不进 body）。
/// `command_id` / `agent_id` 必填：缺任何一个都**明确失败** —— 没有 id 就没有可关联的报告。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InvestigateAgentPayload {
    pub command_id: String,
    pub agent_id: String,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub top: Option<u32>,
    #[serde(default)]
    pub min_size: Option<u64>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub max_seconds: Option<u64>,
    /// 审计用来源（中心传 `center:<actor>`）；缺省由网关回落成 `center`。
    #[serde(default)]
    pub requested_by: Option<String>,
}

impl InvestigateAgentPayload {
    /// 转给网关的 body：去掉 `agent_id`（它在 URL 里），其余字段原样搬运。
    fn forward_body(&self) -> serde_json::Value {
        serde_json::json!({
            "command_id": &self.command_id,
            "topic": &self.topic,
            "path": &self.path,
            "top": self.top,
            "min_size": self.min_size,
            "depth": self.depth,
            "max_seconds": self.max_seconds,
            "requested_by": &self.requested_by,
        })
    }
}

/// `agent_id` 要拼进 URL 路径段：只允许安全字符，并拒掉 `.` / `..`。
///
/// 不清洗而是拒绝：这个值是中心给的，清洗会把一个错误变成一个「另一台机器」。
/// `..` 仍然是合法的路径段，所以单独判。
pub fn is_path_safe_agent_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
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

    /// 数据清单（采集授权 + 调查数据）随 Agent 明细透传：gwlinkd 只做搬运，不解释内容。
    #[test]
    fn parses_the_agent_data_manifest() {
        let report: ReportAgentStatus = serde_json::from_str(
            r#"{"gateway_id":"gw-1","agents":[{"version":"0.4.0","health":"healthy","instance_id":"inst-1","agent_id":"agent-1","status":"online","last_seen_at":"2026-10-04T00:00:00Z","data_manifest":{"collected_at":"2026-10-04T00:00:00Z","collection":[{"family":"HostMetrics","work_id":"w-1","status":"active","plan_version":1,"acknowledged_version":null,"collection_status":"ok","collection_detail":null,"inputs":[{"kind":"MetricInterval","target":"15s"}]}],"investigations":[]}}]}"#,
        )
        .expect("parse manifest");
        let manifest = report.agents[0].data_manifest.as_ref().expect("manifest");
        assert_eq!(manifest.collection[0].family, "HostMetrics");
        assert_eq!(manifest.collection[0].inputs[0].kind, "MetricInterval");
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

    /// 调查载荷：`agent_id` 进 URL，其余字段原样转给网关；缺 id 直接解析失败。
    #[test]
    fn investigate_payload_requires_ids_and_drops_agent_id_when_forwarded() {
        let payload: InvestigateAgentPayload = serde_json::from_str(
            r#"{"command_id":"cmd-1","agent_id":"agent-1","topic":"disk","top":10}"#,
        )
        .expect("payload");
        let body = payload.forward_body();
        assert!(
            body.get("agent_id").is_none(),
            "agent_id 在 URL 里，不该再进 body"
        );
        assert_eq!(body["command_id"], "cmd-1");
        assert_eq!(body["topic"], "disk");
        assert_eq!(body["top"], 10);
        // 中心不开放扫描路径：转给网关的 body 里它就是 null（网关拒绝非空值）。
        assert!(body["path"].is_null());
        assert!(body["requested_by"].is_null());

        assert!(
            serde_json::from_str::<InvestigateAgentPayload>(r#"{"agent_id":"a","topic":"disk"}"#)
                .is_err(),
            "缺 command_id 必须拒绝"
        );
        assert!(
            serde_json::from_str::<InvestigateAgentPayload>(r#"{"command_id":"c","topic":"disk"}"#)
                .is_err(),
            "缺 agent_id 必须拒绝"
        );
    }

    /// `agent_id` 会拼进 URL：`..` / 分隔符一律不是合法段。
    #[test]
    fn agent_ids_must_be_a_safe_path_segment() {
        assert!(is_path_safe_agent_id("agent-host-a55a338d7723"));
        assert!(is_path_safe_agent_id("agent_1.2:3"));
        assert!(!is_path_safe_agent_id("."));
        assert!(!is_path_safe_agent_id(".."));
        assert!(!is_path_safe_agent_id("a/b"));
        assert!(!is_path_safe_agent_id("a b"));
        assert!(!is_path_safe_agent_id(""));
        assert!(!is_path_safe_agent_id(&"x".repeat(129)));
    }

    #[tokio::test]
    async fn launch_investigation_posts_to_the_agent_scoped_route() {
        let (endpoint, captured) = one_shot_capturing(
            "202 Accepted",
            r#"{"agent_id":"agent-1","command_id":"cmd-1","topic":"disk","requested_by":"center"}"#,
        )
        .await;
        let client = GatewayAgentsClient::new(endpoint);
        let payload: InvestigateAgentPayload =
            serde_json::from_str(r#"{"command_id":"cmd-1","agent_id":"agent-1","topic":"disk"}"#)
                .expect("payload");
        let (status, body) = client
            .launch_investigation(&payload)
            .await
            .expect("http response");
        assert_eq!(status, 202);
        assert!(body.contains("cmd-1"));

        let request = captured.await.expect("request captured");
        assert!(
            request.starts_with("POST /api/v1/gateway/agents/agent-1/investigations "),
            "{request}"
        );
    }

    /// 4xx 不吞：原状态码 + 原文回给调用方（回执靠它把网关的话带给中心）。
    #[tokio::test]
    async fn launch_investigation_surfaces_4xx_verbatim() {
        let (endpoint, _captured) = one_shot_capturing(
            "409 Conflict",
            r#"{"error":{"code":"investigation_in_flight","message":"still running"}}"#,
        )
        .await;
        let client = GatewayAgentsClient::new(endpoint);
        let payload: InvestigateAgentPayload =
            serde_json::from_str(r#"{"command_id":"cmd-2","agent_id":"agent-1"}"#)
                .expect("payload");
        let (status, body) = client
            .launch_investigation(&payload)
            .await
            .expect("4xx 仍是正常 HTTP 响应");
        assert_eq!(status, 409);
        assert!(body.contains("investigation_in_flight"), "{body}");
    }

    /// 机队上报里的 `investigations` 也要能解（契约 0.21 起）：gwlinkd 只搬运。
    #[test]
    fn parses_the_investigation_task_status() {
        let report: ReportAgentStatus = serde_json::from_str(
            r#"{"gateway_id":"gw-1","agents":[{"version":"0.4.0","health":"healthy","instance_id":"inst-1","agent_id":"agent-1","status":"online","last_seen_at":"2026-10-04T00:00:00Z","investigations":[{"command_id":"cmd-1","topic":"disk","requested_at":"2026-10-04T00:00:00Z","claimed_at":null,"acked_at":null,"outcome":null,"detail":null}]}]}"#,
        )
        .expect("parse investigations");
        let tasks = &report.agents[0].investigations;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].command_id, "cmd-1");
        assert_eq!(tasks[0].topic, "disk");
        assert!(tasks[0].outcome.is_none(), "未回执时 outcome 必须是 None");
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
