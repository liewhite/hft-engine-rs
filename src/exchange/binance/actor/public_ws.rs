//! BinancePublicWsActor - 管理 Binance 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{now_ms, Symbol, SymbolMeta, Timestamp};
use crate::engine::IncomePubSub;
use crate::exchange::binance::codec::{BookTicker, MarkPriceUpdate, WsResponse};
use crate::exchange::binance::to_binance;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
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
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Binance WebSocket 订阅速率限制：每秒最多 10 条消息
const SUBSCRIBE_INTERVAL_MS: u64 = 110; // 略大于 100ms 以确保安全

/// Public WebSocket URL
const WS_PUBLIC_URL: &str = "wss://fstream.binance.com/ws";

/// BinancePublicWsActor 初始化参数
pub struct BinancePublicWsActorArgs {
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（公开 WS 目前不需要，但保持一致性）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// BinancePublicWsActor - 公开 WebSocket Actor
pub struct BinancePublicWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（用于过滤不存在的 symbol）
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
    /// 上次发送订阅消息的时间戳 (用于速率限制)
    last_subscribe_time: Timestamp,
}

impl BinancePublicWsActor {
    /// 发送订阅消息 (带速率限制)
    async fn send_subscribe(&mut self, kind: &SubscriptionKind) -> Result<(), WsError> {
        // 速率限制：确保距离上次订阅至少 SUBSCRIBE_INTERVAL_MS 毫秒
        let now = now_ms();
        let elapsed = now.saturating_sub(self.last_subscribe_time);
        if elapsed < SUBSCRIBE_INTERVAL_MS {
            let delay = SUBSCRIBE_INTERVAL_MS - elapsed;
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.last_subscribe_time = now_ms();

        let stream = kind_to_stream(kind, &self.quote);
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

    /// 批量发送订阅消息（一条 WebSocket 消息包含多个 streams）
    async fn send_subscribe_batch(&self, streams: Vec<String>) -> Result<(), WsError> {
        if streams.is_empty() {
            return Ok(());
        }

        let msg = json!({
            "method": "SUBSCRIBE",
            "params": streams,
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
        let stream = kind_to_stream(kind, &self.quote);
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

    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_public_message(raw, &self.quote, local_ts, &self.subscribed_kinds)?;
        for event in events {
            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }
        Ok(())
    }
}

impl Actor for BinancePublicWsActor {
    type Args = BinancePublicWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .expect("Failed to connect to Binance public WebSocket");

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

        tracing::info!("BinancePublicWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
            last_subscribe_time: 0,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
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
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 检查 symbol 是否存在于 symbol_metas 中
        let symbol = msg.kind.symbol();
        if !self.symbol_metas.contains_key(symbol) {
            tracing::warn!(
                exchange = "Binance",
                symbol = %symbol,
                "Symbol not found in symbol_metas, ignoring subscription"
            );
            return;
        }

        // 检查是否已订阅该 kind
        if self.subscribed_kinds.contains(&msg.kind) {
            return;
        }

        // 检查底层 stream 是否已订阅
        let stream = kind_to_stream(&msg.kind, &self.quote);
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

impl Message<SubscribeBatch> for BinancePublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. 过滤有效的 kinds（symbol 存在且未订阅）
        let mut new_streams = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in msg.kinds {
            let symbol = kind.symbol();
            if !self.symbol_metas.contains_key(symbol) {
                tracing::warn!(
                    exchange = "Binance",
                    symbol = %symbol,
                    "Symbol not found in symbol_metas, ignoring subscription"
                );
                continue;
            }

            if self.subscribed_kinds.contains(&kind) {
                continue;
            }

            let stream = kind_to_stream(&kind, &self.quote);
            if !self.subscribed_streams.contains(&stream) {
                new_streams.push(stream);
            }
            new_kinds.push(kind);
        }

        // 2. 批量发送订阅请求
        if !new_streams.is_empty() {
            // 去重
            let unique_streams: Vec<_> = new_streams
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            tracing::info!(
                exchange = "Binance",
                count = unique_streams.len(),
                "Batch subscribing to streams"
            );

            if let Err(e) = self.send_subscribe_batch(unique_streams.clone()).await {
                tracing::error!(error = %e, "Failed to send batch subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }

            for stream in unique_streams {
                self.subscribed_streams.insert(stream);
            }
        }

        // 3. 记录已订阅的 kinds
        for kind in new_kinds {
            self.subscribed_kinds.insert(kind);
        }
    }
}

impl Message<Unsubscribe> for BinancePublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.subscribed_kinds.remove(&msg.kind) {
            return;
        }

        // 检查是否还有其他 kinds 使用同一个 stream
        let stream = kind_to_stream(&msg.kind, &self.quote);
        let stream_still_needed = self
            .subscribed_kinds
            .iter()
            .any(|k| kind_to_stream(k, &self.quote) == stream);

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
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                // 解析并发布到 IncomePubSub，失败则 kill actor
                if let Err(e) = self.handle_message(&data).await {
                    tracing::error!(exchange = "Binance", error = %e, raw = %data, "Public WS parse error, killing actor");
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
    quote: &str,
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
            let symbol = update.symbol(quote);

            let mut events = Vec::new();

            // 根据订阅的 kinds 生成对应的事件
            if subscribed_kinds.contains(&SubscriptionKind::FundingRate { symbol: symbol.clone() })
            {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::FundingRate(update.to_funding_rate(quote, exchange_ts)),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::MarkPrice { symbol: symbol.clone() }) {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::MarkPrice(update.to_mark_price(quote, exchange_ts)),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::IndexPrice { symbol }) {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::IndexPrice(update.to_index_price(quote, exchange_ts)),
                });
            }

            Ok(events)
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bookTicker parse: {}", e)))?;
            let bbo = ticker.to_bbo(quote);
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

fn kind_to_stream(kind: &SubscriptionKind, quote: &str) -> String {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 markPrice@1s 流
        SubscriptionKind::FundingRate { symbol }
        | SubscriptionKind::MarkPrice { symbol }
        | SubscriptionKind::IndexPrice { symbol } => {
            format!("{}@markPrice@1s", to_binance(symbol, quote).to_lowercase())
        }
        SubscriptionKind::BBO { symbol } => {
            format!("{}@bookTicker", to_binance(symbol, quote).to_lowercase())
        }
    }
}
