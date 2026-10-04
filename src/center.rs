//! 连 `WistCenter` 的客户端：link-upstream / register / status / credentials:renew。
//!
//! 本进程是**运行期凭据 `rt_` 的唯一持有者** —— 全边缘只此一处与中心对话。wire 类型来自
//! `wist-control`（由 `wist-design/jumo` 模型生成）。

use wist_control::{
    GatewayCredentialBundle, GatewayEnrollmentResult, GatewayInitialConfig, RegisterGateway,
    ReportGatewayStatus,
};

/// link-upstream 的响应壳（中心侧 `InitialConfigReturned`）：配置 + 置备态一次性 RegistToken。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LinkUpstreamReturned {
    pub config: GatewayInitialConfig,
    /// 置备路径：RegistToken 明文；已初始化路径：`None`。
    pub regist_token: Option<String>,
}

/// 中心客户端。
#[derive(Debug, Clone)]
pub struct CenterClient {
    endpoint: String,
    http: reqwest::Client,
}

impl CenterClient {
    /// 以控制中心 endpoint 建客户端（`endpoint` 形如 `https://center.example`）。
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// 控制中心 endpoint（无尾斜杠）。
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 链接上级：`GET /api/v1/gateway/link-upstream?gateway_id=`。
    ///
    /// - 未初始化：`Bearer <bootstrap>` + `X-Gateway-Identity-Token: <ident_>`；
    /// - 已初始化：`Bearer <rt_>`（可不带身份头）。
    pub async fn link_upstream(
        &self,
        gateway_id: &str,
        bearer: &str,
        identity_token: Option<&str>,
    ) -> Result<LinkUpstreamReturned, String> {
        let url = format!("{}/api/v1/gateway/link-upstream", self.endpoint);
        let mut request = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .bearer_auth(bearer)
            .header("accept", "application/json");
        if let Some(identity) = identity_token {
            request = request.header("X-Gateway-Identity-Token", identity);
        }
        let response = request
            .send()
            .await
            .map_err(|err| format!("link-upstream 请求失败: {err}"))?;
        decode(response, "link-upstream").await
    }

    /// 注册：`POST /api/v1/gateway/register`（消费 RegistToken，签发运行期凭据）。
    pub async fn register(
        &self,
        enrollment_token: &str,
        instance_id: &str,
    ) -> Result<GatewayEnrollmentResult, String> {
        let url = format!("{}/api/v1/gateway/register", self.endpoint);
        let payload = RegisterGateway {
            enrollment_token: enrollment_token.to_string(),
            instance_id: instance_id.to_string(),
            requested_at: wist_control::DateTime::now(),
        };
        let response = self
            .http
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|err| format!("register 请求失败: {err}"))?;
        decode(response, "register").await
    }

    /// 周期上报：`POST /api/v1/gateway/status`（`Bearer <rt_>`）。
    pub async fn report_status(
        &self,
        credential: &GatewayCredentialBundle,
        payload: &ReportGatewayStatus,
    ) -> Result<(), String> {
        let url = format!("{}/api/v1/gateway/status", self.endpoint);
        let response = self
            .http
            .post(url)
            .bearer_auth(&credential.bearer_token)
            .json(payload)
            .send()
            .await
            .map_err(|err| format!("status 请求失败: {err}"))?;
        let _: serde_json::Value = decode(response, "status").await?;
        Ok(())
    }

    /// 续期：`POST /api/v1/gateway/credentials:renew`（旧凭据立即失效）。
    pub async fn renew_credential(
        &self,
        credential: &GatewayCredentialBundle,
    ) -> Result<GatewayCredentialBundle, String> {
        let url = format!("{}/api/v1/gateway/credentials:renew", self.endpoint);
        let payload = serde_json::json!({
            "gateway_id": credential.gateway_id,
            "instance_id": credential.instance_id,
        });
        let response = self
            .http
            .post(url)
            .bearer_auth(&credential.bearer_token)
            .json(&payload)
            .send()
            .await
            .map_err(|err| format!("renew 请求失败: {err}"))?;
        decode(response, "renew").await
    }
}

/// 解码响应：非 2xx → 带状态码与响应体的错误；2xx → 反序列化。
async fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    what: &str,
) -> Result<T, String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("读取 {what} 响应体失败: {err}"))?;
    if !status.is_success() {
        return Err(format!("{what} 失败（{status}）：{body}"));
    }
    serde_json::from_str(&body).map_err(|err| format!("解析 {what} 响应失败: {err}；原文：{body}"))
}
