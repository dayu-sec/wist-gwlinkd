//! 消费网关容器内的**自述面**（`GatewaySelfInterface`：进程内算得准的自身状态）。
//!
//! 背景（CR-003）：中心要的是**准确**的网关状态，而准确值只有网关进程内算得出。所以容器暴露一个
//! **环回**自述面（网关侧 `GET /api/v1/gateway/self-state`，手加路由、限环回），本常驻拉取后再上报 ——
//! 网关活着拿到准值；网关不答则把「沉默」当判断。
//!
//! 字段用 **snake_case**，与网关 `self_state.rs` 的输出一致（网关其余管理面 DTO 用 camelCase，属历史分歧）。

/// 网关自述状态（对应模型 `GatewaySelfState`）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewaySelfState {
    pub gateway_id: String,
    pub version: String,
    pub collected_at: wist_control::DateTime,
    pub store_healthy: bool,
    pub agent_count: i64,
    pub uplink_enabled: bool,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl GatewaySelfState {
    /// 派生上报用健康度：存储不健康 / 上送未启用 / 有最后错误 → `degraded`。
    pub fn health(&self) -> &'static str {
        if self.store_healthy && self.uplink_enabled && self.last_error.is_none() {
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

    /// 拉取准确自述状态：`GET /api/v1/gateway/self-state?gateway_id=`。
    pub async fn fetch(&self, gateway_id: &str) -> Result<GatewaySelfState, String> {
        let url = format!("{}/api/v1/gateway/self-state", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|err| format!("self-state 请求失败: {err}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("读取 self-state 响应体失败: {err}"))?;
        if !status.is_success() {
            return Err(format!("self-state 失败（{status}）：{body}"));
        }
        serde_json::from_str(&body)
            .map_err(|err| format!("解析 self-state 响应失败: {err}；原文：{body}"))
    }
}
