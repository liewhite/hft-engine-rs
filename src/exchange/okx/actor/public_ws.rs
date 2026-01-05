//! OkxPublicWsActor - 管理 OKX 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发送到 EventSink

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::okx::codec::{BboData, FundingRateData, IndexTickerData, MarkPriceData, WsPush};
use crate::exchange::okx::{to_okx, to_okx_index};
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
const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";

/// OkxPublicWsActor 初始化参数
pub struct OkxPublicWsActorArgs {
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// OkxPublicWsActor - 公开 WebSocket Actor
pub struct OkxPublicWsActor {
    /// 事件接收器
    event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
}

impl OkxPublicWsActor {
    /// 创建新的 OkxPublicWsActor
    pub fn new(args: OkxPublicWsActorArgs) -> Self {
        Self {
            event_sink: args.event_sink,
            symbol_metas: args.symbol_metas,
            ws_tx: None,
            subscribed: HashSet::new(),
        }
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "subscribe",
            "args": [arg]
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind);
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) {
        let local_ts = now_ms();
        match parse_public_message(raw, local_ts) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %raw, "Failed to parse OKX public message");
            }
        }
    }
}

impl Actor for OkxPublicWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "OkxPublicWsActor"
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

        tracing::info!("OkxPublicWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        self.ws_tx.take();
        tracing::info!("OkxPublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if self.subscribed.contains(&msg.kind) {
            return;
        }

        if let Err(e) = self.send_subscribe(&msg.kind).await {
            tracing::error!(error = %e, "Failed to send subscribe, killing actor");
            ctx.actor_ref().kill();
            return;
        }
        self.subscribed.insert(msg.kind);
    }
}

impl Message<Unsubscribe> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.subscribed.remove(&msg.kind) {
            return;
        }

        if let Err(e) = self.send_unsubscribe(&msg.kind).await {
            tracing::error!(error = %e, "Failed to send unsubscribe, killing actor");
            ctx.actor_ref().kill();
        }
    }
}

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for OkxPublicWsActor {
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
                tracing::error!(error = %e, "Public WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 正常退出（outgoing_tx 被 drop）
                tracing::debug!("WsIncoming stream finished");
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_public_message(raw: &str, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
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
        "funding-rate" => {
            let push: WsPush<FundingRateData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("funding-rate parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let rate = data.to_funding_rate(local_ts);
                    IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::FundingRate(rate),
                    }
                })
                .collect();
            Ok(events)
        }
        "bbo-tbt" => {
            let push: WsPush<BboData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bbo-tbt parse: {}", e)))?;
            let inst_id = push
                .arg
                .inst_id
                .as_ref()
                .ok_or_else(|| WsError::ParseError("Missing instId in bbo-tbt".into()))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let exchange_ts = data
                        .ts
                        .parse::<u64>()
                        .unwrap_or_else(|_| panic!("Failed to parse BBO timestamp: {}", data.ts));
                    let bbo = data.to_bbo(inst_id);
                    IncomeEvent {
                        exchange_ts,
                        local_ts,
                        data: ExchangeEventData::BBO(bbo),
                    }
                })
                .collect();
            Ok(events)
        }
        "mark-price" => {
            let push: WsPush<MarkPriceData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("mark-price parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let mp = data.to_mark_price();
                    IncomeEvent {
                        exchange_ts: mp.timestamp,
                        local_ts,
                        data: ExchangeEventData::MarkPrice(mp),
                    }
                })
                .collect();
            Ok(events)
        }
        "index-tickers" => {
            let push: WsPush<IndexTickerData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("index-tickers parse: {}", e)))?;

            let events = push
                .data
                .iter()
                .map(|data| {
                    let ip = data.to_index_price();
                    IncomeEvent {
                        exchange_ts: ip.timestamp,
                        local_ts,
                        data: ExchangeEventData::IndexPrice(ip),
                    }
                })
                .collect();
            Ok(events)
        }
        _ => {
            tracing::warn!(channel, raw, "Unknown OKX public channel");
            Ok(Vec::new())
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_arg(kind: &SubscriptionKind) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            json!({
                "channel": "funding-rate",
                "instId": to_okx(symbol)
            })
        }
        SubscriptionKind::BBO { symbol } => {
            json!({
                "channel": "bbo-tbt",
                "instId": to_okx(symbol)
            })
        }
        SubscriptionKind::MarkPrice { symbol } => {
            json!({
                "channel": "mark-price",
                "instId": to_okx(symbol)
            })
        }
        SubscriptionKind::IndexPrice { symbol } => {
            // OKX index-tickers 使用指数 ID 格式 (如 BTC-USDT)
            json!({
                "channel": "index-tickers",
                "instId": to_okx_index(symbol)
            })
        }
    }
}
