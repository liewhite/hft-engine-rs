//! IBKR OAuth 1.0a 认证实现
//!
//! 完整实现 OAuth 1.0a + Diffie-Hellman 密钥交换流程:
//! 1. RSA-PKCS1v15 解密 access_token_secret → prepend
//! 2. DH 密钥交换获取 live_session_token
//! 3. HMAC-SHA256 签名后续 REST 请求

use super::auth::IbkrAuth;
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// percent-encoding 字符集: 编码除 unreserved 字符外的所有字符
/// unreserved = ALPHA / DIGIT / '-' / '.' / '_' / '~'
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// IBKR OAuth 1.0a 认证器
///
/// 通过 `create()` 工厂方法构造，构造时完成 DH 密钥交换。
/// 构造后不可变，可安全地通过 `Arc<dyn IbkrAuth>` 共享。
pub struct IbkrOAuth {
    consumer_key: String,
    access_token: String,
    realm: String,
    /// base64 编码的 live session token (DH 交换后获得)
    live_session_token: String,
    base_url: String,
}

impl IbkrOAuth {
    /// 创建并初始化 OAuth 认证器 (含 DH 密钥交换)
    pub async fn create(
        consumer_key: &str,
        encryption_key_fp: &str,
        signature_key_fp: &str,
        access_token: &str,
        access_token_secret: &str,
        dh_prime: &str,
    ) -> anyhow::Result<Self> {
        // 加载两个 RSA 私钥
        let enc_pem = std::fs::read_to_string(encryption_key_fp)?;
        let sig_pem = std::fs::read_to_string(signature_key_fp)?;

        let encryption_key: RsaPrivateKey = rsa::pkcs8::DecodePrivateKey::from_pkcs8_pem(&enc_pem)
            .map_err(|e| anyhow::anyhow!("Failed to parse encryption key: {}", e))?;
        let signature_key: RsaPrivateKey = rsa::pkcs8::DecodePrivateKey::from_pkcs8_pem(&sig_pem)
            .map_err(|e| anyhow::anyhow!("Failed to parse signature key: {}", e))?;

        // RSA-PKCS1v15 解密 access_token_secret
        let access_token_secret_bytes = general_purpose::STANDARD
            .decode(access_token_secret)
            .map_err(|e| anyhow::anyhow!("Failed to decode access_token_secret: {}", e))?;
        let prepend_bytes = encryption_key
            .decrypt(Pkcs1v15Encrypt, &access_token_secret_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to decrypt access_token_secret: {}", e))?;
        let prepend_hex = hex::encode(&prepend_bytes);

        // 解析 DH prime
        let dh_prime_num = BigUint::parse_bytes(dh_prime.as_bytes(), 16)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse dh_prime as hex BigUint"))?;

        let base_url = super::OAUTH_BASE_URL.to_string();

        // DH 密钥交换
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        let live_session_token = dh_exchange(
            &http,
            &base_url,
            consumer_key,
            access_token,
            &prepend_bytes,
            &prepend_hex,
            &signature_key,
            &dh_prime_num,
        )
        .await?;

        tracing::info!("IBKR OAuth: live_session_token obtained and verified");

        Ok(Self {
            consumer_key: consumer_key.to_string(),
            access_token: access_token.to_string(),
            realm: "limited_poa".to_string(),
            live_session_token,
            base_url,
        })
    }
}

impl IbkrAuth for IbkrOAuth {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn ws_url(&self) -> String {
        format!(
            "wss://api.ibkr.com/v1/api/ws?oauth_token={}",
            self.access_token
        )
    }

    fn sign_request(
        &self,
        method: &str,
        url: &str,
        params: Option<&[(&str, &str)]>,
    ) -> anyhow::Result<Option<String>> {
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
        let lst_bytes = general_purpose::STANDARD.decode(&self.live_session_token)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&lst_bytes).expect("HMAC accepts any size");
        mac.update(base_string.as_bytes());
        let signature = url_encode(&general_purpose::STANDARD.encode(mac.finalize().into_bytes()));

        headers.push(("oauth_signature", signature));

        Ok(Some(generate_authorization_header(&headers, &self.realm)))
    }

    fn build_http_client(&self) -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
    }

    fn ws_connector(&self) -> Option<tokio_tungstenite::Connector> {
        None
    }
}

// ============================================================================
// DH 密钥交换 (构造阶段一次性完成)
// ============================================================================

async fn dh_exchange(
    http: &Client,
    base_url: &str,
    consumer_key: &str,
    access_token: &str,
    prepend_bytes: &[u8],
    prepend_hex: &str,
    signature_key: &RsaPrivateKey,
    dh_prime: &BigUint,
) -> anyhow::Result<String> {
    let dh_generator: u32 = 2;

    // 1. 生成 256-bit DH random
    let dh_random_bytes: [u8; 32] = rand::rngs::OsRng.gen();
    let dh_random = BigUint::from_bytes_be(&dh_random_bytes);

    // 2. 计算 DH challenge: g^random mod p
    let generator = BigUint::from(dh_generator);
    let dh_challenge = generator.modpow(&dh_random, dh_prime);
    let dh_challenge_hex = format!("{:x}", dh_challenge);

    // 3. 构建 OAuth header (RSA-SHA256 签名)
    let endpoint = "oauth/live_session_token";
    let url = format!("{}{}", base_url, endpoint);

    let timestamp = current_timestamp();
    let nonce = generate_nonce();

    let mut headers = vec![
        ("oauth_consumer_key", consumer_key.to_string()),
        ("oauth_nonce", nonce),
        ("oauth_signature_method", "RSA-SHA256".to_string()),
        ("oauth_timestamp", timestamp),
        ("oauth_token", access_token.to_string()),
        ("diffie_hellman_challenge", dh_challenge_hex),
    ];

    let base_string =
        generate_base_string("POST", &url, &headers, None, Some(prepend_hex));

    // RSA-SHA256 签名
    let signature = rsa_sha256_sign(signature_key, &base_string)?;
    headers.push(("oauth_signature", signature));

    let auth_header = generate_authorization_header(&headers, "limited_poa");

    // 4. POST 请求 (必须带 Content-Length: 0，否则 411)
    let resp = http
        .post(&url)
        .header("Authorization", &auth_header)
        .header("User-Agent", "ibind-rs")
        .header("Content-Length", "0")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("LST request failed: {}", e))?;

    let status = resp.status();
    let resp_text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("LST response read failed: {}", e))?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "LST request returned {}: {}",
            status,
            resp_text
        ));
    }

    let body: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| anyhow::anyhow!("LST response parse failed: {}", e))?;

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

    let shared_secret = dh_response.modpow(&dh_random, dh_prime);

    // 6. HMAC-SHA1(key=to_byte_array(shared_secret), msg=prepend_bytes) → live_session_token
    let shared_secret_bytes = to_byte_array(&shared_secret);
    let mut hmac_sha1 =
        Hmac::<Sha1>::new_from_slice(&shared_secret_bytes).expect("HMAC accepts any size");
    hmac_sha1.update(prepend_bytes);
    let lst = general_purpose::STANDARD.encode(hmac_sha1.finalize().into_bytes());

    // 7. 验证 live_session_token_signature
    let mut verify_hmac =
        Hmac::<Sha1>::new_from_slice(&general_purpose::STANDARD.decode(&lst)?)
            .expect("HMAC accepts any size");
    verify_hmac.update(consumer_key.as_bytes());
    let computed_sig = hex::encode(verify_hmac.finalize().into_bytes());

    if computed_sig != lst_signature {
        return Err(anyhow::anyhow!(
            "LST signature mismatch: computed={}, expected={}",
            computed_sig,
            lst_signature
        ));
    }

    Ok(lst)
}

fn rsa_sha256_sign(key: &RsaPrivateKey, base_string: &str) -> anyhow::Result<String> {
    use sha2::Digest;
    let hash = Sha256::digest(base_string.as_bytes());
    let signature = key
        .sign(Pkcs1v15Sign::new::<Sha256>(), &hash)
        .map_err(|e| anyhow::anyhow!("RSA sign failed: {}", e))?;
    Ok(url_encode(&general_purpose::STANDARD.encode(&signature)))
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
