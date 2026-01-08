//! HyperliquidPublicWsActor - 管理 Hyperliquid WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::hyperliquid::codec::{WsActiveAssetCtx, WsBbo};
use crate::exchange::hyperliquid::{to_hyperliquid, WS_URL};
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

/// HyperliquidPublicWsActor 初始化参数
pub struct HyperliquidPublicWsActorArgs {
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDC", "USDE")
    pub quote: String,
}

/// HyperliquidPublicWsActor - WebSocket Actor
pub struct HyperliquidPublicWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    #[allow(dead_code)]
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDC", "USDE")
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
}

impl HyperliquidPublicWsActor {

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = kind_to_subscription(kind, &self.quote);
        let msg = json!({
            "method": "subscribe",
            "subscription": subscription
        })
        .to_string();

        let tx = self.ws_tx.as_ref().expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = kind_to_subscription(kind, &self.quote);
        let msg = json!({
            "method": "unsubscribe",
            "subscription": subscription
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
        let events = parse_public_message(raw, local_ts, &self.subscribed_kinds)?;
        for event in events {
            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }
        Ok(())
    }
}

impl Actor for HyperliquidPublicWsActor {
    type Args = HyperliquidPublicWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .expect("Failed to connect to Hyperliquid WebSocket");

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

        tracing::info!("HyperliquidPublicWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        tracing::info!("HyperliquidPublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for HyperliquidPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
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

impl Message<Unsubscribe> for HyperliquidPublicWsActor {
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
        let quote = &self.quote;
        let stream_still_needed = self
            .subscribed_kinds
            .iter()
            .any(|k| kind_to_stream(k, quote) == stream);

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
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPublicWsActor {
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
                    tracing::error!(exchange = "Hyperliquid", error = %e, raw = %data, "Public WS parse error, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "WebSocket loop exited, killing actor");
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

    // 检查是否有错误
    if let Some(error) = value.get("error") {
        return Err(WsError::ParseError(format!(
            "Hyperliquid error: {}",
            error
        )));
    }

    // 检查是否是订阅确认
    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            "subscriptionResponse" => {
                // 订阅响应，检查是否有错误
                if let Some(data) = value.get("data") {
                    if let Some(err) = data.get("error") {
                        return Err(WsError::ParseError(format!(
                            "Hyperliquid subscription error: {}",
                            err
                        )));
                    }
                }
                return Ok(Vec::new());
            }
            "activeAssetCtx" => {
                // 资产上下文（包含资金费率、标记价格、指数价格）
                let data = &value["data"];
                let ctx: WsActiveAssetCtx = serde_json::from_value(data.clone())
                    .map_err(|e| WsError::ParseError(format!("activeAssetCtx parse: {}", e)))?;

                let symbol = ctx.symbol();
                let mut events = Vec::new();

                // 根据订阅的 kinds 生成对应的事件
                if subscribed_kinds
                    .contains(&SubscriptionKind::FundingRate { symbol: symbol.clone() })
                {
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::FundingRate(ctx.to_funding_rate(local_ts)),
                    });
                }
                if subscribed_kinds
                    .contains(&SubscriptionKind::MarkPrice { symbol: symbol.clone() })
                {
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::MarkPrice(ctx.to_mark_price(local_ts)),
                    });
                }
                if subscribed_kinds.contains(&SubscriptionKind::IndexPrice { symbol }) {
                    events.push(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::IndexPrice(ctx.to_index_price(local_ts)),
                    });
                }

                return Ok(events);
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                let bbo_data: WsBbo = serde_json::from_value(data.clone())
                    .map_err(|e| WsError::ParseError(format!("bbo parse: {}", e)))?;

                let bbo = bbo_data.to_bbo();
                return Ok(vec![IncomeEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: ExchangeEventData::BBO(bbo),
                }]);
            }
            "allMids" => {
                // 所有中间价，当前不处理
                return Ok(Vec::new());
            }
            _ => {
                tracing::debug!(channel, "Unknown Hyperliquid public channel");
                return Ok(Vec::new());
            }
        }
    }

    // pong 消息
    if value.get("method").map(|v| v.as_str()) == Some(Some("pong")) {
        return Ok(Vec::new());
    }

    // 其他未知消息
    tracing::debug!(raw, "Unhandled Hyperliquid public message");
    Ok(Vec::new())
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 将 SubscriptionKind 转换为底层 stream 标识符 (用于去重)
fn kind_to_stream(kind: &SubscriptionKind, quote: &str) -> String {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 activeAssetCtx 订阅
        SubscriptionKind::FundingRate { symbol }
        | SubscriptionKind::MarkPrice { symbol }
        | SubscriptionKind::IndexPrice { symbol } => {
            format!("activeAssetCtx:{}", to_hyperliquid(symbol, quote))
        }
        SubscriptionKind::BBO { symbol } => {
            format!("bbo:{}", to_hyperliquid(symbol, quote))
        }
    }
}

/// 将 SubscriptionKind 转换为 Hyperliquid 订阅格式
fn kind_to_subscription(kind: &SubscriptionKind, quote: &str) -> serde_json::Value {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用 activeAssetCtx 订阅
        SubscriptionKind::FundingRate { symbol }
        | SubscriptionKind::MarkPrice { symbol }
        | SubscriptionKind::IndexPrice { symbol } => {
            json!({
                "type": "activeAssetCtx",
                "coin": to_hyperliquid(symbol, quote)
            })
        }
        SubscriptionKind::BBO { symbol } => {
            // 使用 bbo 获取最优买卖价
            json!({
                "type": "bbo",
                "coin": to_hyperliquid(symbol, quote)
            })
        }
    }
}
