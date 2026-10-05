//! gwlinkd 自身状态（心跳）载荷：周期推给网关，供网关 Web 展示宿主侧常驻是否在跑 / 健康。
//!
//! 设计 `wist-design/doc/design/edge/gateway-linkd-status.md`。字段 **snake_case**，
//! 与网关侧 `GwlinkdStatus`（`wist-gateway/src/api/linkd_status.rs`）同钉一份形状 —— 任一侧改名即爆。

use wist_control::DateTime;

/// `state`：在跑，尚未接入（无客户端证书），等页面提交接入物。
pub const STATE_WAITING_LINK_REQUEST: &str = "WaitingLinkRequest";
/// `state`：已接入（持客户端证书），mTLS 正常。
pub const STATE_LINKED: &str = "Linked";
/// `state`：已接入但最近有失败（中心不可达 / 上报被拒 / 续期失败），带 `last_error`。
pub const STATE_DEGRADED: &str = "Degraded";

/// gwlinkd 自身状态（**无密钥**；网关 admin 视图可原样回传）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GwlinkdStatus {
    pub gateway_id: String,
    pub instance_id: String,
    pub version: String,
    pub center_endpoint: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_center_report_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub reported_at: DateTime,
}

/// RFC3339「现在」：经 serde 取 chrono 的 rfc3339 序列化，免得 gwlinkd 直接依赖 `chrono`。
pub fn now_rfc3339() -> String {
    serde_json::to_value(DateTime::now())
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 载荷**键集契约**（snake_case）。网关侧 `linkd_status.rs` 有同一形状的测试 —— 任一侧改名即爆。
    #[test]
    fn status_serializes_snake_case_and_skips_none() {
        let status = GwlinkdStatus {
            gateway_id: "gw-1".into(),
            instance_id: "gw-1/inst-1".into(),
            version: "0.4.0".into(),
            center_endpoint: "https://center.example".into(),
            state: STATE_LINKED.into(),
            credential_expires_at: None,
            last_center_report_at: None,
            last_error: None,
            reported_at: DateTime::now(),
        };
        let value = serde_json::to_value(&status).expect("serialize");
        let object = value.as_object().expect("object");
        for key in [
            "gateway_id",
            "instance_id",
            "version",
            "center_endpoint",
            "state",
            "reported_at",
        ] {
            assert!(object.contains_key(key), "缺 {key}: {value}");
        }
        assert!(!object.contains_key("last_error"), "None 应被跳过: {value}");
        assert!(
            value["reported_at"].is_string(),
            "reported_at 应为 RFC3339 串: {value}"
        );
    }

    /// 可选字段有值时必须**如实序列化**（None 跳过 ≠ 有值也丢）。
    #[test]
    fn status_serializes_optional_fields_when_present() {
        let status = GwlinkdStatus {
            gateway_id: "gw-1".into(),
            instance_id: "gw-1/inst-1".into(),
            version: "0.4.0".into(),
            center_endpoint: "https://center.example".into(),
            state: STATE_DEGRADED.into(),
            credential_expires_at: Some("2026-12-01T00:00:00Z".into()),
            last_center_report_at: Some("2026-10-05T00:00:00Z".into()),
            last_error: Some("中心不可达".into()),
            reported_at: DateTime::now(),
        };
        let value = serde_json::to_value(&status).expect("serialize");
        assert_eq!(value["state"], "Degraded");
        assert_eq!(value["last_error"], "中心不可达");
        assert!(value["credential_expires_at"].is_string(), "{value}");
        assert!(value["last_center_report_at"].is_string(), "{value}");
    }

    /// `state` 字面量与网关侧同钉一份契约 —— 任一侧改字面量即爆（页面判定直接靠这个）。
    #[test]
    fn state_constants_match_the_contract() {
        assert_eq!(STATE_WAITING_LINK_REQUEST, "WaitingLinkRequest");
        assert_eq!(STATE_LINKED, "Linked");
        assert_eq!(STATE_DEGRADED, "Degraded");
    }

    /// 心跳时刻必须是**非空 RFC3339 串**（网关按它解析 / 对齐排障）。
    #[test]
    fn now_rfc3339_is_a_non_empty_string() {
        let now = now_rfc3339();
        assert!(now.contains('T'), "应是 RFC3339（形如 2026-…T…）：{now}");
        assert!(now.len() >= 19, "RFC3339 至少到秒：{now}");
    }
}
