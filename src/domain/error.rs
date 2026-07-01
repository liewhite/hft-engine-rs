use crate::domain::models::Exchange;
use std::time::Duration;
use thiserror::Error;

/// 订单被交易所拒绝的结构化原因
///
/// 由各交易所适配层在错误边界翻译（已知错误码可直接构造具体变体，只有文本消息时用
/// [`RejectReason::classify`] 启发式分类）。编排层按枚举判断，不再字符串嗅探。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// reduce-only 单因仓位已平而被拒——通常无害（平仓目标已达成）
    ReduceOnlyClosed,
    /// 其它拒绝原因，保留原始交易所消息
    Other(String),
}

impl RejectReason {
    /// 从交易所返回的原始消息启发式分类。
    /// 各适配层若已知错误码，应直接构造具体变体而非依赖此方法。
    pub fn classify(msg: &str) -> Self {
        let lower = msg.to_lowercase();
        if lower.contains("reduce only") || lower.contains("reduceonly") {
            RejectReason::ReduceOnlyClosed
        } else {
            RejectReason::Other(msg.to_string())
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::ReduceOnlyClosed => {
                write!(f, "reduce-only rejected (position already closed)")
            }
            RejectReason::Other(m) => write!(f, "{m}"),
        }
    }
}

/// 交易所错误类型
///
/// `Clone` 是必需的：kameo `wait_for_startup_result()` 约束 `A::Error: Clone`，
/// actor 以 `ExchangeError` 作为 `type Error` 时需要能克隆启动错误向上传播。
#[derive(Debug, Clone, Error)]
pub enum ExchangeError {
    #[error("Connection failed to {0}: {1}")]
    ConnectionFailed(Exchange, String),

    #[error("Authentication failed for {0}")]
    AuthenticationFailed(Exchange),

    #[error("Rate limited on {0}, retry after {1:?}")]
    RateLimited(Exchange, Duration),

    #[error("Order rejected on {0}: {1}")]
    OrderRejected(Exchange, RejectReason),

    #[error("Insufficient balance on {0}: need {1}, have {2}")]
    InsufficientBalance(Exchange, f64, f64),

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

    #[error("{0}")]
    Other(String),
}

impl From<tokio_tungstenite::tungstenite::Error> for ExchangeError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        ExchangeError::WebSocketError(e.to_string())
    }
}

// reqwest::Error 不实现 From，需要在各交易所 REST 客户端中显式处理
// 以确保正确标记交易所来源

impl From<serde_json::Error> for ExchangeError {
    fn from(e: serde_json::Error) -> Self {
        ExchangeError::ParseError(e.to_string())
    }
}
