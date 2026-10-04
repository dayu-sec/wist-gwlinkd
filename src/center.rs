//! 连 `WistCenter` 的客户端：link-upstream / register / status / credentials:renew。
//!
//! 本进程是**运行期凭据 `rt_` 的唯一持有者** —— 全边缘只此一处与中心对话。wire 类型来自
//! `wist-control`（由 `wist-design/jumo` 模型生成）。
//!
//! 骨架：HTTP 调用与凭据落盘逐步落地。

/// 中心客户端。
pub struct CenterClient {
    endpoint: String,
    #[allow(dead_code)]
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

    // TODO(④): link_upstream（带 X-Gateway-Identity-Token）/ register / report_status /
    //          renew_credential；运行期凭据 `rt_` 落盘见 crate::state。
}
