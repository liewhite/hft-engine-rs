//! OKX 交易所配置实现

use crate::domain::Exchange;
use crate::exchange::actor::public_ws::{ConnectionId, WsDataSink, WsError};
use crate::exchange::actor::private_ws::PrivateConnectionHandle;
use crate::exchange::actor::{OkxPrivateHandle, OkxPrivateWsActor, OkxPrivateWsActorArgs};
use crate::exchange::okx::codec::{
    AccountData, BboData, FundingRateData, OrderPushData, PositionData, WsPush,
};
use crate::exchange::okx::{OkxRestClient, WS_PUBLIC_URL};
use crate::exchange::subscriber::{ExchangeConfig, ParsedMessage, SubscriptionKind};
use async_trait::async_trait;
use kameo::actor::ActorRef;
use serde_json::json;
use std::sync::Arc;

/// OKX 凭证
#[derive(Clone)]
pub struct OkxCredentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

/// OKX 交易所配置
pub struct OkxConfig;

#[async_trait]
impl ExchangeConfig for OkxConfig {
    const EXCHANGE: Exchange = Exchange::OKX;

    const PUBLIC_WS_URL: &'static str = WS_PUBLIC_URL;

    const MAX_SUBSCRIPTIONS_PER_CONN: usize = 100;

    type Credentials = OkxCredentials;
    type RestClient = OkxRestClient;

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
                Some(ParsedMessage::FundingRate {
                    symbol,
                    rate,
                    next_funding_time: None, // OKX 使用固定 8h 间隔
                })
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

    async fn create_private_connection<S: WsDataSink>(
        credentials: &Self::Credentials,
        _rest_client: Arc<Self::RestClient>,
        data_sink: Arc<S>,
        conn_id: ConnectionId,
        link_to: ActorRef<impl kameo::Actor>,
    ) -> Result<Box<dyn PrivateConnectionHandle>, WsError> {
        let actor = OkxPrivateWsActor::new(OkxPrivateWsActorArgs {
            conn_id,
            credentials: credentials.clone(),
            data_sink,
        });

        let actor_ref = kameo::actor::spawn_link(&link_to, actor).await;
        Ok(Box::new(OkxPrivateHandle::new(actor_ref, conn_id)))
    }
}
