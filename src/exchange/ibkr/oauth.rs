//! IBKR OAuth 1.0a 认证实现
//!
//! 完整实现 OAuth 1.0a + Diffie-Hellman 密钥交换流程:
//! 1. RSA-PKCS1v15 解密 access_token_secret → prepend
//! 2. DH 密钥交换获取 live_session_token
//! 3. HMAC-SHA256 签名后续 REST 请求

use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::rngs::OsRng;
use rand::Rng;
use reqwest::Client;
use rsa::{Pkcs1v15Encrypt, Pkcs1v15Sign, RsaPrivateKey};
use sha1::Sha1;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

use super::IbkrCredentials;

/// percent-encoding 字符集: 编码除 unreserved 字符外的所有字符
/// unreserved = ALPHA / DIGIT / '-' / '.' / '_' / '~'
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// IBKR OAuth 1.0a 认证器
pub struct IbkrOAuth {
    consumer_key: String,
    access_token: String,
    realm: String,
    signature_key: RsaPrivateKey,
    dh_prime: BigUint,
    dh_generator: u32,
    /// RSA 解密后的 access_token_secret 原始字节
    prepend_bytes: Vec<u8>,
    /// prepend 的 hex 字符串 (用于 base_string)
    prepend_hex: String,
    /// base64 编码的 live session token
    live_session_token: Option<String>,
    base_url: String,
}

impl IbkrOAuth {
    /// 创建新的 OAuth 认证器
    pub fn new(cred: &IbkrCredentials) -> anyhow::Result<Self> {
        // 加载两个 RSA 私钥
        let enc_pem = std::fs::read_to_string(&cred.encryption_key_fp)?;
        let sig_pem = std::fs::read_to_string(&cred.signature_key_fp)?;

        let encryption_key: RsaPrivateKey = rsa::pkcs8::DecodePrivateKey::from_pkcs8_pem(&enc_pem)
            .map_err(|e| anyhow::anyhow!("Failed to parse encryption key: {}", e))?;
        let signature_key: RsaPrivateKey = rsa::pkcs8::DecodePrivateKey::from_pkcs8_pem(&sig_pem)
            .map_err(|e| anyhow::anyhow!("Failed to parse signature key: {}", e))?;

        // RSA-PKCS1v15 解密 access_token_secret
        let access_token_secret_bytes = general_purpose::STANDARD
            .decode(&cred.access_token_secret)
            .map_err(|e| anyhow::anyhow!("Failed to decode access_token_secret: {}", e))?;
        let prepend_bytes = encryption_key
            .decrypt(Pkcs1v15Encrypt, &access_token_secret_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to decrypt access_token_secret: {}", e))?;
        let prepend_hex = hex::encode(&prepend_bytes);

        // 解析 DH prime
        let dh_prime = BigUint::parse_bytes(cred.dh_prime.as_bytes(), 16)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse dh_prime as hex BigUint"))?;

        Ok(Self {
            consumer_key: cred.consumer_key.clone(),
            access_token: cred.access_token.clone(),
            realm: "limited_poa".to_string(),
            signature_key,
            dh_prime,
            dh_generator: 2,
            prepend_bytes,
            prepend_hex,
            live_session_token: None,
            base_url: super::REST_BASE_URL.to_string(),
        })
    }

    /// 获取 access_token (供 WebSocket 连接使用)
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// 获取 base_url
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 初始化: 执行 DH 密钥交换获取 live_session_token
    pub async fn init(&mut self, http: &Client) -> anyhow::Result<()> {
        // 1. 生成 256-bit DH random
        let dh_random_bytes: [u8; 32] = rand::rngs::OsRng.gen();
        let dh_random = BigUint::from_bytes_be(&dh_random_bytes);

        // 2. 计算 DH challenge: g^random mod p
        let generator = BigUint::from(self.dh_generator);
        let dh_challenge = generator.modpow(&dh_random, &self.dh_prime);
        let dh_challenge_hex = format!("{:x}", dh_challenge);

        // 3. 构建 OAuth header (RSA-SHA256 签名)
        let endpoint = "oauth/live_session_token";
        let url = format!("{}{}", self.base_url, endpoint);

        let timestamp = current_timestamp();
        let nonce = generate_nonce();

        let mut headers = vec![
            ("oauth_consumer_key", self.consumer_key.clone()),
            ("oauth_nonce", nonce),
            ("oauth_signature_method", "RSA-SHA256".to_string()),
            ("oauth_timestamp", timestamp),
            ("oauth_token", self.access_token.clone()),
            ("diffie_hellman_challenge", dh_challenge_hex),
        ];

        let base_string =
            generate_base_string("POST", &url, &headers, None, Some(&self.prepend_hex));

        // RSA-SHA256 签名
        let signature = self.rsa_sha256_sign(&base_string)?;
        headers.push(("oauth_signature", signature));

        let auth_header = generate_authorization_header(&headers, &self.realm);

        // 4. POST 请求
        let resp = http
            .post(&url)
            .header("Authorization", &auth_header)
            .header("User-Agent", "ibind-rs")
            .header("Host", "api.ibkr.com")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("LST request failed: {}", e))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("LST response parse failed: {}", e))?;

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "LST request returned {}: {}",
                status,
                body
            ));
        }

        // 5. 提取 DH response 并计算 shared secret
        let dh_response_hex = body["diffie_hellman_response"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing diffie_hellman_response in LST response"))?;
        let lst_signature = body["live_session_token_signature"]
            .as_str()
            .ok_or_else(|| {
                anyhow::anyhow!("Missing live_session_token_signature in LST response")
            })?;

        let dh_response = BigUint::parse_bytes(dh_response_hex.as_bytes(), 16)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse dh_response"))?;

        let shared_secret = dh_response.modpow(&dh_random, &self.dh_prime);

        // 6. HMAC-SHA1(key=to_byte_array(shared_secret), msg=prepend_bytes) → live_session_token
        let shared_secret_bytes = to_byte_array(&shared_secret);
        let mut hmac_sha1 =
            Hmac::<Sha1>::new_from_slice(&shared_secret_bytes).expect("HMAC accepts any size");
        hmac_sha1.update(&self.prepend_bytes);
        let lst = general_purpose::STANDARD.encode(hmac_sha1.finalize().into_bytes());

        // 7. 验证 live_session_token_signature
        let mut verify_hmac =
            Hmac::<Sha1>::new_from_slice(&general_purpose::STANDARD.decode(&lst)?)
                .expect("HMAC accepts any size");
        verify_hmac.update(self.consumer_key.as_bytes());
        let computed_sig = hex::encode(verify_hmac.finalize().into_bytes());

        if computed_sig != lst_signature {
            return Err(anyhow::anyhow!(
                "LST signature mismatch: computed={}, expected={}",
                computed_sig,
                lst_signature
            ));
        }

        tracing::info!("IBKR OAuth: live_session_token obtained and verified");
        self.live_session_token = Some(lst);
        Ok(())
    }

    /// 为 REST 请求生成 OAuth Authorization header
    pub fn sign_request(
        &self,
        method: &str,
        url: &str,
        params: Option<&[(&str, &str)]>,
    ) -> anyhow::Result<String> {
        let lst = self
            .live_session_token
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("live_session_token not initialized"))?;

        let timestamp = current_timestamp();
        let nonce = generate_nonce();

        let mut headers = vec![
            ("oauth_consumer_key", self.consumer_key.clone()),
            ("oauth_nonce", nonce),
            ("oauth_signature_method", "HMAC-SHA256".to_string()),
            ("oauth_timestamp", timestamp),
            ("oauth_token", self.access_token.clone()),
        ];

        let base_string = generate_base_string(method, url, &headers, params, None);

        // HMAC-SHA256 签名
        let lst_bytes = general_purpose::STANDARD.decode(lst)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&lst_bytes).expect("HMAC accepts any size");
        mac.update(base_string.as_bytes());
        let signature = url_encode(&general_purpose::STANDARD.encode(mac.finalize().into_bytes()));

        headers.push(("oauth_signature", signature));

        Ok(generate_authorization_header(&headers, &self.realm))
    }

    /// RSA-SHA256 签名 (用于 LST 请求)
    fn rsa_sha256_sign(&self, base_string: &str) -> anyhow::Result<String> {
        use sha2::Digest;
        let hash = Sha256::digest(base_string.as_bytes());
        let signature = self
            .signature_key
            .sign(Pkcs1v15Sign::new::<Sha256>(), &hash)
            .map_err(|e| anyhow::anyhow!("RSA sign failed: {}", e))?;
        Ok(url_encode(&general_purpose::STANDARD.encode(&signature)))
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 生成 16 字符随机 nonce
fn generate_nonce() -> String {
    use rand::distributions::Alphanumeric;
    OsRng
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect()
}

/// 当前 Unix 时间戳 (秒)
fn current_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

/// URL encode (percent encoding)
fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, ENCODE_SET).to_string()
}

/// 生成 OAuth base string
///
/// 格式: METHOD&url_encode(url)&url_encode(sorted_params)
fn generate_base_string(
    method: &str,
    url: &str,
    oauth_headers: &[(&str, String)],
    query_params: Option<&[(&str, &str)]>,
    prepend: Option<&str>,
) -> String {
    // 合并所有参数
    let mut all_params: Vec<(String, String)> = oauth_headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();

    if let Some(params) = query_params {
        for (k, v) in params {
            all_params.push((k.to_string(), v.to_string()));
        }
    }

    // 按 key 排序
    all_params.sort_by(|a, b| a.0.cmp(&b.0));

    // 拼接: key=value&key=value
    let params_string: String = all_params
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    let encoded_url = url_encode_plus(url);
    let encoded_params = url_encode_plus(&params_string);

    let base = format!("{}&{}&{}", method, encoded_url, encoded_params);

    match prepend {
        Some(p) => format!("{}{}", p, base),
        None => base,
    }
}

/// quote_plus style encoding (与 Python urllib.parse.quote_plus 一致)
fn url_encode_plus(s: &str) -> String {
    // percent_encode 将空格编码为 %20，quote_plus 编码为 +
    // 这里用 percent_encode 以匹配 Python 的 quote_plus 行为
    let encoded = utf8_percent_encode(s, ENCODE_SET).to_string();
    encoded.replace("%20", "+")
}

/// 生成 OAuth Authorization header string
fn generate_authorization_header(params: &[(&str, String)], realm: &str) -> String {
    let mut sorted: Vec<(&str, &String)> = params.iter().map(|(k, v)| (*k, v)).collect();
    sorted.sort_by_key(|(k, _)| *k);

    let pairs: Vec<String> = sorted
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, v))
        .collect();

    format!("OAuth realm=\"{}\", {}", realm, pairs.join(", "))
}

/// 将 BigUint 转换为字节数组 (含前导零逻辑，与 Python ibind 一致)
fn to_byte_array(x: &BigUint) -> Vec<u8> {
    let mut bytes = x.to_bytes_be();
    // 与 Python ibind 一致: 如果最高位 bit 为 1，需要前置 0 字节
    if !bytes.is_empty() && bytes[0] & 0x80 != 0 {
        bytes.insert(0, 0);
    }
    bytes
}
