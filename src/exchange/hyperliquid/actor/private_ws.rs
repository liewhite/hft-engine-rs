//! HyperliquidPrivateWsActor - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 自动订阅账户频道 (webData3, orderUpdates)
//! - 直接解析消息并发布到 IncomePubSub
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址

use crate::domain::{now_ms, Balance, Exchange, ExchangeError};
use crate::engine::IncomePubSub;
use crate::exchange::client::WsError;
use crate::exchange::hyperliquid::codec::{ClearinghouseState, WsOrderUpdate, WsUserFills};
use crate::exchange::hyperliquid::symbol::belongs_to_dex;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde_json::json;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// HyperliquidPrivateWsActor 初始化参数
pub struct HyperliquidPrivateWsActorArgs {
    /// 用户钱包地址 (0x...)
    pub wallet_address: String,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Perp DEX 名称 ("" = 默认 perp DEX)
    pub dex: String,
}

/// HyperliquidPrivateWsActor - 账户 WebSocket Actor
pub struct HyperliquidPrivateWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// 本 client 接入的 perp DEX：**账户级**推送（orderUpdates / userFills）不按 dex
    /// 下发，必须自己过滤（与 REST 侧 `belongs_to_dex` 同口径），否则同一钱包在其他
    /// dex 上的成交会被剥掉前缀后当成本 dex 同名标的的 Fill，直接污染「基线 + Fill」
    /// 维护的持仓。
    dex: String,
    /// 发送消息到 ws_loop 的 channel
    #[allow(dead_code)]
    ws_tx: Option<mpsc::Sender<String>>,
}

impl HyperliquidPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&mut self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_private_message(raw, &self.dex, local_ts)?;
        for event in events {
            if let Err(e) = self.income_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to IncomePubSub");
            }
        }
        Ok(())
    }
}

impl Actor for HyperliquidPrivateWsActor {
    type Args = HyperliquidPrivateWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 2. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 3. 创建入站消息 channel
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 4. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        // 5. 订阅账户频道
        // clearinghouseState: perp 账户状态 (positions, equity, margin)
        let subscribe_clearinghouse = json!({
            "method": "subscribe",
            "subscription": {
                "type": "clearinghouseState",
                "user": args.wallet_address,
                "dex": args.dex
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_clearinghouse)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send clearinghouseState: {e}")))?;

        // orderUpdates: 订单更新
        let subscribe_orders = json!({
            "method": "subscribe",
            "subscription": {
                "type": "orderUpdates",
                "user": args.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_orders)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send orderUpdates: {e}")))?;

        // userFills: 成交推送
        let subscribe_fills = json!({
            "method": "subscribe",
            "subscription": {
                "type": "userFills",
                "user": args.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_fills)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send userFills: {e}")))?;

        tracing::info!(
            wallet = %args.wallet_address,
            "HyperliquidPrivateWsActor started, subscribed to clearinghouseState, orderUpdates and userFills"
        );

        Ok(Self {
            income_pubsub: args.income_pubsub,
            dex: args.dex,
            ws_tx: Some(outgoing_tx),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("HyperliquidPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Result<String, WsError>, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        crate::dispatch_ws_stream_message!(self, msg, ctx, "Hyperliquid");
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_private_message(raw: &str, dex: &str, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是订阅确认
    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            "subscriptionResponse" => {
                // 订阅响应，忽略
                return Ok(Vec::new());
            }
            "clearinghouseState" => {
                // perp 账户状态：只取 equity / notional / withdrawable，持仓不入事件流
                let data = &value["data"];
                return parse_clearinghouse_state(data, local_ts);
            }
            "orderUpdates" => {
                // 订单更新
                let data = &value["data"];
                return parse_order_updates(data, dex, local_ts);
            }
            "userFills" => {
                // 成交推送
                let data = &value["data"];
                return parse_user_fills(data, dex, local_ts);
            }
            _ => {
                tracing::debug!(channel, "Unknown Hyperliquid private channel");
                return Ok(Vec::new());
            }
        }
    }

    // pong 消息
    if value.get("method").map(|v| v.as_str()) == Some(Some("pong")) {
        return Ok(Vec::new());
    }

    // 其他未知消息
    tracing::debug!(raw, "Unhandled Hyperliquid private message");
    Ok(Vec::new())
}

/// 解析 clearinghouseState 消息 (perp 账户状态)
///
/// **只取账户级读数（equity / notional / withdrawable），不产出持仓事件。**
/// 本推送里的 `assetPositions` 是持仓**快照**，而持仓的维护模型是「启动期 REST 基线 +
/// 之后全程 Fill 累加」（见 [`ExchangeEventData::PositionBaseline`]）：快照既当不了基线
/// （基线只能来自 ManagerActor），也不能参与增量（会与 Fill 重复计算）。要校验本地持仓
/// 是否漂移，走 `PositionReport` 那条独立通道。
///
/// 之所以不能像 OKX 那样干脆退订整个频道：equity / notional / withdrawable 都只有这里有。
fn parse_clearinghouse_state(
    data: &serde_json::Value,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // clearinghouseState 订阅返回结构: { clearinghouseState: {...}, user: "...", dex: "..." }
    // 需要提取 clearinghouseState 字段
    let state_value = data.get("clearinghouseState").unwrap_or(data);
    let state: ClearinghouseState = serde_json::from_value(state_value.clone())
        .map_err(|e| WsError::ParseError(format!("clearinghouseState parse: {}", e)))?;

    tracing::debug!(
        positions_count = state.asset_positions.len(),
        coins = ?state.asset_positions.iter().map(|w| &w.position.coin).collect::<Vec<_>>(),
        "Hyperliquid clearinghouseState received (持仓部分不入事件流，见函数文档)"
    );

    // 解析账户信息 (equity + notional)
    let equity = f64::from_str(&state.margin_summary.account_value)
        .map_err(|_| WsError::ParseError(format!("Failed to parse Hyperliquid accountValue: {}", state.margin_summary.account_value)))?;
    let notional = f64::from_str(&state.margin_summary.total_ntl_pos)
        .map_err(|_| WsError::ParseError(format!("Failed to parse Hyperliquid total_ntl_pos: {}", state.margin_summary.total_ntl_pos)))?;
    events.push(IncomeEvent {
        exchange_ts: local_ts,
        local_ts,
        data: ExchangeEventData::AccountInfo {
            exchange: Exchange::Hyperliquid,
            equity,
            notional,
        },
    });

    // 解析可用余额
    let withdrawable = f64::from_str(&state.withdrawable)
        .map_err(|_| WsError::ParseError(format!("invalid withdrawable: {}", state.withdrawable)))?;
    events.push(IncomeEvent {
        exchange_ts: local_ts,
        local_ts,
        data: ExchangeEventData::Balance(Balance {
            exchange: Exchange::Hyperliquid,
            asset: "USDC".to_string(),
            available: withdrawable,
            frozen: 0.0,
        }),
    });

    Ok(events)
}

/// 解析 orderUpdates 消息。
///
/// **按 dex 过滤在剥前缀之前**：orderUpdates 是账户级推送（一个钱包在所有 perp dex 上
/// 的订单都从这里下发），与 REST 侧同口径（[`belongs_to_dex`]）。不过滤的话，xyz dex
/// 的 "xyz:AAPL" 会被剥成 "AAPL"，与默认 dex 的同名标的混在一起。
fn parse_order_updates(
    data: &serde_json::Value,
    dex: &str,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // orderUpdates 必须是一个数组
    let updates = data.as_array()
        .ok_or_else(|| WsError::ParseError(format!("orderUpdates is not an array: {}", data)))?;

    for update in updates {
        let order_update: WsOrderUpdate = serde_json::from_value(update.clone())
            .map_err(|e| WsError::ParseError(format!("orderUpdate parse: {}", e)))?;
        if !belongs_to_dex(&order_update.order.coin, dex) {
            tracing::debug!(
                coin = %order_update.order.coin,
                dex,
                "orderUpdate 属于其他 perp dex，跳过"
            );
            continue;
        }
        let update = order_update.to_order_update()
            ?;
        events.push(IncomeEvent {
            exchange_ts: update.timestamp,
            local_ts,
            data: ExchangeEventData::OrderUpdate(update),
        });
    }

    Ok(events)
}

/// 解析 userFills 消息。
///
/// 与 [`parse_order_updates`] 同理，**必须按 dex 过滤**：userFills 是账户级推送，
/// 其他 dex 的成交混进来会直接污染「基线 + Fill」维护的持仓 —— 这正是持仓对账通道
/// 要抓的那类错账，不该由适配层制造。
fn parse_user_fills(
    data: &serde_json::Value,
    dex: &str,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let user_fills: WsUserFills = serde_json::from_value(data.clone())
        .map_err(|e| WsError::ParseError(format!("userFills parse: {}", e)))?;

    // 忽略 snapshot（初始快照），只处理增量更新
    if user_fills.is_snapshot {
        tracing::debug!(
            user = %user_fills.user,
            fills_count = user_fills.fills.len(),
            "Received userFills snapshot, ignoring"
        );
        return Ok(Vec::new());
    }

    let mut events = Vec::new();
    for ws_fill in &user_fills.fills {
        if !belongs_to_dex(&ws_fill.coin, dex) {
            tracing::debug!(coin = %ws_fill.coin, dex, "fill 属于其他 perp dex，跳过");
            continue;
        }
        let fill = ws_fill.to_fill()
            ?;
        events.push(IncomeEvent {
            exchange_ts: fill.timestamp,
            local_ts,
            data: ExchangeEventData::Fill(fill),
        });
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fill_json(coin: &str) -> serde_json::Value {
        json!({
            "coin": coin,
            "px": "100.0",
            "sz": "1.0",
            "side": "B",
            "time": 1u64,
            "oid": 1u64,
            "fee": "0.01",
            "closedPnl": "0.0"
        })
    }

    fn order_update_json(coin: &str) -> serde_json::Value {
        json!({
            "order": {
                "coin": coin,
                "side": "B",
                "limitPx": "100.0",
                "sz": "1.0",
                "oid": 1u64,
                "timestamp": 1u64,
                "origSz": "1.0"
            },
            "status": "open",
            "statusTimestamp": 1u64
        })
    }

    /// **错账防线**：userFills 是账户级推送，其他 perp dex 的成交必须被过滤 ——
    /// 否则 "xyz:AAPL" 会被剥前缀成 "AAPL"，当成本 dex 同名标的的 Fill 累进持仓。
    #[test]
    fn fills_from_other_dexes_are_filtered_out() {
        let data = json!({
            "user": "0xabc",
            "fills": [fill_json("ETH"), fill_json("xyz:AAPL")]
        });
        // 默认 dex 只收裸 coin
        let events = parse_user_fills(&data, "", 1).unwrap();
        assert_eq!(events.len(), 1);
        // 具名 dex 只收自己前缀的，裸 coin（默认 dex 的同名标的）也不能收
        let events = parse_user_fills(&data, "xyz", 1).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].data {
            ExchangeEventData::Fill(f) => assert_eq!(f.symbol, "AAPL"),
            other => panic!("expected fill, got {other:?}"),
        }
    }

    /// orderUpdates 同口径过滤
    #[test]
    fn order_updates_from_other_dexes_are_filtered_out() {
        let data = json!([order_update_json("ETH"), order_update_json("xyz:AAPL")]);
        let events = parse_order_updates(&data, "", 1).unwrap();
        assert_eq!(events.len(), 1);
        let events = parse_order_updates(&data, "xyz", 1).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].data {
            ExchangeEventData::OrderUpdate(u) => assert_eq!(u.symbol, "AAPL"),
            other => panic!("expected order update, got {other:?}"),
        }
    }
}
