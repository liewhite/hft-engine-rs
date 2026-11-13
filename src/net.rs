//! 出站 HTTP / WebSocket 的**唯一**构造入口：每条对外连接都必须带期限才建得出来。
//!
//! 卡住的外部请求会让进程活着却不干活、也不响应停机（停机要等在做的请求做完）。此前 9 处各自
//! `reqwest::Client::builder()`，超时一处处补（#713 ListenKey、#764 私有流建连、空闲连接），
//! 漏一处就是一次事故。现在建 client、建 WS 连接只有本模块的入口，期限是必填参数；
//! 直接调 reqwest / tokio-tungstenite 的构造函数由 `clippy.toml` 的 disallowed-methods 拦下
//! （`cargo clippy -- -D clippy::disallowed_methods`，出部署包前跑；`Client::default()` 拦不到，别用），
//! 本模块是唯一放行处。
//!
//! 所有 HTTP client 共用的设置（一条知识一处）：
//! - **User-Agent**：不带 UA 的请求会被部分 CDN 拒；
//! - **空闲连接只在池里留 [`POOL_IDLE_TIMEOUT`]**：对方会关掉空闲过久的长连接（常见 60s 上下），
//!   reqwest 默认留 90s。请求恰好发在对方关连接的时刻，会以「连接中途断开」失败 —— 下单时这算
//!   结果不确定、整机停机。让我们先于对方丢弃空闲连接，代价是空闲超过它后的第一个请求多一次建连。

use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// 见模块文档
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

const USER_AGENT: &str = concat!("hft-engine-rs/", env!("CARGO_PKG_VERSION"));

/// 一个 HTTP client 的构造参数。只能经 [`http`] 拿到，期限是必填的。
#[must_use = "调 build() 才建出 client"]
pub struct HttpSpec {
    timeout: Duration,
    accept_invalid_certs: bool,
}

/// 开始构造一个 HTTP client：`timeout` 是单个请求从发出到读完回复的上限
pub fn http(timeout: Duration) -> HttpSpec {
    HttpSpec {
        timeout,
        accept_invalid_certs: false,
    }
}

impl HttpSpec {
    /// 跳过证书校验（只给本机自签证书的 IBKR Client Portal Gateway 用）
    pub fn accept_invalid_certs(mut self) -> Self {
        self.accept_invalid_certs = true;
        self
    }

    #[allow(clippy::disallowed_methods)] // 唯一放行处，见模块文档
    pub fn build(self) -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .user_agent(USER_AGENT)
            .danger_accept_invalid_certs(self.accept_invalid_certs)
            .build()
    }

    /// 同步版（只给回测数据下载这类不在异步运行时里的工具用）
    #[allow(clippy::disallowed_methods)] // 唯一放行处，见模块文档
    pub fn build_blocking(self) -> Result<reqwest::blocking::Client, reqwest::Error> {
        reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .user_agent(USER_AGENT)
            .danger_accept_invalid_certs(self.accept_invalid_certs)
            .build()
    }
}

/// 发一个交易所 REST 请求：要先在该所的限频闸门取到额度（见 [`crate::exchange::rate_limit`]）。
/// 直接调 `RequestBuilder::send` 由 `clippy.toml` 拦下，本函数和 [`send_unmetered`] 是唯一放行处。
#[allow(clippy::disallowed_methods)] // 唯一放行处，见上
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    _admitted: crate::exchange::rate_limit::Admitted,
) -> reqwest::Result<reqwest::Response> {
    request.send().await
}

/// 发一个不受交易所限频约束的请求：只给 Pushgateway、告警这类非交易所接口用。
#[allow(clippy::disallowed_methods)] // 唯一放行处，见 [`send`]
pub(crate) async fn send_unmetered(
    request: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    request.send().await
}

pub type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 建 WS 连接（TCP + TLS + WS 握手），限时 `timeout`：网络半死时握手可能一直挂着，不限时进程就
/// 活着却一直起不来，也不响应停机（#764）。`tls` 为 `None` 用默认 TLS 配置。
///
/// 返回的错误已说明是超时还是握手失败；调用方按自己的错误类型包一层。
#[allow(clippy::disallowed_methods)] // 唯一放行处，见模块文档
pub async fn ws_connect(
    request: impl IntoClientRequest + Unpin,
    tls: Option<tokio_tungstenite::Connector>,
    timeout: Duration,
) -> Result<WsStream, String> {
    let handshake = tokio_tungstenite::connect_async_tls_with_config(request, None, false, tls);
    let (ws, _) = tokio::time::timeout(timeout, handshake)
        .await
        .map_err(|_| format!("握手超过 {}s", timeout.as_secs()))?
        .map_err(|e| e.to_string())?;
    Ok(ws)
}
