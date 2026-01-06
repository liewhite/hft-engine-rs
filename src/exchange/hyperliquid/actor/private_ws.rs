//! HyperliquidPrivateWsActor - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 自动订阅账户频道 (webData3, orderUpdates)
//! - 直接解析消息并发送到 EventSink
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址

use crate::domain::{now_ms, Balance, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, WsError};
use crate::exchange::hyperliquid::codec::{ClearinghouseState, WsOrderUpdate};
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// HyperliquidPrivateWsActor 初始化参数
pub struct HyperliquidPrivateWsActorArgs {
    /// 用户钱包地址 (0x...)
    pub wallet_address: String,
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// HyperliquidPrivateWsActor - 账户 WebSocket Actor
pub struct HyperliquidPrivateWsActor {
    /// 用户钱包地址
    wallet_address: String,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    #[allow(dead_code)]
    ws_tx: Option<mpsc::Sender<String>>,
}

impl HyperliquidPrivateWsActor {
    /// 创建新的 HyperliquidPrivateWsActor
    pub fn new(args: HyperliquidPrivateWsActorArgs) -> Self {
        Self {
            wallet_address: args.wallet_address,
            event_sink: args.event_sink,
            symbol_metas: args.symbol_metas,
            ws_tx: None,
        }
    }

    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) {
        let local_ts = now_ms();
        match parse_private_message(raw, local_ts) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %raw, "Failed to parse Hyperliquid private message");
            }
        }
    }
}

impl Actor for HyperliquidPrivateWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidPrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 2. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx.clone());

        // 3. 创建入站消息 channel
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 4. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        // 5. 订阅账户频道
        // webData3: 账户状态 (positions, balance)
        let subscribe_web_data = json!({
            "method": "subscribe",
            "subscription": {
                "type": "webData3",
                "user": self.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_web_data)
            .await
            .map_err(|_| WsError::Network("Failed to send webData3 subscription".to_string()))?;

        // orderUpdates: 订单更新
        let subscribe_orders = json!({
            "method": "subscribe",
            "subscription": {
                "type": "orderUpdates",
                "user": self.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_orders)
            .await
            .map_err(|_| WsError::Network("Failed to send orderUpdates subscription".to_string()))?;

        tracing::info!(
            wallet = %self.wallet_address,
            "HyperliquidPrivateWsActor started, subscribed to webData3 and orderUpdates"
        );

        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
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
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 直接解析并发送到 EventSink
                self.handle_message(&data).await;
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Private WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 异常退出，kill actor 触发级联退出
                tracing::error!("Private WebSocket stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_private_message(raw: &str, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
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
            "webData3" => {
                // 账户状态 (positions, balance)
                let data = &value["data"];
                return parse_web_data3(data, local_ts);
            }
            "orderUpdates" => {
                // 订单更新
                let data = &value["data"];
                return parse_order_updates(data, local_ts);
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

/// 解析 webData3 消息 (账户状态)
fn parse_web_data3(data: &serde_json::Value, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // 解析 clearinghouseState
    if let Some(ch_state) = data.get("clearinghouseState") {
        match serde_json::from_value::<ClearinghouseState>(ch_state.clone()) {
            Ok(state) => {
                // 解析仓位
                for wrapper in &state.asset_positions {
                    let position = wrapper.position.to_position();
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    });
                }

                // 解析账户净值 (equity = accountValue)
                let equity = f64::from_str(&state.cross_margin_summary.account_value)
                    .expect("accountValue must be valid float from Hyperliquid API");
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::Equity {
                        exchange: Exchange::Hyperliquid,
                        equity,
                    },
                });

                // 解析可用余额
                let withdrawable = f64::from_str(&state.withdrawable)
                    .expect("withdrawable must be valid float from Hyperliquid API");
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::Balance(Balance {
                        exchange: Exchange::Hyperliquid,
                        asset: "USDC".to_string(),
                        available: withdrawable,
                        frozen: 0.0, // Hyperliquid 不直接提供 frozen，通过 marginUsed 计算
                    }),
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, data = %ch_state, "Failed to parse clearinghouseState");
            }
        }
    }

    Ok(events)
}

/// 解析 orderUpdates 消息
fn parse_order_updates(
    data: &serde_json::Value,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // orderUpdates 是一个数组
    if let Some(updates) = data.as_array() {
        for update in updates {
            match serde_json::from_value::<WsOrderUpdate>(update.clone()) {
                Ok(order_update) => {
                    let update = order_update.to_order_update();
                    events.push(IncomeEvent {
                        exchange_ts: update.timestamp,
                        local_ts,
                        data: ExchangeEventData::OrderUpdate(update),
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, data = %update, "Failed to parse order update");
                }
            }
        }
    }

    Ok(events)
}
