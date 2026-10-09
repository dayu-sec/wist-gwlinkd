//! 「Agent 包下发」（发布 ②）的**环回交付器**。
//!
//! 中心派下的 `push-agent-package` 计划：**gwlinkd 用自己的 CA-S 客户端取包**（它持中心信任），
//! 把字节落到本机投放目录，再**环回 POST** 同机网关的 `POST /api/v1/gateway/agent-package` ——
//! 网关据此「从本机路径取 → 校验摘要 → 缓存 → 记历史」（与 admin 端点同内核），包即进入网关包管理，
//! **升不升由网关决定**。gwlinkd 仍**纯出站**（本通道是它出站轮询的一站）。
//!
//! 分层（谁取包、谁托管）见设计 `wist-design/doc/design/edge/center-content-delivery.md`；
//! 特性见 `.../agent-package-push-to-gateways.md`；网关侧对应 `wist-gateway/src/api/agent_package.rs`。

/// 环回写入器（形如 `https://127.0.0.1:3000`，与 self / link-request 面同一 endpoint + 信任锚）。
#[derive(Debug, Clone)]
pub struct AgentPackageClient {
    endpoint: String,
    http: reqwest::Client,
}

/// 交付载荷：与网关 admin 端点 `POST /api/v1/admin/agent/install-package` **同形 + `origin`**。
#[derive(Debug, serde::Serialize)]
struct PushRequest<'a> {
    artifacts: Vec<PushArtifact<'a>>,
    requested_by: &'a str,
}

#[derive(Debug, serde::Serialize)]
struct PushArtifact<'a> {
    platform: &'a str,
    /// 网关**能取**的来源：gwlinkd 落地的本机路径（容器可见，如 `/packages/<file>`）。
    package_url: &'a str,
    /// 中心镜像地址（**provenance/留痕**；网关**不**据此取包，因它不持中心信任）。
    origin: &'a str,
    package_sha256: &'a str,
}

/// 一个待交付的平台制品（②）：平台 + 本机路径（容器可见）+ 中心地址（留痕）+ 期望摘要。
#[derive(Debug, Clone)]
pub struct AgentPackageItem<'a> {
    pub platform: &'a str,
    pub package_url: &'a str,
    pub origin: &'a str,
    pub package_sha256: &'a str,
}

impl AgentPackageClient {
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
    ) -> Result<Self, String> {
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: crate::center::build_http_client(trust)?,
        })
    }

    /// 把**一批**（多平台）agentd 包交付给网关托管：`POST /api/v1/gateway/agent-package`（环回，loopback-only）。
    ///
    /// 一次 POST 带全部平台 —— 网关侧**整批一次提交**（任一不合格整体拒绝落库；各平台的本地缓存文件
    /// 在拉取阶段逐个写入，见设计 `edge/agent-package-push-to-gateways.md` §10）。`package_url` = 本机路径
    /// （容器可见）；`origin` = 中心镜像地址（**留痕**）；`package_sha256` = 中心给的期望摘要。
    /// 非 2xx → `Err`（含响应体，便于诊断）。
    pub async fn push(&self, items: &[AgentPackageItem<'_>]) -> Result<(), String> {
        let url = format!("{}/api/v1/gateway/agent-package", self.endpoint);
        let payload = PushRequest {
            artifacts: items
                .iter()
                .map(|item| PushArtifact {
                    platform: item.platform,
                    package_url: item.package_url,
                    origin: item.origin,
                    package_sha256: item.package_sha256,
                })
                .collect(),
            requested_by: "wist-gwlinkd",
        };
        let response = self
            .http
            .post(url)
            .header("accept", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|err| format!("agent-package 请求失败: {err}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("agent-package 失败（{status}）：{body}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 一次性 HTTP 服务器：把收到的原始请求回传，并回固定响应。
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

    /// 交付载荷**形状契约**（多平台）：POST 到环回端点，body 带**每个平台**的 `platform` / `package_url`（本机路径）/
    /// `origin`（中心地址，留痕）/ `package_sha256` + `requested_by`。网关侧钉同一形状。
    #[tokio::test]
    async fn push_posts_the_agent_package_payload_to_the_loopback_endpoint() {
        let (endpoint, captured) = one_shot_capturing("200 OK", r#"{"packages":[]}"#).await;
        let client = AgentPackageClient::new(endpoint);
        client
            .push(&[
                AgentPackageItem {
                    platform: "aarch64-apple-darwin",
                    package_url: "/packages/wist-agentd-0.1.9-aarch64-apple-darwin.tar.gz",
                    origin: "https://center.example/api/v1/releases/artifact/wist-agentd/0.1.9/wist-agentd-0.1.9-aarch64-apple-darwin.tar.gz",
                    package_sha256: "sha256:3f9a1c0d",
                },
                AgentPackageItem {
                    platform: "x86_64-unknown-linux-musl",
                    package_url: "/packages/wist-agentd-0.1.9-x86_64-unknown-linux-musl.tar.gz",
                    origin: "https://center.example/api/v1/releases/artifact/wist-agentd/0.1.9/wist-agentd-0.1.9-x86_64-unknown-linux-musl.tar.gz",
                    package_sha256: "sha256:aa11bb22",
                },
            ])
            .await
            .expect("push ok");

        let request = captured.await.expect("request captured");
        assert!(
            request.starts_with("POST /api/v1/gateway/agent-package "),
            "{request}"
        );
        assert!(
            request.contains("\"platform\":\"aarch64-apple-darwin\""),
            "{request}"
        );
        assert!(
            request.contains("\"platform\":\"x86_64-unknown-linux-musl\""),
            "多平台：两个制品都要带：{request}"
        );
        assert!(
            request.contains("\"package_url\":\"/packages/wist-agentd-0.1.9-aarch64-apple-darwin.tar.gz\""),
            "{request}"
        );
        assert!(
            request.contains("\"origin\":\"https://center.example/"),
            "{request}"
        );
        assert!(
            request.contains("\"package_sha256\":\"sha256:3f9a1c0d\""),
            "{request}"
        );
        assert!(
            request.contains("\"requested_by\":\"wist-gwlinkd\""),
            "{request}"
        );
    }

    /// 非 2xx → `Err`（含响应体，便于诊断）。
    #[tokio::test]
    async fn a_non_success_response_is_an_error() {
        let (endpoint, _captured) =
            one_shot_capturing("403 Forbidden", "agent-package is loopback-only").await;
        let client = AgentPackageClient::new(endpoint);
        let err = client
            .push(&[AgentPackageItem {
                platform: "aarch64-apple-darwin",
                package_url: "/packages/wist-agentd-0.1.9.tar.gz",
                origin: "https://c/pkg.tar.gz",
                package_sha256: "sha256:aa",
            }])
            .await
            .expect_err("must fail");
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("loopback-only"), "{err}");
    }

    #[test]
    fn with_trust_rejects_a_missing_ca() {
        let err = AgentPackageClient::with_trust(
            "https://127.0.0.1:3000",
            Some(std::path::Path::new("/definitely/not/here.pem")),
        )
        .expect_err("missing CA must error（不静默回落）");
        assert!(err.contains("读取信任锚失败"), "{err}");
    }
}
