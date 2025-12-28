//! Binance 交易所配置实现

use crate::domain::Exchange;
use crate::exchange::binance::codec::{
    AccountUpdate, BookTicker, MarkPriceUpdate, OrderTradeUpdate, WsResponse,
};
use crate::exchange::binance::WS_PUBLIC_URL;
use crate::exchange::subscriber::{ExchangeConfig, ParsedMessage, SubscriptionKind};
use serde_json::json;

/// Binance 凭证
#[derive(Clone)]
pub struct BinanceCredentials {
    pub api_key: String,
    pub secret: String,
    /// ListenKey (由 REST API 获取，用于 private WebSocket)
    pub listen_key: Option<String>,
}

/// Binance 交易所配置
pub struct BinanceConfig;

impl ExchangeConfig for BinanceConfig {
    const EXCHANGE: Exchange = Exchange::Binance;

    const PUBLIC_WS_URL: &'static str = WS_PUBLIC_URL;

    // Binance private 使用 ListenKey URL
    const PRIVATE_WS_URL: &'static str = "wss://fstream.binance.com/ws";

    const MAX_SUBSCRIPTIONS_PER_CONN: usize = 200;

    type Credentials = BinanceCredentials;

    fn build_subscribe_msg(kinds: &[SubscriptionKind]) -> String {
        let mut streams: Vec<String> = Vec::new();

        for kind in kinds {
            match kind {
                SubscriptionKind::FundingRate { symbol } => {
                    let s = symbol.to_binance().to_lowercase();
                    streams.push(format!("{}@markPrice@1s", s));
                }
                SubscriptionKind::BBO { symbol } => {
                    let s = symbol.to_binance().to_lowercase();
                    streams.push(format!("{}@bookTicker", s));
                }
                SubscriptionKind::Private => {
                    // Private 订阅不需要额外的消息，连接时自动订阅所有私有数据
                }
            }
        }

        if streams.is_empty() {
            return String::new();
        }

        json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": 1
        })
        .to_string()
    }

    fn build_unsubscribe_msg(kinds: &[SubscriptionKind]) -> String {
        let mut streams: Vec<String> = Vec::new();

        for kind in kinds {
            match kind {
                SubscriptionKind::FundingRate { symbol } => {
                    let s = symbol.to_binance().to_lowercase();
                    streams.push(format!("{}@markPrice@1s", s));
                }
                SubscriptionKind::BBO { symbol } => {
                    let s = symbol.to_binance().to_lowercase();
                    streams.push(format!("{}@bookTicker", s));
                }
                SubscriptionKind::Private => {
                    // Private 订阅无法取消
                }
            }
        }

        if streams.is_empty() {
            return String::new();
        }

        json!({
            "method": "UNSUBSCRIBE",
            "params": streams,
            "id": 2
        })
        .to_string()
    }

    fn parse_message(raw: &str) -> Option<ParsedMessage> {
        // 尝试解析为 JSON
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;

        // 检查是否是订阅响应
        if value.get("id").is_some() {
            if let Ok(resp) = serde_json::from_str::<WsResponse>(raw) {
                if resp.error.is_some() {
                    tracing::error!(raw = %raw, "Binance subscribe error");
                }
                return Some(ParsedMessage::Subscribed);
            }
        }

        // 根据事件类型解析
        let event_type = value.get("e")?.as_str()?;

        match event_type {
            "markPriceUpdate" => {
                let update: MarkPriceUpdate = serde_json::from_str(raw).ok()?;
                let symbol = update.symbol()?;
                let next_funding_time = update.t as u64;

                // 使用默认 8h 间隔，ExchangeActor 会根据状态更新
                let rate = update.to_funding_rate(8.0);
                Some(ParsedMessage::FundingRate {
                    symbol,
                    rate,
                    next_funding_time: Some(next_funding_time),
                })
            }
            "bookTicker" => {
                let ticker: BookTicker = serde_json::from_str(raw).ok()?;
                let bbo = ticker.to_bbo();
                let symbol = bbo.symbol.clone();
                Some(ParsedMessage::BBO { symbol, bbo })
            }
            "ACCOUNT_UPDATE" => {
                let update: AccountUpdate = serde_json::from_str(raw).ok()?;

                // 返回第一个 position 更新 (简化处理)
                // 实际上可能需要返回多个
                if let Some(pos_data) = update.a.positions.first() {
                    let position = pos_data.to_position();
                    let symbol = position.symbol.clone();
                    return Some(ParsedMessage::Position { symbol, position });
                }

                // 返回第一个 balance 更新
                if let Some(bal_data) = update.a.balances.first() {
                    let balance = bal_data.to_balance();
                    return Some(ParsedMessage::Balance(balance));
                }

                Some(ParsedMessage::Ignored)
            }
            "ORDER_TRADE_UPDATE" => {
                let update: OrderTradeUpdate = serde_json::from_str(raw).ok()?;
                let order_update = update.to_order_update();
                let symbol = order_update.symbol.clone();
                Some(ParsedMessage::OrderUpdate {
                    symbol,
                    update: order_update,
                })
            }
            _ => Some(ParsedMessage::Ignored),
        }
    }

    fn build_auth_msg(_credentials: &Self::Credentials) -> String {
        // Binance 不需要发送认证消息，使用 ListenKey URL
        // 但我们需要在 URL 中包含 ListenKey
        // 这里返回空字符串，实际的 URL 构建在连接时处理
        String::new()
    }
}

impl BinanceConfig {
    /// 构建 private WebSocket URL (包含 ListenKey)
    pub fn build_private_ws_url(listen_key: &str) -> String {
        format!("{}/{}", Self::PRIVATE_WS_URL, listen_key)
    }
}
