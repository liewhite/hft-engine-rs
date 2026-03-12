//! OkxPublicWsActor - 管理 OKX 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::okx::codec::{BboData, FundingRateData, IndexTickerData, MarkPriceData, WsPush};
use crate::exchange::okx::{to_okx, to_okx_index};
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";

/// OkxPublicWsActor 初始化参数
pub struct OkxPublicWsActorArgs {
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// OkxPublicWsActor - 公开 WebSocket Actor
pub struct OkxPublicWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（用于过滤不存在的 symbol）
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
}

impl OkxPublicWsActor {
    /// 批量发送订阅消息（一条 WebSocket 消息包含多个 args）
    async fn send_subscribe_batch(&self, args: Vec<serde_json::Value>) -> Result<(), WsError> {
        if args.is_empty() {
            return Ok(());
        }

        let msg = json!({
            "op": "subscribe",
            "args": args
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind, &self.quote);
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
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_public_message(raw, local_ts)?;
        for event in events {
            if let Err(e) = self.income_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to IncomePubSub");
            }
        }
        Ok(())
    }
}

impl Actor for OkxPublicWsActor {
    type Args = OkxPublicWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .expect("Failed to connect to OKX public WebSocket");

        let (write, read) = ws_stream.split();

        // 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        tracing::info!("OkxPublicWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            subscribed: HashSet::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
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
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 委托给批量订阅
        self.handle(SubscribeBatch { kinds: vec![msg.kind] }, ctx)
            .await
    }
}

impl Message<SubscribeBatch> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. 过滤有效的 kinds（symbol 存在且未订阅）
        let mut new_args = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in msg.kinds {
            let symbol = kind.symbol();
            if !self.symbol_metas.contains_key(symbol) {
                tracing::warn!(
                    exchange = "OKX",
                    symbol = %symbol,
                    "Symbol not found in symbol_metas, ignoring subscription"
                );
                continue;
            }

            if self.subscribed.contains(&kind) {
                continue;
            }

            new_args.push(kind_to_arg(&kind, &self.quote));
            new_kinds.push(kind);
        }

        // 2. 批量发送订阅请求
        if !new_args.is_empty() {
            tracing::info!(
                exchange = "OKX",
                count = new_args.len(),
                "Batch subscribing to channels"
            );

            if let Err(e) = self.send_subscribe_batch(new_args).await {
                tracing::error!(error = %e, "Failed to send batch subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }
        }

        // 3. 记录已订阅的 kinds
        for kind in new_kinds {
            self.subscribed.insert(kind);
        }
    }
}

impl Message<Unsubscribe> for OkxPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: &mut Context<Self, Self::Reply>,
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
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 解析并发布到 IncomePubSub，失败则 kill actor
                if let Err(e) = self.handle_message(&data).await {
                    tracing::error!(exchange = "OKX", error = %e, raw = %data, "Public WS parse error, killing actor");
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

            let mut events = Vec::new();
            for data in &push.data {
                let rate = data.to_funding_rate(local_ts)
                    .map_err(|e| WsError::ParseError(e))?;
                events.push(IncomeEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: ExchangeEventData::FundingRate(rate),
                });
            }
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

            let mut events = Vec::new();
            for data in &push.data {
                let bbo = data.to_bbo(inst_id)
                    .map_err(|e| WsError::ParseError(e))?;
                events.push(IncomeEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: ExchangeEventData::BBO(bbo),
                });
            }
            Ok(events)
        }
        "mark-price" => {
            let push: WsPush<MarkPriceData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("mark-price parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let mp = data.to_mark_price()
                    .map_err(|e| WsError::ParseError(e))?;
                events.push(IncomeEvent {
                    exchange_ts: mp.timestamp,
                    local_ts,
                    data: ExchangeEventData::MarkPrice(mp),
                });
            }
            Ok(events)
        }
        "index-tickers" => {
            let push: WsPush<IndexTickerData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("index-tickers parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let ip = data.to_index_price()
                    .map_err(|e| WsError::ParseError(e))?;
                events.push(IncomeEvent {
                    exchange_ts: ip.timestamp,
                    local_ts,
                    data: ExchangeEventData::IndexPrice(ip),
                });
            }
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

fn kind_to_arg(kind: &SubscriptionKind, quote: &str) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            json!({
                "channel": "funding-rate",
                "instId": to_okx(symbol, quote)
            })
        }
        SubscriptionKind::BBO { symbol } => {
            json!({
                "channel": "bbo-tbt",
                "instId": to_okx(symbol, quote)
            })
        }
        SubscriptionKind::MarkPrice { symbol } => {
            json!({
                "channel": "mark-price",
                "instId": to_okx(symbol, quote)
            })
        }
        SubscriptionKind::IndexPrice { symbol } => {
            // OKX index-tickers 使用指数 ID 格式 (如 BTC-USDT)
            json!({
                "channel": "index-tickers",
                "instId": to_okx_index(symbol, quote)
            })
        }
        SubscriptionKind::Candle { .. } => {
            panic!("Candle subscriptions should be routed to BusinessWsActor")
        }
    }
}
