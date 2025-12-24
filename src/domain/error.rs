use crate::domain::model::Exchange;
use rust_decimal::Decimal;
use std::time::Duration;
use thiserror::Error;

/// 交易所错误类型
#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("Connection failed to {0}: {1}")]
    ConnectionFailed(Exchange, String),

    #[error("Authentication failed for {0}")]
    AuthenticationFailed(Exchange),

    #[error("Rate limited on {0}, retry after {1:?}")]
    RateLimited(Exchange, Duration),

    #[error("Order rejected on {0}: {1}")]
    OrderRejected(Exchange, String),

    #[error("Insufficient balance on {0}: need {1}, have {2}")]
    InsufficientBalance(Exchange, Decimal, Decimal),

    #[error("Symbol not found on {0}: {1}")]
    SymbolNotFound(Exchange, String),

    #[error("API error from {0}: code={1}, msg={2}")]
    ApiError(Exchange, i32, String),

    #[error("Websocket error: {0}")]
    WebSocketError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Timeout: {0}")]
    Timeout(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for ExchangeError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        ExchangeError::WebSocketError(e.to_string())
    }
}

impl From<reqwest::Error> for ExchangeError {
    fn from(e: reqwest::Error) -> Self {
        ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string())
    }
}

impl From<serde_json::Error> for ExchangeError {
    fn from(e: serde_json::Error) -> Self {
        ExchangeError::ParseError(e.to_string())
    }
}
