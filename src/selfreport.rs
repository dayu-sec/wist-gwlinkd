//! 消费网关容器内的**自述面**（`GatewaySelfInterface`：进程内算得准的自身状态）。
//!
//! 背景（CR-003）：中心要的是**准确**的网关状态，而准确值只有网关进程内算得出。所以容器暴露一个
//! **环回**自述面（网关侧 `GET /api/v1/gateway/self-state`，手加路由、限环回），本常驻拉取后再上报 ——
//! 网关活着拿到准值；网关不答则把「沉默」当判断。
//!
//! 字段用 **snake_case**，与网关 `self_state.rs` 的输出一致（网关其余管理面 DTO 用 camelCase，属历史分歧）。

use orion_error::prelude::*;

use crate::error::{CenterReason, CenterResult};

/// 网关自述状态（对应模型 `GatewaySelfState`）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewaySelfState {
    pub gateway_id: String,
    pub version: String,
    // 网关**对外基址**（对外域名）；老版本网关不带这个键 → `default` 兑底（`None`）。
    #[serde(default)]
    pub public_base_url: Option<String>,
    pub collected_at: wist_control::DateTime,
    pub store_healthy: bool,
    pub agent_count: i64,
    #[serde(default)]
    pub last_error: Option<String>,
    // 以下为后加的富化字段：老版本网关照常无这些键 → `default` 兜底，不因缺字段而整条拉失败。
    #[serde(default)]
    pub uptime_seconds: i64,
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub online_agents: i64,
    #[serde(default)]
    pub offline_agents: i64,
    #[serde(default)]
    pub last_seen_lag_seconds: i64,
    #[serde(default)]
    pub store_bytes: u64,
    #[serde(default)]
    pub ingest_accepted_total: u64,
    #[serde(default)]
    pub ingest_rejected_total: u64,
    #[serde(default)]
    pub last_ingest_at: Option<wist_control::DateTime>,
    #[serde(default)]
    pub memory_total_bytes: Option<u64>,
    #[serde(default)]
    pub load_1m: Option<f64>,
    #[serde(default)]
    pub load_5m: Option<f64>,
    #[serde(default)]
    pub load_15m: Option<f64>,
    #[serde(default)]
    pub disk_usage_percent: Option<f64>,
    #[serde(default)]
    pub disk_total_bytes: Option<u64>,
    #[serde(default)]
    pub disk_available_bytes: Option<u64>,
}

impl GatewaySelfState {
    /// 派生上报用健康度：存储不健康 / 有最后错误 → `degraded`。
    pub fn health(&self) -> &'static str {
        if self.store_healthy && self.last_error.is_none() {
            "ok"
        } else {
            "degraded"
        }
    }
}

/// 自述面客户端（环回）。
#[derive(Debug, Clone)]
pub struct SelfReportClient {
    endpoint: String,
    http: reqwest::Client,
}

impl SelfReportClient {
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

    /// 同 [`Self::new`]，但额外挂**信任锚**（PEM）：网关以**自签证书**提供环回 HTTPS 时必需。
    /// 缺省（`None`）= 系统根。读/解析失败即报错，**不静默回落**（否则真部署会够不到自述面）。
    pub fn with_trust(
        endpoint: impl Into<String>,
        trust: Option<&std::path::Path>,
    ) -> CenterResult<Self> {
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: crate::center::build_http_client(trust)?,
        })
    }

    /// 拉取准确自述状态：`GET /api/v1/gateway/self-state?gateway_id=`。
    pub async fn fetch(&self, gateway_id: &str) -> CenterResult<GatewaySelfState> {
        let url = format!("{}/api/v1/gateway/self-state", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .header("accept", "application/json")
            .send()
            .await
            .source_raw_err(CenterReason::Http, "self-state 请求失败")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .source_raw_err(CenterReason::Http, "读取 self-state 响应体失败")?;
        if !status.is_success() {
            return Err(CenterReason::Http.err(format!("self-state 失败（{status}）：{body}")));
        }
        serde_json::from_str(&body).map_err(|err| {
            CenterReason::Decode.err(format!("解析 self-state 响应失败: {err}；原文：{body}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 网关 `self_state.rs` 输出的**契约 fixture**（snake_case）。
    /// 网关侧有同一份 fixture 的**序列化**测试（`serializes_the_self_state_contract_keys`）——
    /// 两侧同钉一份形状，任一侧改名即在此处爆掉（防三份拷贝漂移）。
    const GATEWAY_SELF_STATE_JSON: &str = r#"{"gateway_id":"gw-1","version":"0.1.15","public_base_url":"https://gw.example.com","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":3,"last_error":null,"uptime_seconds":3600,"cpu_percent":1.5,"memory_bytes":104857600,"online_agents":2,"offline_agents":1,"last_seen_lag_seconds":30}"#;

    #[test]
    fn parses_the_gateway_self_state_contract() {
        let state: GatewaySelfState =
            serde_json::from_str(GATEWAY_SELF_STATE_JSON).expect("parse gateway contract");
        assert_eq!(state.gateway_id, "gw-1");
        assert_eq!(state.version, "0.1.15");
        assert_eq!(
            state.public_base_url.as_deref(),
            Some("https://gw.example.com")
        );
        assert_eq!(state.agent_count, 3);
        assert!(state.store_healthy);
        assert_eq!(state.health(), "ok");
    }

    /// 老网关不带 `public_base_url` 键：仍应可解，缺省为 `None`（不是解析失败）。
    #[test]
    fn parses_a_legacy_self_state_without_public_base_url() {
        let legacy = r#"{"gateway_id":"gw-1","version":"0.1.24","collected_at":"2026-10-04T00:00:00Z","store_healthy":true,"agent_count":0,"last_error":null}"#;
        let state: GatewaySelfState = serde_json::from_str(legacy).expect("老自述面应可解");
        assert_eq!(state.public_base_url, None);
    }

    #[test]
    fn degraded_when_store_unhealthy_or_error_present() {
        let unhealthy = GatewaySelfState {
            gateway_id: "gw-1".into(),
            version: "0.1.15".into(),
            public_base_url: None,
            collected_at: wist_control::DateTime::now(),
            store_healthy: false,
            agent_count: 0,
            last_error: None,
            uptime_seconds: 0,
            cpu_percent: None,
            memory_bytes: None,
            online_agents: 0,
            offline_agents: 0,
            last_seen_lag_seconds: 0,
            store_bytes: 4096,
            ingest_accepted_total: 0,
            ingest_rejected_total: 0,
            last_ingest_at: None,
            memory_total_bytes: None,
            load_1m: None,
            load_5m: None,
            load_15m: None,
            disk_usage_percent: None,
            disk_total_bytes: None,
            disk_available_bytes: None,
        };
        assert_eq!(unhealthy.health(), "degraded");
        let errored = GatewaySelfState {
            store_healthy: true,
            last_error: Some("boom".into()),
            ..unhealthy
        };
        assert_eq!(errored.health(), "degraded");
    }

    #[test]
    fn with_trust_accepts_no_ca_and_rejects_a_missing_one() {
        assert!(SelfReportClient::with_trust("https://127.0.0.1:3000", None).is_ok());
        let err = SelfReportClient::with_trust(
            "https://127.0.0.1:3000",
            Some(std::path::Path::new("/definitely/not/here.pem")),
        )
        .expect_err("missing CA must error（不静默回落）");
        assert!(err.contains("读取信任锚失败"), "{err}");
    }
}
