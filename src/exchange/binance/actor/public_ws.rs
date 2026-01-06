//! BinancePublicWsActor - 管理 Binance 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发送到 EventSink

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{BookTicker, MarkPriceUpdate, WsResponse};
use crate::exchange::binance::to_binance;
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

/// BinancePublicWsActor 初始化参数
pub struct BinancePublicWsActorArgs {
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据（公开 WS 目前不需要，但保持一致性）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// BinancePublicWsActor - 公开 WebSocket Actor
pub struct BinancePublicWsActor {
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
}

impl BinancePublicWsActor {
    /// 创建新的 BinancePublicWsActor
    pub fn new(args: BinancePublicWsActorArgs) -> Self {
        Self {
            event_sink: args.event_sink,
            symbol_metas: args.symbol_metas,
            ws_tx: None,
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
        }
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "SUBSCRIBE",
            "params": [stream],
            "id": 1
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let stream = kind_to_stream(kind);
        let msg = json!({
            "method": "UNSUBSCRIBE",
            "params": [stream],
            "id": 2
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 解析并处理消息，返回是否成功
    async fn handle_message(&self, raw: &str) -> bool {
        let local_ts = now_ms();
        match parse_public_message(raw, local_ts, &self.subscribed_kinds) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
                true
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %raw, "Failed to parse Binance public message");
                false
            }
        }
    }
}

impl Actor for BinancePublicWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinancePublicWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| WsError::ConnectionFailed(e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        self.ws_tx = Some(outgoing_tx);

        // 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        tracing::info!("BinancePublicWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        tracing::info!("BinancePublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for BinancePublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查是否已订阅该 kind
        if self.subscribed_kinds.contains(&msg.kind) {
            return;
        }

        // 检查底层 stream 是否已订阅
        let stream = kind_to_stream(&msg.kind);
        if !self.subscribed_streams.contains(&stream) {
            // 发送 WebSocket 订阅请求
            if let Err(e) = self.send_subscribe(&msg.kind).await {
                tracing::error!(error = %e, "Failed to send subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }
            self.subscribed_streams.insert(stream);
        }

        self.subscribed_kinds.insert(msg.kind);
    }
}

impl Message<Unsubscribe> for BinancePublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.subscribed_kinds.remove(&msg.kind) {
            return;
        }

        // 检查是否还有其他 kinds 使用同一个 stream
        let stream = kind_to_stream(&msg.kind);
        let stream_still_needed = self
            .subscribed_kinds
            .iter()
            .any(|k| kind_to_stream(k) == stream);

        if !stream_still_needed {
            if let Err(e) = self.send_unsubscribe(&msg.kind).await {
                tracing::error!(error = %e, "Failed to send unsubscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }
            self.subscribed_streams.remove(&stream);
        }
    }
}

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for BinancePublicWsActor {
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
                tracing::error!(error = %e, "Public WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 异常退出，kill actor 触发级联退出
                tracing::error!("WebSocket stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_public_message(
    raw: &str,
    local_ts: u64,
    subscribed_kinds: &HashSet<SubscriptionKind>,
) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是订阅响应（控制消息，返回空 Vec）
    if value.get("id").is_some() {
        if let Ok(resp) = serde_json::from_str::<WsResponse>(raw) {
            if let Some(err) = resp.error {
                return Err(WsError::ParseError(format!(
                    "Subscribe error: code={}, msg={}",
                    err.code, err.msg
                )));
            }
        }
        return Ok(Vec::new());
    }

    // 提取交易所事件时间 (E 字段，毫秒)
    let exchange_ts = value
        .get("E")
        .and_then(|v| v.as_u64())
        .unwrap_or(local_ts);

    // 根据事件类型解析
    let event_type = value
        .get("e")
        .and_then(|v| v.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing event type: {}", raw)))?;

    match event_type {
        "markPriceUpdate" => {
            let update: MarkPriceUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("markPriceUpdate parse: {}", e)))?;
            let symbol = update.symbol();

            let mut events = Vec::new();

            // 根据订阅的 kinds 生成对应的事件
            if subscribed_kinds.contains(&SubscriptionKind::FundingRate { symbol: symbol.clone() })
            {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::FundingRate(update.to_funding_rate(exchange_ts)),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::MarkPrice { symbol: symbol.clone() }) {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::MarkPrice(update.to_mark_price(exchange_ts)),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::IndexPrice { symbol }) {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::IndexPrice(update.to_index_price(exchange_ts)),
                });
            }

            Ok(events)
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bookTicker parse: {}", e)))?;
            let bbo = ticker.to_bbo();
            Ok(vec![IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::BBO(bbo),
            }])
        }
        _ => {
            // 未知事件类型，记录警告但不报错
            tracing::warn!(event_type, raw, "Unknown Binance public event type");
            Ok(Vec::new())
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_stream(kind: &SubscriptionKind) -> String {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 markPrice@1s 流
        SubscriptionKind::FundingRate { symbol }
        | SubscriptionKind::MarkPrice { symbol }
        | SubscriptionKind::IndexPrice { symbol } => {
            format!("{}@markPrice@1s", to_binance(symbol).to_lowercase())
        }
        SubscriptionKind::BBO { symbol } => {
            format!("{}@bookTicker", to_binance(symbol).to_lowercase())
        }
    }
}
