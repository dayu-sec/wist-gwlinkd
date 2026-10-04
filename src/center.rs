//! 连 `WistCenter` 的客户端：link-upstream / register / status / credentials:renew / upgrade-plan / upgrade-result。
//!
//! 本进程是**运行期凭据 `rt_` 的唯一持有者** —— 全边缘只此一处与中心对话。wire 类型来自
//! `wist-control`（由 `wist-design/jumo` 模型生成）。

use std::path::Path;
use std::time::Duration;

use wist_control::{
    GatewayCredentialBundle, GatewayEnrollmentResult, GatewayInitialConfig, GatewayUpgradePlan,
    RegisterGateway, ReportGatewayStatus,
};

/// 单次请求超时（避免中心/网关半死把常驻循环卡住）。
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// 中心调用的错误：**区分「凭据被拒（401/403）」与其它** —— 前者要退避 + 提示重置备，
/// 不能靠匹配错误串子串（响应体里也可能出现 "401"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CenterError {
    /// 401 / 403：凭据被拒。
    Unauthorized(String),
    /// 网络 / 5xx / 解析等其它错误。
    Other(String),
}

impl CenterError {
    /// 是否凭据被拒。
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Unauthorized(_))
    }
}

impl std::fmt::Display for CenterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized(message) => write!(f, "凭据被拒：{message}"),
            Self::Other(message) => write!(f, "{message}"),
        }
    }
}

/// 建 HTTP 客户端：带超时；给了 `trust_bundle`（PEM）则作为**自定义信任锚**（自签中心证书时必需）。
pub fn build_http_client(trust_bundle: Option<&Path>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(HTTP_TIMEOUT);
    if let Some(path) = trust_bundle {
        let pem = std::fs::read(path)
            .map_err(|err| format!("读取信任锚失败 {}: {err}", path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|err| format!("解析信任锚失败 {}: {err}", path.display()))?;
        builder = builder.add_root_certificate(cert);
    }
    builder
        .build()
        .map_err(|err| format!("建 HTTP 客户端失败: {err}"))
}

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
    ///
    /// **不**接自定义信任锚；需要自签锚时用 [`build_http_client`] + [`CenterClient::with_client`]。
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// 以既有 HTTP 客户端建（供注入超时 / 信任锚）。
    pub fn with_client(endpoint: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http,
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
    ) -> Result<LinkUpstreamReturned, CenterError> {
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
            .map_err(|err| CenterError::Other(format!("link-upstream 请求失败: {err}")))?;
        decode(response, "link-upstream").await
    }

    /// 注册：`POST /api/v1/gateway/register`（消费 RegistToken，签发运行期凭据）。
    pub async fn register(
        &self,
        enrollment_token: &str,
        instance_id: &str,
    ) -> Result<GatewayEnrollmentResult, CenterError> {
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
            .map_err(|err| CenterError::Other(format!("register 请求失败: {err}")))?;
        decode(response, "register").await
    }

    /// 周期上报：`POST /api/v1/gateway/status`（`Bearer <rt_>`）。
    pub async fn report_status(
        &self,
        credential: &GatewayCredentialBundle,
        payload: &ReportGatewayStatus,
    ) -> Result<(), CenterError> {
        let url = format!("{}/api/v1/gateway/status", self.endpoint);
        let response = self
            .http
            .post(url)
            .bearer_auth(&credential.bearer_token)
            .json(payload)
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("status 请求失败: {err}")))?;
        let _: serde_json::Value = decode(response, "status").await?;
        Ok(())
    }

    /// 续期：`POST /api/v1/gateway/credentials:renew`（旧凭据立即失效）。
    pub async fn renew_credential(
        &self,
        credential: &GatewayCredentialBundle,
    ) -> Result<GatewayCredentialBundle, CenterError> {
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
            .map_err(|err| CenterError::Other(format!("renew 请求失败: {err}")))?;
        decode(response, "renew").await
    }

    /// 取升级目标：`GET /api/v1/gateway/upgrade-plan?gateway_id=`（`Bearer <rt_>`）。
    pub async fn get_upgrade_plan(
        &self,
        credential: &GatewayCredentialBundle,
        gateway_id: &str,
    ) -> Result<GatewayUpgradePlan, CenterError> {
        let url = format!("{}/api/v1/gateway/upgrade-plan", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .bearer_auth(&credential.bearer_token)
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("upgrade-plan 请求失败: {err}")))?;
        decode(response, "upgrade-plan").await
    }

    /// 升级回执：`POST /api/v1/gateway/upgrade-result`（字段对齐 `upgrade.json`）。
    pub async fn report_upgrade_result(
        &self,
        credential: &GatewayCredentialBundle,
        record: &crate::state::UpgradeRecord,
    ) -> Result<(), CenterError> {
        let url = format!("{}/api/v1/gateway/upgrade-result", self.endpoint);
        let response = self
            .http
            .post(url)
            .bearer_auth(&credential.bearer_token)
            .json(record)
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("upgrade-result 请求失败: {err}")))?;
        let _: serde_json::Value = decode(response, "upgrade-result").await?;
        Ok(())
    }
}

/// 解码响应：401/403 → [`CenterError::Unauthorized`]；其它非 2xx / 解析失败 → [`CenterError::Other`]。
async fn decode<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    what: &str,
) -> Result<T, CenterError> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| CenterError::Other(format!("读取 {what} 响应体失败: {err}")))?;
    if !status.is_success() {
        let message = format!("{what} 失败（{status}）：{body}");
        return Err(
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                CenterError::Unauthorized(message)
            } else {
                CenterError::Other(message)
            },
        );
    }
    serde_json::from_str(&body)
        .map_err(|err| CenterError::Other(format!("解析 {what} 响应失败: {err}；原文：{body}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 起一个一次性 HTTP 服务器，回固定响应；返回其 `http://127.0.0.1:<port>`。
    async fn one_shot_server(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0_u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        format!("http://{addr}")
    }

    fn credential() -> GatewayCredentialBundle {
        GatewayCredentialBundle {
            credential_id: "cred-1".into(),
            gateway_id: "gw-1".into(),
            instance_id: "gw-1/boot-1".into(),
            auth_scheme: "bearer".into(),
            bearer_token: "rt_abc".into(),
            issued_at: wist_control::DateTime::now(),
            expires_at: wist_control::DateTime::now(),
        }
    }

    #[tokio::test]
    async fn upgrade_plan_parses_a_success_response() {
        let endpoint =
            one_shot_server("200 OK", r#"{"gateway_id":"gw-1","has_plan":false,"plan_id":null,"component":null,"to_version":null}"#)
                .await;
        let client = CenterClient::new(endpoint);
        let plan = client
            .get_upgrade_plan(&credential(), "gw-1")
            .await
            .expect("plan");
        assert!(!plan.has_plan);
        assert_eq!(plan.gateway_id, "gw-1");
    }

    #[tokio::test]
    async fn a_401_is_classified_as_unauthorized_not_just_other() {
        // 响应体刻意含 "401" —— 旧子串匹配会误判，类型化后不会。
        let endpoint =
            one_shot_server("401 Unauthorized", r#"{"error":"invalid rt_401 token"}"#).await;
        let client = CenterClient::new(endpoint);
        let err = client
            .get_upgrade_plan(&credential(), "gw-1")
            .await
            .expect_err("must fail");
        assert!(err.is_unauthorized(), "{err}");
    }

    #[tokio::test]
    async fn a_500_is_not_unauthorized() {
        let endpoint = one_shot_server("500 Internal Server Error", r#"{"error":"boom"}"#).await;
        let client = CenterClient::new(endpoint);
        let err = client
            .report_status(&credential(), &status_payload())
            .await
            .expect_err("must fail");
        assert!(!err.is_unauthorized(), "{err}");
    }

    fn status_payload() -> ReportGatewayStatus {
        ReportGatewayStatus {
            gateway_id: "gw-1".into(),
            instance_id: "gw-1/boot-1".into(),
            version: "0.1.0".into(),
            status: "running".into(),
            health: "ok".into(),
            memory_bytes: None,
            cpu_percent: None,
            reported_at: wist_control::DateTime::now(),
        }
    }
}
