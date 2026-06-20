use crate::backtest::data_cache::DataCache;
use crate::domain::{ExchangeError, Symbol};
use std::time::Duration;

/// 从 data.binance.vision 拉取历史数据 zip，带 [`DataCache`] 透写缓存。
///
/// 流程：先查缓存命中即返回；未命中则 HTTP 下载、落缓存、返回字节。
/// 404 视为"该日无此数据"返回 `Ok(None)` (而非错误)，其余非 2xx 返回 `Err` fail-fast。
///
/// 采用 `reqwest::blocking` —— 回测引擎是单线程虚拟时间循环，数据按天同步拉取最简单可靠。
pub struct BinanceHistoryDownloader {
    client: reqwest::blocking::Client,
    cache: Box<dyn DataCache>,
    base_url: String,
}

const BASE_URL: &str = "https://data.binance.vision/data";

impl BinanceHistoryDownloader {
    pub fn new(cache: Box<dyn DataCache>) -> Result<Self, ExchangeError> {
        Self::with_base_url(cache, BASE_URL.to_string())
    }

    pub fn with_base_url(cache: Box<dyn DataCache>, base_url: String) -> Result<Self, ExchangeError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| ExchangeError::Other(format!("build http client: {e}")))?;
        Ok(Self {
            client,
            cache,
            base_url,
        })
    }

    /// 取 key 对应的 zip 字节；`Ok(None)` 表示数据源 404 (该日无数据)。
    pub fn fetch(&self, key: &str) -> Result<Option<Vec<u8>>, ExchangeError> {
        if let Some(bytes) = self.cache.get(key) {
            return Ok(Some(bytes));
        }
        let url = format!("{}/{}", self.base_url, key);
        tracing::info!(%url, "downloading");
        let resp = self
            .client
            .get(&url)
            .send()
            .map_err(|e| ExchangeError::Other(format!("download {url}: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            let bytes = resp
                .bytes()
                .map_err(|e| ExchangeError::Other(format!("read body {url}: {e}")))?
                .to_vec();
            self.cache.put(key, &bytes);
            Ok(Some(bytes))
        } else if status.as_u16() == 404 {
            tracing::warn!(key, "no data (404)");
            Ok(None)
        } else {
            Err(ExchangeError::Other(format!(
                "download failed {status}: {url}"
            )))
        }
    }

    /// U 本位合约 (futures/um) 每日数据的 key，镜像官方目录结构。
    ///
    /// `kind` 如 "bookTicker" / "trades"，`date` 为 YYYY-MM-DD。
    pub fn daily_key(kind: &str, symbol: &Symbol, date: &str) -> String {
        format!("futures/um/daily/{kind}/{symbol}/{symbol}-{kind}-{date}.zip")
    }
}
