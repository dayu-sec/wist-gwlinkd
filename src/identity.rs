//! 网关客户端身份的密钥对与 CSR 生成（**私钥永不出本机**）。
//!
//! 入场与轮换都走同一条路：本机当场生成一套密钥对，只把 **CSR（公钥）** 交给中心；
//! 中心用 CA-G 签出「每网关一张」客户端证书后回执。长期身份 = 证书 + 私钥（mTLS），
//! 取代旧的对称 bearer `rt_`。

use rcgen::{CertificateParams, DnType, KeyPair};

/// 新生成的一套客户端密钥对与 CSR（PEM）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientKeypair {
    /// 客户端私钥（PEM，PKCS#8）。**永不上送 / 不出本机**。
    pub private_key_pem: String,
    /// 证书签名请求（PEM，只含公钥）。
    pub csr_pem: String,
}

/// 生成一套新的客户端密钥对与 CSR。`common_name` 仅作人类可读标签
/// （权威身份是中心按 `gateway_id` 填的 URI SAN；主体以中心为准）。
pub fn generate_client_keypair(common_name: &str) -> Result<ClientKeypair, String> {
    let key = KeyPair::generate().map_err(|err| format!("生成客户端密钥失败: {err}"))?;
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, common_name.to_string());
    let csr_pem = params
        .serialize_request(&key)
        .map_err(|err| format!("生成 CSR 失败: {err}"))?
        .pem()
        .map_err(|err| format!("CSR 编码 PEM 失败: {err}"))?;
    Ok(ClientKeypair {
        private_key_pem: key.serialize_pem(),
        csr_pem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_csr_whose_public_key_parses() {
        let pair = generate_client_keypair("gw-1").expect("keypair");
        assert!(pair.private_key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(pair.csr_pem.contains("BEGIN CERTIFICATE REQUEST"));

        let (_, pem) = x509_parser::pem::parse_x509_pem(pair.csr_pem.as_bytes()).expect("pem");
        assert_eq!(pem.label, "CERTIFICATE REQUEST");
        assert!(!pem.contents.is_empty());
    }

    #[test]
    fn each_call_generates_a_distinct_key() {
        let a = generate_client_keypair("gw-1").expect("a");
        let b = generate_client_keypair("gw-1").expect("b");
        assert_ne!(a.private_key_pem, b.private_key_pem);
        assert_ne!(a.csr_pem, b.csr_pem);
    }
}
