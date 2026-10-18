//! 「知识库包下发」的**环回交付器**。
//!
//! 中心派下的 `push-knowledge-package` 计划：gwlinkd 用自己的 CA-S 客户端取包（它持中心信任），
//! 把字节落到本机投放目录（与 agent 包同一份 `packages/`，网关容器只读挂载），再**环回 POST**
//! 同机网关的 `POST /api/v1/gateway/knowledge-package` —— 网关据此「从本机路径取 → 校验摘要 /
//! 验签 → 内容寻址缓存 → 记历史」，`activate` 决定是否立刻切生效指针。
//!
//! 与 agent 包下发的差别：知识库是**内容**，不重建网关、不涉及升级；签名旁文件
//! （`<tarball>.sig`）要一起落地 —— 配了验签公钥的网关会拒收没有 `.sig` 的包。
//!
//! 分层见设计 `wist-design/doc/design/edge/center-content-delivery.md`、
//! `wist-design/doc/design/edge/knowledge-package-push-to-gateways.md`。

use orion_error::prelude::*;

use crate::error::{CenterReason, CenterResult};

/// 环回写入器（形如 `https://127.0.0.1:3000`，与 self / link-request 面同一 endpoint + 信任锚）。
#[derive(Debug, Clone)]
pub struct KnowledgePackageClient {
    endpoint: String,
    http: reqwest::Client,
}

/// 交付载荷：与网关环回端点 `POST /api/v1/gateway/knowledge-package` 同形。
#[derive(Debug, serde::Serialize)]
struct PushRequest<'a> {
    /// 网关**能取**的来源：gwlinkd 落地的本机路径（容器可见，如 `/packages/<file>`）。
    source: &'a str,
    /// 中心给的期望摘要（网关拿来源字节核对）。
    sha256: &'a str,
    /// 中心镜像地址（**provenance/留痕**；网关不据此取包）。
    origin: &'a str,
    /// 录入后是否立即激活。
    activate: bool,
    requested_by: &'a str,
}

impl KnowledgePackageClient {
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
    pub fn with_trust(
        endpoint: impl Into<String>,
        trust: Option<&std::path::Path>,
    ) -> CenterResult<Self> {
        Ok(Self {
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            http: crate::center::build_http_client(trust)?,
        })
    }

    /// 把一份知识库包交付给网关知识库管理：`POST /api/v1/gateway/knowledge-package`（环回，loopback-only）。
    /// 非 2xx → `Err`（含响应体，便于诊断）。
    pub async fn push(
        &self,
        source: &str,
        sha256: &str,
        origin: &str,
        activate: bool,
    ) -> CenterResult<()> {
        let url = format!("{}/api/v1/gateway/knowledge-package", self.endpoint);
        let payload = PushRequest {
            source,
            sha256,
            origin,
            activate,
            requested_by: "wist-gwlinkd",
        };
        let response = self
            .http
            .post(url)
            .header("accept", "application/json")
            .json(&payload)
            .send()
            .await
            .source_raw_err(CenterReason::Http, "knowledge-package 请求失败")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(
                CenterReason::Http.err(format!("knowledge-package 失败（{status}）：{body}"))
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

    /// 载荷**形状契约**（与网关侧 `knowledge_package::tests` 钉同一组键）。
    #[tokio::test]
    async fn push_posts_the_knowledge_package_payload_to_the_loopback_endpoint() {
        let (endpoint, captured) = one_shot_capturing("200 OK", r#"{"package_id":"kbp-a"}"#).await;
        let client = KnowledgePackageClient::new(endpoint);
        client
            .push(
                "/packages/wist-knowledge-0.3.0.tar.gz",
                "sha256:780d6375472e069016209755a42bfdaa89876da03670d2c9ffc8d397bc52c6ec",
                "https://center.example/api/v1/releases/artifact/wist-knowledge/0.3.0/wist-knowledge-0.3.0.tar.gz",
                true,
            )
            .await
            .expect("push ok");

        let request = captured.await.expect("request captured");
        assert!(
            request.starts_with("POST /api/v1/gateway/knowledge-package "),
            "{request}"
        );
        assert!(
            request.contains("\"source\":\"/packages/wist-knowledge-0.3.0.tar.gz\""),
            "{request}"
        );
        assert!(
            request.contains(
                "\"sha256\":\"sha256:780d6375472e069016209755a42bfdaa89876da03670d2c9ffc8d397bc52c6ec\""
            ),
            "{request}"
        );
        assert!(
            request.contains("\"origin\":\"https://center.example/"),
            "{request}"
        );
        assert!(request.contains("\"activate\":true"), "{request}");
        assert!(
            request.contains("\"requested_by\":\"wist-gwlinkd\""),
            "{request}"
        );
    }

    /// 非 2xx → `Err`（含响应体，便于诊断）。
    #[tokio::test]
    async fn a_non_success_response_is_an_error() {
        let (endpoint, _captured) =
            one_shot_capturing("403 Forbidden", "knowledge-package is loopback-only").await;
        let client = KnowledgePackageClient::new(endpoint);
        let err = client
            .push(
                "/packages/k.tar.gz",
                "sha256:aa",
                "https://c/k.tar.gz",
                false,
            )
            .await
            .expect_err("must fail");
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("loopback-only"), "{err}");
    }

    #[test]
    fn with_trust_rejects_a_missing_ca() {
        let err = KnowledgePackageClient::with_trust(
            "https://127.0.0.1:3000",
            Some(std::path::Path::new("/definitely/not/here.pem")),
        )
        .expect_err("missing CA must error（不静默回落）");
        assert!(err.contains("读取信任锚失败"), "{err}");
    }
}
