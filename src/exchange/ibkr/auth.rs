//! IbkrAuth trait — 抽象 IBKR 认证行为
//!
//! OAuth 和 Gateway 两种模式共享同一套业务逻辑（REST + WS），
//! 仅在认证方式、URL、TLS 配置上有差异。

/// IBKR 认证抽象
///
/// - `sign_request` 返回 `Option<String>`: OAuth 返回签名 header, Gateway 返回 None
/// - `ws_url()`: OAuth 返回含 token 的 URL, Gateway 返回裸 URL
/// - `build_http_client()`: Gateway 跳过证书验证
/// - `ws_connector()`: Gateway 返回跳过证书验证的 TLS connector
pub trait IbkrAuth: Send + Sync + 'static {
    fn base_url(&self) -> &str;
    fn ws_url(&self) -> String;
    fn sign_request(
        &self,
        method: &str,
        url: &str,
        params: Option<&[(&str, &str)]>,
    ) -> anyhow::Result<Option<String>>;
    fn build_http_client(&self) -> anyhow::Result<reqwest::Client>;
    fn ws_connector(&self) -> Option<tokio_tungstenite::Connector>;
    /// 格式化 WebSocket 连接所需的 session cookie
    fn format_ws_cookie(&self, session_id: &str) -> String;

    /// 构建带认证 header 的 HTTP 请求
    ///
    /// 自动调用 `sign_request` 并在有签名时添加 Authorization header。
    fn authed_request(
        &self,
        http: &reqwest::Client,
        method: &str,
        url: &str,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let auth_header = self.sign_request(method, url, None)?;

        let builder = match method {
            "GET" => http.get(url),
            "POST" => http.post(url),
            "DELETE" => http.delete(url),
            _ => unreachable!("unsupported HTTP method: {}", method),
        };

        let mut req = builder.header("User-Agent", "ibind-rs");
        if let Some(header) = auth_header {
            req = req.header("Authorization", header);
        }

        Ok(req)
    }
}
