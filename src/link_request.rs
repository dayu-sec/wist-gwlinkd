//! 网关侧「接入请求」通道（页面发起接入；CR-003）。
//!
//! 运维在网关「链接上级」页提交接入物（中心地址 + 一次性接入券 + CA-S），落在网关容器；
//! 本常驻通过**环回**接口拉取并完成 link-upstream / register，再回报结果。这样接入由网关侧发起
//! （私钥在本机生成），而 gwlinkd 仍**纯出站**（无入站服务）。
//!
//! 见设计 `wist-design/doc/design/edge/gateway-onboard-request.md`；网关侧对应 `api/link_request.rs`。
//! 字段 snake_case（两侧同钉一份 fixture，任一侧改名即爆）。

/// 环回拉取到的接入请求（对应网关 `GatewayLinkRequest`，snake_case）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayLinkRequest {
    pub has_request: bool,
    #[serde(default)]
    pub gateway_id: String,
    #[serde(default)]
    pub center_endpoint: String,
    #[serde(default)]
    pub link_token: String,
    #[serde(default)]
    pub trust_bundle_pem: String,
    #[serde(default)]
    pub status: String,
}

/// 接入请求通道客户端（环回，形如 `https://127.0.0.1:3000`）。
#[derive(Debug, Clone)]
pub struct LinkRequestClient {
    endpoint: String,
    http: reqwest::Client,
}

impl LinkRequestClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(crate::center::HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// 拉取待办接入请求：`GET /api/v1/gateway/link-request?gateway_id=`。
    pub async fn fetch(&self, gateway_id: &str) -> Result<GatewayLinkRequest, String> {
        let url = format!("{}/api/v1/gateway/link-request", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|err| format!("link-request 请求失败: {err}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("读取 link-request 响应体失败: {err}"))?;
        if !status.is_success() {
            return Err(format!("link-request 失败（{status}）：{body}"));
        }
        serde_json::from_str(&body)
            .map_err(|err| format!("解析 link-request 响应失败: {err}；原文：{body}"))
    }

    /// 回报接入结果：`POST /api/v1/gateway/link-result`。`status` = `Connected` | `Failed`。
    pub async fn report_result(
        &self,
        gateway_id: &str,
        status: &str,
        detail: &str,
    ) -> Result<(), String> {
        let url = format!("{}/api/v1/gateway/link-result", self.endpoint);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "gateway_id": gateway_id,
                "status": status,
                "detail": detail,
            }))
            .send()
            .await
            .map_err(|err| format!("link-result 请求失败: {err}"))?;
        let code = response.status();
        let body = response.text().await.unwrap_or_default();
        if !code.is_success() {
            return Err(format!("link-result 失败（{code}）：{body}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 网关 `api/link_request.rs` 输出的**契约 fixture**（snake_case）。
    /// 网关侧有对应测试（`gateway_link_request_flow_round_trips`）钉同一形状 —— 任一侧改名即爆。
    const GATEWAY_LINK_REQUEST_JSON: &str = r#"{"has_request":true,"gateway_id":"gw-1","center_endpoint":"https://center.example","link_token":"link_abc","trust_bundle_pem":"-----BEGIN CERTIFICATE-----\n","status":"Pending"}"#;

    #[test]
    fn parses_the_gateway_link_request_contract() {
        let request: GatewayLinkRequest =
            serde_json::from_str(GATEWAY_LINK_REQUEST_JSON).expect("parse gateway contract");
        assert!(request.has_request);
        assert_eq!(request.gateway_id, "gw-1");
        assert_eq!(request.center_endpoint, "https://center.example");
        assert_eq!(request.link_token, "link_abc");
        assert_eq!(request.status, "Pending");
    }

    #[test]
    fn parses_an_empty_link_request() {
        let request: GatewayLinkRequest = serde_json::from_str(
            r#"{"has_request":false,"gateway_id":"","center_endpoint":"","link_token":"","trust_bundle_pem":"","status":""}"#,
        )
        .expect("parse");
        assert!(!request.has_request);
    }
}
