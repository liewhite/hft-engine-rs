use crate::domain::Symbol;
use crate::strategy::FundingArbConfig;
use rust_decimal::Decimal;
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
    pub min_spread: Decimal,
    #[serde(default = "default_max_spread")]
    pub max_spread: Decimal,
    #[serde(default = "default_close_spread")]
    pub close_spread: Decimal,
    #[serde(default = "default_base_quantity")]
    pub base_quantity: Decimal,
    #[serde(default = "default_max_quantity")]
    pub max_quantity: Decimal,
}

fn default_min_spread() -> Decimal {
    Decimal::new(5, 4)
}
fn default_max_spread() -> Decimal {
    Decimal::new(20, 4)
}
fn default_close_spread() -> Decimal {
    Decimal::new(2, 4)
}
fn default_base_quantity() -> Decimal {
    Decimal::new(1, 2)
}
fn default_max_quantity() -> Decimal {
    Decimal::new(1, 0)
}

impl Default for FundingArbStrategyConfig {
    fn default() -> Self {
        Self {
            min_spread: default_min_spread(),
            max_spread: default_max_spread(),
            close_spread: default_close_spread(),
            base_quantity: default_base_quantity(),
            max_quantity: default_max_quantity(),
        }
    }
}

impl From<FundingArbStrategyConfig> for FundingArbConfig {
    fn from(cfg: FundingArbStrategyConfig) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        Self {
            min_spread: cfg.min_spread.to_f64().unwrap_or(0.0),
            max_spread: cfg.max_spread.to_f64().unwrap_or(0.0),
            close_spread: cfg.close_spread.to_f64().unwrap_or(0.0),
            base_quantity: cfg.base_quantity.to_f64().unwrap_or(0.0),
            max_quantity: cfg.max_quantity.to_f64().unwrap_or(0.0),
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
