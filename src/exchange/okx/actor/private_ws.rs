//! OkxPrivateWsActor - 管理 OKX 私有 WebSocket 连接
//!
//! 职责:
//! - 建立私有 WebSocket 连接并完成登录
//! - 自动订阅私有频道 (positions, account, orders)
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{now_ms, Balance, Exchange, ExchangeError, Position, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::WsError;
use crate::exchange::okx::codec::{AccountData, OrderPushData, PositionData, WsEvent, WsPush};
use crate::exchange::okx::{OkxCredentials, WS_PRIVATE_URL};
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 登录响应等待超时 (秒)。超时 → 启动失败受控退出，避免 on_start 永久阻塞。
const LOGIN_TIMEOUT_SECS: u64 = 10;

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs {
    /// 凭证
    pub credentials: OkxCredentials,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（用于仓位转换）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// OkxPrivateWsActor - 私有 WebSocket Actor
pub struct OkxPrivateWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
}

impl OkxPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        tracing::debug!(raw, "OKX private message received");
        let local_ts = now_ms();
        let events = parse_private_message(raw, local_ts, &self.symbol_metas)?;
        tracing::debug!(count = events.len(), "OKX private events parsed");
        for event in events {
            if let Err(e) = self.income_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to IncomePubSub");
            }
        }
        Ok(())
    }
}

impl Actor for OkxPrivateWsActor {
    type Args = OkxPrivateWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 连接私有 WebSocket（失败向上传播 → 启动期受控退出）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 2. 发送 login 消息
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let sign = args.credentials.sign_ws_login(&timestamp);

        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": args.credentials.api_key,
                "passphrase": args.credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string();

        write
            .send(WsMessage::Text(login_msg))
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("OKX send login: {e}")))?;

        // 3. 等待 login 响应（带超时；任何失败向上传播 → 启动期受控退出，不重试）
        let login_wait = async {
            loop {
                match read.next().await {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                            if event.event == "login" {
                                if event.code.as_deref() == Some("0") {
                                    tracing::info!("OKX private login success");
                                    return Ok(());
                                } else {
                                    return Err(ExchangeError::AuthenticationFailed(Exchange::OKX));
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        write.send(WsMessage::Pong(data)).await.map_err(|e| {
                            ExchangeError::WebSocketError(format!("OKX pong during login: {e}"))
                        })?;
                    }
                    Some(Err(e)) => {
                        return Err(ExchangeError::WebSocketError(format!(
                            "OKX WS error during login: {e}"
                        )));
                    }
                    None => {
                        return Err(ExchangeError::ConnectionFailed(
                            Exchange::OKX,
                            "WS closed during login".to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(LOGIN_TIMEOUT_SECS), login_wait)
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!(
                    "OKX private login timed out after {LOGIN_TIMEOUT_SECS}s"
                ))
            })??;

        // 4. 订阅私有频道
        let subscribe_msg = json!({
            "op": "subscribe",
            "args": [
                {"channel": "positions", "instType": "SWAP"},
                {"channel": "account"},
                {"channel": "orders", "instType": "SWAP"}
            ]
        })
        .to_string();

        write
            .send(WsMessage::Text(subscribe_msg))
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("OKX send private subscribe: {e}")))?;

        // 5. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 6. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 7. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        tracing::info!("OkxPrivateWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            ws_tx: Some(outgoing_tx),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("OkxPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for OkxPrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Result<String, WsError>, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        crate::dispatch_ws_stream_message!(self, msg, ctx, "OKX");
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_private_message(
    raw: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是事件响应（控制消息，返回空 Vec）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => {
                return Ok(Vec::new());
            }
            "channel-conn-count" => {
                // OKX 连接计数事件，忽略
                return Ok(Vec::new());
            }
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => {
                tracing::warn!(event, raw, "OKX unknown event");
                return Ok(Vec::new());
            }
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing channel: {}", raw)))?;

    match channel {
        "positions" => {
            let push: WsPush<PositionData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("positions parse: {}", e)))?;

            let mut events = Vec::new();
            let mut seen_symbols = std::collections::HashSet::new();

            for data in &push.data {
                let mut position = data.to_position()?;
                if let Some(meta) = symbol_metas.get(&position.symbol) {
                    position.size = meta.qty_to_coin(position.size);
                    seen_symbols.insert(position.symbol.clone());
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    });
                }
            }

            // 为推送中缺失的配置 symbol 补推 0-position，
            // 确保策略端能区分 "确认空仓" 和 "未初始化"
            for symbol in symbol_metas.keys() {
                if !seen_symbols.contains(symbol) {
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(Position {
                            exchange: Exchange::OKX,
                            symbol: symbol.clone(),
                            size: 0.0,
                            entry_price: 0.0,
                            unrealized_pnl: 0.0,
                        }),
                    });
                }
            }

            Ok(events)
        }
        "account" => {
            let push: WsPush<AccountData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("account parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let exchange_ts = data
                    .u_time
                    .parse::<u64>()
                    .map_err(|_| WsError::ParseError(format!("Failed to parse OKX account timestamp: {}", data.u_time)))?;
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::AccountInfo {
                        exchange: Exchange::OKX,
                        equity: data.to_equity()?,
                        notional: data.to_notional()?,
                    },
                });

                // 推送每个币种的现金余额 (用于修正 greeks delta)
                for detail in &data.details {
                    let cash_bal: f64 = detail.cash_bal.parse()
                        .map_err(|_| WsError::ParseError(format!(
                            "Failed to parse cash_bal '{}' for ccy {}", detail.cash_bal, detail.ccy)))?;
                    let frozen: f64 = detail.frozen_bal.parse()
                        .map_err(|_| WsError::ParseError(format!(
                            "Failed to parse frozen_bal '{}' for ccy {}", detail.frozen_bal, detail.ccy)))?;
                    events.push(IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::Balance(Balance {
                            exchange: Exchange::OKX,
                            asset: detail.ccy.clone(),
                            available: cash_bal,
                            frozen,
                        }),
                    });
                }
            }
            Ok(events)
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("orders parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let mut order_update = data.to_order_update()
                    ?;

                // 获取 meta 转换数量单位（张 -> 币）
                if let Some(meta) = symbol_metas.get(&order_update.symbol) {
                    order_update.quantity = meta.qty_to_coin(order_update.quantity);
                    order_update.filled_quantity = meta.qty_to_coin(order_update.filled_quantity);
                    order_update.fill_sz = meta.qty_to_coin(order_update.fill_sz);
                }

                // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
                if let Some(mut fill) = data.to_fill()
                    ? {
                    // 获取 meta 转换数量单位（张 -> 币）
                    if let Some(meta) = symbol_metas.get(&fill.symbol) {
                        fill.size = meta.qty_to_coin(fill.size);
                    }
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Fill(fill),
                    });
                }

                // OrderUpdate 事件
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::OrderUpdate(order_update),
                });
            }
            Ok(events)
        }
        // account-greeks 已迁移到 REST 轮询 (OkxGreeksPollingActor)
        "account-greeks" => Ok(Vec::new()),
        _ => {
            tracing::warn!(channel, raw, "Unknown OKX private channel");
            Ok(Vec::new())
        }
    }
}
