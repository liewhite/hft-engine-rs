//! OkxPrivateWsActor - 管理 OKX 私有 WebSocket 连接
//!
//! 职责:
//! - 建立私有 WebSocket 连接并完成登录
//! - 自动订阅私有频道 (positions, account, orders)
//! - 直接解析消息并发送到 EventSink

use crate::domain::{now_ms, Exchange, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, WsError};
use crate::exchange::okx::codec::{AccountData, OrderPushData, PositionData, WsEvent, WsPush};
use crate::exchange::okx::OkxCredentials;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Private WebSocket URL
const WS_PRIVATE_URL: &str = "wss://ws.okx.com:8443/ws/v5/private";

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs {
    /// 凭证
    pub credentials: OkxCredentials,
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据（用于仓位转换）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// OkxPrivateWsActor - 私有 WebSocket Actor
pub struct OkxPrivateWsActor {
    /// 凭证
    credentials: OkxCredentials,
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
}

impl OkxPrivateWsActor {
    /// 创建新的 OkxPrivateWsActor
    pub fn new(args: OkxPrivateWsActorArgs) -> Self {
        Self {
            credentials: args.credentials,
            event_sink: args.event_sink,
            symbol_metas: args.symbol_metas,
            ws_tx: None,
        }
    }

    /// 解析并处理消息，返回是否成功
    async fn handle_message(&self, raw: &str) -> bool {
        let local_ts = now_ms();
        match parse_private_message(raw, local_ts, &self.symbol_metas) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
                true
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %raw, "Failed to parse OKX private message");
                false
            }
        }
    }
}

impl Actor for OkxPrivateWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxPrivateWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 1. 连接私有 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 2. 发送 login 消息
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let sign = self.credentials.sign_ws_login(&timestamp);

        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": self.credentials.api_key,
                "passphrase": self.credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string();

        write
            .send(WsMessage::Text(login_msg))
            .await
            .map_err(|e| WsError::AuthFailed(e.to_string()))?;

        // 3. 等待 login 响应
        loop {
            match read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                        if event.event == "login" {
                            if event.code.as_deref() == Some("0") {
                                tracing::info!("OKX private login success");
                                break;
                            } else {
                                return Err(WsError::AuthFailed(format!(
                                    "Login failed: {:?}",
                                    event.msg
                                ))
                                .into());
                            }
                        }
                    }
                }
                Some(Ok(WsMessage::Ping(data))) => {
                    write
                        .send(WsMessage::Pong(data))
                        .await
                        .map_err(|e| WsError::Network(format!("Failed to send pong: {}", e)))?;
                }
                Some(Err(e)) => {
                    return Err(WsError::Network(e.to_string()).into());
                }
                None => {
                    return Err(WsError::ServerClosed.into());
                }
                _ => {}
            }
        }

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
            .map_err(|e| WsError::Network(e.to_string()))?;

        // 5. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx);

        // 6. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 7. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        tracing::info!("OkxPrivateWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
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
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 解析并发送到 EventSink，失败则 kill actor
                if !self.handle_message(&data).await {
                    tracing::error!("Critical parse error, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
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
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是事件响应（控制消息，返回空 Vec）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return Ok(Vec::new()),
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => return Ok(Vec::new()),
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

            let events = push
                .data
                .iter()
                .map(|data| {
                    let mut position = data.to_position();
                    let meta = symbol_metas
                        .get(&position.symbol)
                        .expect("SymbolMeta not found for position symbol");
                    position.size = meta.qty_to_coin(position.size);
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Position(position),
                    }
                })
                .collect();
            Ok(events)
        }
        "account" => {
            let push: WsPush<AccountData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("account parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let exchange_ts = data.u_time.parse::<u64>().unwrap_or_else(|_| {
                        panic!("Failed to parse account timestamp: {}", data.u_time)
                    });
                    let equity = data.to_equity();
                    IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::Equity {
                            exchange: Exchange::OKX,
                            equity,
                        },
                    }
                })
                .collect();
            Ok(events)
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("orders parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let update = data.to_order_update();
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::OrderUpdate(update),
                    }
                })
                .collect();
            Ok(events)
        }
        _ => {
            tracing::warn!(channel, raw, "Unknown OKX private channel");
            Ok(Vec::new())
        }
    }
}
