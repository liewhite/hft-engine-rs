//! OKX 交易所配置实现

use crate::domain::Exchange;
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsPush,
};
use crate::exchange::okx::{WS_PRIVATE_URL, WS_PUBLIC_URL};
use crate::exchange::subscriber::{ExchangeConfig, ParsedMessage, SubscriptionKind};
use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

/// OKX 凭证
#[derive(Clone)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl OkxCredentials {
    /// WebSocket 登录签名
    fn sign_ws_login(&self, timestamp: &str) -> String {
        let message = format!("{}GET/users/self/verify", timestamp);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.secret.as_bytes()).expect("HMAC accepts any size");
        mac.update(message.as_bytes());
        let result = mac.finalize();
        general_purpose::STANDARD.encode(result.into_bytes())
    }

    /// Unix 时间戳 (秒)
    fn unix_timestamp() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }
}

/// OKX 交易所配置
pub struct OkxConfig;

impl ExchangeConfig for OkxConfig {
    const EXCHANGE: Exchange = Exchange::OKX;

    const PUBLIC_WS_URL: &'static str = WS_PUBLIC_URL;

    const PRIVATE_WS_URL: &'static str = WS_PRIVATE_URL;

    const MAX_SUBSCRIPTIONS_PER_CONN: usize = 100;

    type Credentials = OkxCredentials;

    fn build_subscribe_msg(kinds: &[SubscriptionKind]) -> String {
        let mut args = Vec::new();

        for kind in kinds {
            match kind {
                SubscriptionKind::FundingRate { symbol } => {
                    let inst_id = symbol.to_okx();
                    args.push(json!({
                        "channel": "funding-rate",
                        "instId": inst_id
                    }));
                }
                SubscriptionKind::BBO { symbol } => {
                    let inst_id = symbol.to_okx();
                    args.push(json!({
                        "channel": "bbo-tbt",
                        "instId": inst_id
                    }));
                }
                SubscriptionKind::Private => {
                    // 私有频道订阅
                    args.push(json!({"channel": "positions", "instType": "SWAP"}));
                    args.push(json!({"channel": "account"}));
                    args.push(json!({"channel": "orders", "instType": "SWAP"}));
                }
            }
        }

        if args.is_empty() {
            return String::new();
        }

        json!({
            "op": "subscribe",
            "args": args
        })
        .to_string()
    }

    fn build_unsubscribe_msg(kinds: &[SubscriptionKind]) -> String {
        let mut args = Vec::new();

        for kind in kinds {
            match kind {
                SubscriptionKind::FundingRate { symbol } => {
                    let inst_id = symbol.to_okx();
                    args.push(json!({
                        "channel": "funding-rate",
                        "instId": inst_id
                    }));
                }
                SubscriptionKind::BBO { symbol } => {
                    let inst_id = symbol.to_okx();
                    args.push(json!({
                        "channel": "bbo-tbt",
                        "instId": inst_id
                    }));
                }
                SubscriptionKind::Private => {
                    args.push(json!({"channel": "positions", "instType": "SWAP"}));
                    args.push(json!({"channel": "account"}));
                    args.push(json!({"channel": "orders", "instType": "SWAP"}));
                }
            }
        }

        if args.is_empty() {
            return String::new();
        }

        json!({
            "op": "unsubscribe",
            "args": args
        })
        .to_string()
    }

    fn parse_message(raw: &str) -> Option<ParsedMessage> {
        // 尝试解析为 JSON
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;

        // 检查是否是事件响应 (subscribe/login 确认等)
        if let Some(event) = value.get("event").and_then(|e| e.as_str()) {
            match event {
                "subscribe" | "unsubscribe" | "channel-conn-count" => {
                    return Some(ParsedMessage::Subscribed);
                }
                "login" => {
                    let code = value.get("code").and_then(|c| c.as_str());
                    if code == Some("0") {
                        tracing::info!("OKX login successful");
                        return Some(ParsedMessage::Subscribed);
                    } else {
                        let msg = value.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
                        tracing::error!(code = ?code, msg = %msg, "OKX login failed");
                        return Some(ParsedMessage::Ignored);
                    }
                }
                "error" => {
                    let code = value.get("code").and_then(|c| c.as_str()).unwrap_or("unknown");
                    let msg = value.get("msg").and_then(|m| m.as_str()).unwrap_or("unknown");
                    tracing::error!(code = %code, msg = %msg, "OKX error");
                    return Some(ParsedMessage::Ignored);
                }
                _ => return Some(ParsedMessage::Ignored),
            }
        }

        // 检查是否是数据推送
        let arg = value.get("arg")?;
        let channel = arg.get("channel")?.as_str()?;

        match channel {
            "funding-rate" => {
                let push: WsPush<FundingRateData> = serde_json::from_str(raw).ok()?;
                let data = push.data.first()?;
                let rate = data.to_funding_rate();
                let symbol = rate.symbol.clone();
                Some(ParsedMessage::FundingRate { symbol, rate })
            }
            "bbo-tbt" => {
                let push: WsPush<BboData> = serde_json::from_str(raw).ok()?;
                let inst_id = push.arg.inst_id.as_ref()?;
                let data = push.data.first()?;
                let bbo = data.to_bbo(inst_id);
                let symbol = bbo.symbol.clone();
                Some(ParsedMessage::BBO { symbol, bbo })
            }
            "positions" => {
                let push: WsPush<PositionData> = serde_json::from_str(raw).ok()?;
                let data = push.data.first()?;
                let position = data.to_position();
                let symbol = position.symbol.clone();
                Some(ParsedMessage::Position { symbol, position })
            }
            "account" => {
                let push: WsPush<AccountData> = serde_json::from_str(raw).ok()?;
                let data = push.data.first()?;

                // 先返回 equity
                let equity = data.to_equity();
                // 实际上还应该返回 balance，但这里简化处理只返回 equity
                Some(ParsedMessage::Equity(equity))
            }
            "orders" => {
                let push: WsPush<OrderPushData> = serde_json::from_str(raw).ok()?;
                let data = push.data.first()?;
                let update = data.to_order_update();
                let symbol = update.symbol.clone();
                Some(ParsedMessage::OrderUpdate { symbol, update })
            }
            _ => Some(ParsedMessage::Ignored),
        }
    }

    fn build_auth_msg(credentials: &Self::Credentials) -> String {
        let timestamp = OkxCredentials::unix_timestamp();
        let sign = credentials.sign_ws_login(&timestamp);

        json!({
            "op": "login",
            "args": [{
                "apiKey": credentials.api_key,
                "passphrase": credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string()
    }
}
