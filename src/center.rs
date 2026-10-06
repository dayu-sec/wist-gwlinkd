//! 连 `WistCenter` 的客户端：link-upstream / register / status / credentials:renew / upgrade-plan / upgrade-result。
//!
//! 本进程是**网关客户端证书/私钥的唯一持有者** —— 全边缘只此一处与中心对话。长期身份走 **mTLS**
//! （客户端证书），取代旧的对称 bearer `rt_`；wire 类型全部来自 `wist-control`（模型生成）。

use std::path::Path;
use std::time::Duration;

use wist_control::{
    DateTime, GatewayCredentialBundle, GatewayEnrollmentResult, GatewayInitialConfig,
    GatewayUpgradePlan, RegisterGateway, RenewGatewayCredential, ReportGatewayStatus,
    ReportGatewayUpgradeResult,
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

/// 建 **mTLS** HTTP 客户端：信任锚同上，另挂网关客户端证书/私钥（`identity_pem` = 证书 + 私钥 PEM）。
///
/// 已注册的网关面调用（status / renew / upgrade-plan / upgrade-result）都用这个客户端：
/// 中心在握手期验客户端证书（CA-G）后认人。
pub fn build_mtls_http_client(
    trust_bundle: Option<&Path>,
    identity_pem: &str,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().timeout(HTTP_TIMEOUT);
    if let Some(path) = trust_bundle {
        let pem = std::fs::read(path)
            .map_err(|err| format!("读取信任锚失败 {}: {err}", path.display()))?;
        let cert = reqwest::Certificate::from_pem(&pem)
            .map_err(|err| format!("解析信任锚失败 {}: {err}", path.display()))?;
        builder = builder.add_root_certificate(cert);
    }
    let identity = reqwest::Identity::from_pem(identity_pem.as_bytes())
        .map_err(|err| format!("加载客户端证书/私钥失败: {err}"))?;
    builder = builder.identity(identity);
    builder
        .build()
        .map_err(|err| format!("建 mTLS HTTP 客户端失败: {err}"))
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
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
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
    /// **只用于首跑置备**（本地还没有客户端证书时）：`Bearer <link>` +
    /// `X-Gateway-Identity-Token: <ident_>`。已注册后不再调此端点（直接走 mTLS）。
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

    /// 注册：`POST /api/v1/gateway/register`（消费 RegistToken；中心用 CA-G 按 CSR 签客户端证书）。
    ///
    /// `public_base_url` = 网关**对外域名**（由自述面取，见 [`crate::selfreport`]）；取不到传 `None`
    /// （首次注册时网关可能还没起来）—— 中心容忍缺该字段。
    pub async fn register(
        &self,
        enrollment_token: &str,
        instance_id: &str,
        certificate_signing_request: &str,
        public_base_url: Option<&str>,
    ) -> Result<GatewayEnrollmentResult, CenterError> {
        let url = format!("{}/api/v1/gateway/register", self.endpoint);
        let payload = RegisterGateway {
            enrollment_token: enrollment_token.to_string(),
            instance_id: instance_id.to_string(),
            public_base_url: public_base_url.map(str::to_string),
            certificate_signing_request: certificate_signing_request.to_string(),
            requested_at: DateTime::now(),
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

    /// 周期上报：`POST /api/v1/gateway/status`（客户端证书 mTLS 鉴权，身份在客户端里）。
    pub async fn report_status(&self, payload: &ReportGatewayStatus) -> Result<(), CenterError> {
        let url = format!("{}/api/v1/gateway/status", self.endpoint);
        let response = self
            .http
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("status 请求失败: {err}")))?;
        let _: serde_json::Value = decode(response, "status").await?;
        Ok(())
    }

    /// 轮换：`POST /api/v1/gateway/credentials:renew`（以当前证书证明身份 + 新 CSR → 新证书；旧证书作废）。
    pub async fn renew_credential(
        &self,
        gateway_id: &str,
        current_certificate_serial: &str,
        certificate_signing_request: &str,
    ) -> Result<GatewayCredentialBundle, CenterError> {
        let url = format!("{}/api/v1/gateway/credentials:renew", self.endpoint);
        let payload = RenewGatewayCredential {
            gateway_id: gateway_id.to_string(),
            current_certificate_serial: current_certificate_serial.to_string(),
            certificate_signing_request: certificate_signing_request.to_string(),
            requested_at: DateTime::now(),
        };
        let response = self
            .http
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("renew 请求失败: {err}")))?;
        decode(response, "renew").await
    }

    /// 取升级目标：`GET /api/v1/gateway/upgrade-plan?gateway_id=`（客户端证书 mTLS 鉴权）。
    pub async fn get_upgrade_plan(
        &self,
        gateway_id: &str,
    ) -> Result<GatewayUpgradePlan, CenterError> {
        let url = format!("{}/api/v1/gateway/upgrade-plan", self.endpoint);
        let response = self
            .http
            .get(url)
            .query(&[("gateway_id", gateway_id)])
            .send()
            .await
            .map_err(|err| CenterError::Other(format!("upgrade-plan 请求失败: {err}")))?;
        decode(response, "upgrade-plan").await
    }

    /// 升级回执：`POST /api/v1/gateway/upgrade-result`（客户端证书 mTLS 鉴权）。
    ///
    /// body 是中心侧 `ReportGatewayUpgradeResult`（**含必填 `gateway_id` / `reported_at`**，
    /// 不能直接发本地 `UpgradeRecord` —— 否则中心反序列化失败）。
    pub async fn report_upgrade_result(
        &self,
        gateway_id: &str,
        record: &crate::state::UpgradeRecord,
    ) -> Result<(), CenterError> {
        let url = format!("{}/api/v1/gateway/upgrade-result", self.endpoint);
        let payload = ReportGatewayUpgradeResult {
            gateway_id: gateway_id.to_string(),
            work_id: record.work_id.clone(),
            from_version: record.from_version.clone(),
            to_version: record.to_version.clone(),
            step: record.step.clone(),
            status: record.status.clone(),
            detail: record.detail.clone(),
            reported_at: wist_control::DateTime::now(),
        };
        let response = self
            .http
            .post(url)
            .json(&payload)
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

    #[tokio::test]
    async fn upgrade_plan_parses_a_success_response() {
        let endpoint =
            one_shot_server("200 OK", r#"{"gateway_id":"gw-1","has_plan":false,"plan_id":null,"component":null,"to_version":null}"#)
                .await;
        let client = CenterClient::new(endpoint);
        let plan = client.get_upgrade_plan("gw-1").await.expect("plan");
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
            .get_upgrade_plan("gw-1")
            .await
            .expect_err("must fail");
        assert!(err.is_unauthorized(), "{err}");
    }

    #[tokio::test]
    async fn a_500_is_not_unauthorized() {
        let endpoint = one_shot_server("500 Internal Server Error", r#"{"error":"boom"}"#).await;
        let client = CenterClient::new(endpoint);
        let err = client
            .report_status(&status_payload())
            .await
            .expect_err("must fail");
        assert!(!err.is_unauthorized(), "{err}");
    }

    #[test]
    fn build_mtls_http_client_rejects_a_bad_identity() {
        assert!(build_mtls_http_client(None, "not a pem").is_err());
    }

    #[test]
    fn build_mtls_http_client_accepts_a_real_certificate_and_key() {
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = rcgen::CertificateParams::default()
            .self_signed(&key)
            .expect("cert");
        let pem = format!("{}\n{}", cert.pem(), key.serialize_pem());
        assert!(build_mtls_http_client(None, &pem).is_ok());
    }

    fn status_payload() -> ReportGatewayStatus {
        ReportGatewayStatus {
            gateway_id: "gw-1".into(),
            instance_id: "gw-1/boot-1".into(),
            public_base_url: Some("https://gw.example.com".into()),
            version: "0.1.15".into(),
            status: "online".into(),
            health: "ok".into(),
            memory_bytes: None,
            cpu_percent: None,
            reported_at: wist_control::DateTime::now(),
            uptime_seconds: None,
            agent_count: None,
            online_agents: None,
            offline_agents: None,
            last_seen_lag_seconds: None,
            store_bytes: None,
            ingest_accepted_total: None,
            ingest_rejected_total: None,
            last_ingest_at: None,
            memory_total_bytes: None,
            load_1m: None,
            load_5m: None,
            load_15m: None,
            disk_usage_percent: None,
            disk_total_bytes: None,
            disk_available_bytes: None,
        }
    }

    /// 起一次性服务器，回固定响应并**捕获完整请求**；返回 (endpoint, 请求文本句柄)。
    async fn capture_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let mut request = Vec::new();
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0_u8; 4096];
                // 读到无数据（50ms 静默）为止：小请求一次读全，含 body。
                loop {
                    match tokio::time::timeout(Duration::from_millis(50), socket.read(&mut buffer))
                        .await
                    {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => request.extend_from_slice(&buffer[..n]),
                        Ok(Err(_)) => break,
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn link_upstream_sends_gateway_id_bearer_and_identity_header() {
        let (endpoint, captured) = capture_server(
            "200 OK",
            r#"{"config":{"gateway_id":"gw-1","control_center_endpoint":"https://c","trust_bundle":null,"server_tls_required":false,"protocol_version":"1.0","enrollment_token_id":"e1"},"regist_token":"reg_xyz"}"#,
        )
        .await;
        let client = CenterClient::new(endpoint);
        let returned = client
            .link_upstream("gw-1", "link_tok", Some("ident_xyz"))
            .await
            .expect("ok");
        assert_eq!(returned.regist_token.as_deref(), Some("reg_xyz"));
        let request = captured.await.expect("request");
        let lower = request.to_lowercase();
        assert!(
            lower.starts_with("get /api/v1/gateway/link-upstream?gateway_id=gw-1"),
            "{request}"
        );
        assert!(
            lower.contains("authorization: bearer link_tok"),
            "{request}"
        );
        assert!(
            lower.contains("x-gateway-identity-token: ident_xyz"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn register_posts_enrollment_token_instance_id_and_csr() {
        let (endpoint, captured) = capture_server(
            "200 OK",
            r#"{"status":"accepted","gateway_id":"gw-1","instance_id":"inst-1","credential_id":"c1","initial_config":"v1","credential_bundle":{"credential_id":"c1","gateway_id":"gw-1","instance_id":"inst-1","certificate":"-----BEGIN CERTIFICATE-----\nA\n-----END CERTIFICATE-----\n","ca_bundle":null,"issued_at":"2026-10-04T00:00:00Z","not_before":"2026-10-04T00:00:00Z","not_after":"2026-11-04T00:00:00Z"}}"#,
        )
        .await;
        let client = CenterClient::new(endpoint);
        let result = client
            .register(
                "reg_tok",
                "inst-1",
                "-----BEGIN CERTIFICATE REQUEST-----\nQ\n-----END CERTIFICATE REQUEST-----\n",
                Some("https://gw.example.com"),
            )
            .await
            .expect("ok");
        assert!(
            result
                .credential_bundle
                .certificate
                .contains("BEGIN CERTIFICATE")
        );
        assert_eq!(result.credential_bundle.gateway_id, "gw-1");
        let request = captured.await.expect("request");
        assert!(
            request
                .to_lowercase()
                .starts_with("post /api/v1/gateway/register"),
            "{request}"
        );
        assert!(
            request.contains("reg_tok")
                && request.contains("inst-1")
                && request.contains("certificate_signing_request")
                && request.contains("public_base_url")
                && request.contains("https://gw.example.com"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn renew_credential_parses_the_new_certificate_bundle() {
        let (endpoint, capture) = capture_server(
            "200 OK",
            r#"{"credential_id":"c2","gateway_id":"gw-1","instance_id":null,"certificate":"-----BEGIN CERTIFICATE-----\nB\n-----END CERTIFICATE-----\n","ca_bundle":null,"issued_at":"2026-10-04T00:00:00Z","not_before":"2026-10-04T00:00:00Z","not_after":"2026-12-04T00:00:00Z"}"#,
        )
        .await;
        let client = CenterClient::new(endpoint);
        let bundle = client
            .renew_credential(
                "gw-1",
                "01ab",
                "-----BEGIN CERTIFICATE REQUEST-----\nQ\n-----END CERTIFICATE REQUEST-----\n",
            )
            .await
            .expect("ok");
        assert!(bundle.certificate.contains("BEGIN CERTIFICATE"));
        let request = capture.await.expect("request");
        assert!(
            request.contains("current_certificate_serial")
                && request.contains("01ab")
                && request.contains("certificate_signing_request"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn report_upgrade_result_posts_the_center_contract() {
        let (endpoint, captured) = capture_server("200 OK", r#"{}"#).await;
        let client = CenterClient::new(endpoint);
        let record = crate::state::UpgradeRecord {
            work_id: "w-1".into(),
            from_version: "0.1.0".into(),
            to_version: "0.1.16".into(),
            step: "verify".into(),
            status: "done".into(),
            detail: "ok".into(),
        };
        client
            .report_upgrade_result("gw-1", &record)
            .await
            .expect("ok");
        let request = captured.await.expect("request");
        assert!(
            request
                .to_lowercase()
                .starts_with("post /api/v1/gateway/upgrade-result"),
            "{request}"
        );
        // body 必须是中心侧 ReportGatewayUpgradeResult（含必填 gateway_id / reported_at）——
        // 直接发 UpgradeRecord 会被中心 422 拒掉。
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let sent: wist_control::ReportGatewayUpgradeResult = serde_json::from_str(body)
            .unwrap_or_else(|err| panic!("body 不是合法回执: {err}; {body}"));
        assert_eq!(sent.gateway_id, "gw-1");
        assert_eq!(sent.work_id, "w-1");
        assert_eq!(sent.status, "done");
    }
}
