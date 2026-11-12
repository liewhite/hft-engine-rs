//! 币安 × Hyperliquid 资金费套利（#668）
mod config;
mod signal;
mod strategy;

pub use config::{FundingArbConfig, PairConfig};
pub use signal::{SettledFundingWindow, FUNDING_WINDOW_MS};
pub use strategy::FundingArbStrategy;

