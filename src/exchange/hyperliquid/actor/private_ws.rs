//! HyperliquidPrivateWsActor - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 自动订阅账户频道 (webData3, orderUpdates)
//! - 直接解析消息并发布到 IncomePubSub
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址

use crate::domain::{now_ms, Balance, Exchange, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::WsError;
use crate::exchange::hyperliquid::codec::{ClearinghouseState, WsOrderUpdate};
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
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
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// HyperliquidPrivateWsActor - 账户 WebSocket Actor
pub struct HyperliquidPrivateWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    #[allow(dead_code)]
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已知持仓的 symbols（用于检测仓位消失）
    known_positions: std::collections::HashSet<Symbol>,
}

impl HyperliquidPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&mut self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_private_message(raw, local_ts, &mut self.known_positions)?;
        for event in events {
            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }
        Ok(())
    }
}

impl Actor for HyperliquidPrivateWsActor {
    type Args = HyperliquidPrivateWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .expect("Failed to connect to Hyperliquid WebSocket");

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
                "user": args.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_clearinghouse)
            .await
            .expect("Failed to send clearinghouseState subscription");

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
            .expect("Failed to send orderUpdates subscription");

        tracing::info!(
            wallet = %args.wallet_address,
            "HyperliquidPrivateWsActor started, subscribed to clearinghouseState and orderUpdates"
        );

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            ws_tx: Some(outgoing_tx),
            known_positions: std::collections::HashSet::new(),
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
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 解析并发布到 IncomePubSub，失败则 kill actor
                if let Err(e) = self.handle_message(&data).await {
                    tracing::error!(exchange = "Hyperliquid", error = %e, raw = %data, "Private WS parse error, killing actor");
                    ctx.actor_ref().kill();
                }
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

fn parse_private_message(
    raw: &str,
    local_ts: u64,
    known_positions: &mut std::collections::HashSet<Symbol>,
) -> Result<Vec<IncomeEvent>, WsError> {
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
                // perp 账户状态 (positions, equity, margin)
                let data = &value["data"];
                return parse_clearinghouse_state(data, local_ts, known_positions);
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

/// 解析 clearinghouseState 消息 (perp 账户状态)
fn parse_clearinghouse_state(
    data: &serde_json::Value,
    local_ts: u64,
    known_positions: &mut std::collections::HashSet<Symbol>,
) -> Result<Vec<IncomeEvent>, WsError> {
    use crate::domain::Position;
    use std::collections::HashSet;

    let mut events = Vec::new();

    // clearinghouseState 订阅返回结构: { clearinghouseState: {...}, user: "...", dex: "..." }
    // 需要提取 clearinghouseState 字段
    let state_value = data.get("clearinghouseState").unwrap_or(data);
    let state: ClearinghouseState = serde_json::from_value(state_value.clone())
        .map_err(|e| WsError::ParseError(format!("clearinghouseState parse: {}", e)))?;

    // 收集本次响应中包含的 symbols
    let mut current_symbols: HashSet<Symbol> = HashSet::new();

    // 解析仓位
    tracing::debug!(
        positions_count = state.asset_positions.len(),
        coins = ?state.asset_positions.iter().map(|w| &w.position.coin).collect::<Vec<_>>(),
        "Hyperliquid clearinghouseState received"
    );
    for wrapper in &state.asset_positions {
        let position = wrapper.position.to_position();
        // 只处理非零仓位
        if position.size.abs() > 1e-10 {
            tracing::debug!(
                symbol = %position.symbol,
                size = position.size,
                "Hyperliquid position update"
            );
            current_symbols.insert(position.symbol.clone());
            events.push(IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::Position(position),
            });
        }
    }

    // 对于之前有仓位但本次响应中消失的 symbols，发送 size=0 的仓位更新
    for symbol in known_positions.difference(&current_symbols) {
        tracing::info!(
            symbol = %symbol,
            "Hyperliquid position disappeared, setting to zero"
        );
        events.push(IncomeEvent {
            exchange_ts: local_ts,
            local_ts,
            data: ExchangeEventData::Position(Position {
                exchange: Exchange::Hyperliquid,
                symbol: symbol.clone(),
                size: 0.0,
                entry_price: 0.0,
                mark_price: 0.0,
                unrealized_pnl: 0.0,
                leverage: 1,
            }),
        });
    }

    // 更新已知仓位集合
    *known_positions = current_symbols;

    // 解析账户净值 (equity = marginSummary.accountValue，包含全仓+逐仓)
    let equity = f64::from_str(&state.margin_summary.account_value)
        .map_err(|_| WsError::ParseError(format!("invalid accountValue: {}", state.margin_summary.account_value)))?;
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

/// 解析 orderUpdates 消息
fn parse_order_updates(
    data: &serde_json::Value,
    local_ts: u64,
) -> Result<Vec<IncomeEvent>, WsError> {
    let mut events = Vec::new();

    // orderUpdates 必须是一个数组
    let updates = data.as_array()
        .ok_or_else(|| WsError::ParseError(format!("orderUpdates is not an array: {}", data)))?;

    for update in updates {
        let order_update: WsOrderUpdate = serde_json::from_value(update.clone())
            .map_err(|e| WsError::ParseError(format!("orderUpdate parse: {}", e)))?;
        let update = order_update.to_order_update();
        events.push(IncomeEvent {
            exchange_ts: update.timestamp,
            local_ts,
            data: ExchangeEventData::OrderUpdate(update),
        });
    }

    Ok(events)
}
