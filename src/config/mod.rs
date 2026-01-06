use crate::domain::Exchange;
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::hyperliquid::HyperliquidCredentials;
use crate::exchange::okx::OkxCredentials;
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// 交易所配置集合
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangesConfig {
    pub binance: BinanceCredentials,
    pub okx: OkxCredentials,
    #[serde(default)]
    pub hyperliquid: Option<HyperliquidCredentials>,
}

impl ExchangesConfig {
    /// 获取已配置的交易所列表
    pub fn enabled_exchanges(&self) -> Vec<Exchange> {
        let mut exchanges = vec![Exchange::Binance, Exchange::OKX];
        if self.hyperliquid.is_some() {
            exchanges.push(Exchange::Hyperliquid);
        }
        exchanges
    }
}

/// 引擎配置
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
}

fn default_queue_capacity() -> usize {
    256
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            queue_capacity: default_queue_capacity(),
        }
    }
}

/// 框架层应用配置（不包含策略配置）
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub exchanges: ExchangesConfig,
    #[serde(default)]
    pub engine: EngineConfig,
}

/// 从配置文件加载配置
pub fn load_config<T: for<'de> Deserialize<'de>, P: AsRef<Path>>(
    path: P,
) -> Result<T, ConfigError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;

    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => Ok(serde_json::from_str(&content)?),
        Some("toml") => Ok(toml::from_str(&content)?),
        Some(ext) => Err(ConfigError::UnsupportedFormat(ext.to_string())),
        None => Err(ConfigError::UnsupportedFormat("unknown".to_string())),
    }
}

/// 配置错误
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    ParseToml(#[from] toml::de::Error),

    #[error("JSON parse error: {0}")]
    ParseJson(#[from] serde_json::Error),

    #[error("Unsupported config format: {0}")]
    UnsupportedFormat(String),
}
