use crate::domain::{Exchange, Symbol};
use crate::strategy::FundingArbConfig;
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// 应用配置
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub exchanges: ExchangesConfig,
    pub strategy: StrategyConfig,
    pub engine: EngineConfig,
}

/// 交易所凭证配置
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeCredentials {
    pub api_key: String,
    pub secret: String,
    #[serde(default)]
    pub passphrase: Option<String>,
}

/// 交易所配置集合
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangesConfig {
    pub binance: ExchangeCredentials,
    pub okx: OkxCredentials,
    #[serde(default)]
    pub hyperliquid: Option<HyperliquidCredentials>,
}

/// OKX 特定凭证 (需要 passphrase)
#[derive(Debug, Clone, Deserialize)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

/// Hyperliquid 凭证 (使用私钥签名)
#[derive(Debug, Clone, Deserialize)]
pub struct HyperliquidCredentials {
    /// 钱包地址 (0x...)
    pub wallet_address: String,
    /// 私钥 (不含 0x 前缀)
    pub private_key: String,
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

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub symbols: Vec<String>,
    pub funding_arb: FundingArbConfig,
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

impl AppConfig {
    /// 从配置文件加载 (支持 JSON 和 TOML)
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)?;

        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Ok(serde_json::from_str(&content)?),
            Some("toml") => Ok(toml::from_str(&content)?),
            Some(ext) => Err(ConfigError::UnsupportedFormat(ext.to_string())),
            None => Err(ConfigError::UnsupportedFormat("unknown".to_string())),
        }
    }

    /// 解析 symbols 字符串为 Symbol 列表
    pub fn parse_symbols(&self) -> Vec<Symbol> {
        self.strategy
            .symbols
            .iter()
            .filter_map(|s| Symbol::from_canonical(s))
            .collect()
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
