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
}

/// OKX 特定凭证 (需要 passphrase)
#[derive(Debug, Clone, Deserialize)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl ExchangesConfig {
    /// 获取已配置的交易所列表
    pub fn enabled_exchanges(&self) -> Vec<Exchange> {
        // 当前配置中 binance 和 okx 都是必填，所以都启用
        vec![Exchange::Binance, Exchange::OKX]
    }
}

/// 策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    pub symbols: Vec<String>,
    #[serde(default)]
    pub funding_arb: FundingArbStrategyConfig,
}

/// 资金费率套利策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct FundingArbStrategyConfig {
    #[serde(default = "default_min_spread")]
    pub min_spread: f64,
    #[serde(default = "default_max_spread")]
    pub max_spread: f64,
    #[serde(default = "default_close_spread")]
    pub close_spread: f64,
    #[serde(default = "default_max_notional")]
    pub max_notional: f64,
    #[serde(default = "default_max_quantity")]
    pub max_quantity: f64,
    #[serde(default = "default_order_timeout_ms")]
    pub order_timeout_ms: u64,
    #[serde(default = "default_unhedge_ratio_threshold")]
    pub unhedge_ratio_threshold: f64,
    #[serde(default = "default_unhedge_value_threshold")]
    pub unhedge_value_threshold: f64,
}

fn default_min_spread() -> f64 {
    0.0005
}
fn default_max_spread() -> f64 {
    0.002
}
fn default_close_spread() -> f64 {
    0.0002
}
fn default_max_notional() -> f64 {
    1000.0
}
fn default_max_quantity() -> f64 {
    1.0
}
fn default_order_timeout_ms() -> u64 {
    10_000
}
fn default_unhedge_ratio_threshold() -> f64 {
    0.01 // 1%
}
fn default_unhedge_value_threshold() -> f64 {
    50.0 // 50 USD
}

impl Default for FundingArbStrategyConfig {
    fn default() -> Self {
        Self {
            min_spread: default_min_spread(),
            max_spread: default_max_spread(),
            close_spread: default_close_spread(),
            max_notional: default_max_notional(),
            max_quantity: default_max_quantity(),
            order_timeout_ms: default_order_timeout_ms(),
            unhedge_ratio_threshold: default_unhedge_ratio_threshold(),
            unhedge_value_threshold: default_unhedge_value_threshold(),
        }
    }
}

impl From<FundingArbStrategyConfig> for FundingArbConfig {
    fn from(cfg: FundingArbStrategyConfig) -> Self {
        Self {
            min_spread: cfg.min_spread,
            max_spread: cfg.max_spread,
            close_spread: cfg.close_spread,
            max_notional: cfg.max_notional,
            max_quantity: cfg.max_quantity,
            order_timeout_ms: cfg.order_timeout_ms,
            unhedge_ratio_threshold: cfg.unhedge_ratio_threshold,
            unhedge_value_threshold: cfg.unhedge_value_threshold,
        }
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

    /// 从环境变量加载配置
    pub fn from_env() -> Result<Self, ConfigError> {
        let binance = ExchangeCredentials {
            api_key: std::env::var("BINANCE_API_KEY")
                .map_err(|_| ConfigError::MissingEnv("BINANCE_API_KEY"))?,
            secret: std::env::var("BINANCE_SECRET")
                .map_err(|_| ConfigError::MissingEnv("BINANCE_SECRET"))?,
            passphrase: None,
        };

        let okx = OkxCredentials {
            api_key: std::env::var("OKX_API_KEY")
                .map_err(|_| ConfigError::MissingEnv("OKX_API_KEY"))?,
            secret: std::env::var("OKX_SECRET")
                .map_err(|_| ConfigError::MissingEnv("OKX_SECRET"))?,
            passphrase: std::env::var("OKX_PASSPHRASE")
                .map_err(|_| ConfigError::MissingEnv("OKX_PASSPHRASE"))?,
        };

        let symbols_str =
            std::env::var("SYMBOLS").unwrap_or_else(|_| "BTC_USDT,ETH_USDT".to_string());
        let symbols: Vec<String> = symbols_str.split(',').map(|s| s.trim().to_string()).collect();

        Ok(Self {
            exchanges: ExchangesConfig { binance, okx },
            strategy: StrategyConfig {
                symbols,
                funding_arb: FundingArbStrategyConfig::default(),
            },
            engine: EngineConfig::default(),
        })
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

    #[error("Missing environment variable: {0}")]
    MissingEnv(&'static str),

    #[error("Unsupported config format: {0}")]
    UnsupportedFormat(String),
}
