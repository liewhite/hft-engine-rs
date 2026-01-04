//! HyperliquidPublicWsActor - 管理 Hyperliquid WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发送到 EventSink

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::client::{EventSink, Subscribe, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::hyperliquid::codec::{WsActiveAssetCtx, WsBbo};
use crate::exchange::hyperliquid::{to_hyperliquid, WS_URL};
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

/// HyperliquidPublicWsActor 初始化参数
pub struct HyperliquidPublicWsActorArgs {
    /// 事件接收器
    pub event_sink: Arc<dyn EventSink>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// HyperliquidPublicWsActor - WebSocket Actor
pub struct HyperliquidPublicWsActor {
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

impl HyperliquidPublicWsActor {
    /// 创建新的 HyperliquidPublicWsActor
    pub fn new(args: HyperliquidPublicWsActorArgs) -> Self {
        Self {
            event_sink: args.event_sink,
            symbol_metas: args.symbol_metas,
            ws_tx: None,
            subscribed: HashSet::new(),
        }
    }

    /// 发送订阅消息
    async fn send_subscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = kind_to_subscription(kind);
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
        let subscription = kind_to_subscription(kind);
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
    async fn handle_message(&self, raw: &str) {
        let local_ts = now_ms();
        match parse_public_message(raw, local_ts) {
            Ok(events) => {
                for event in events {
                    self.event_sink.send_event(event).await;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, raw = %raw, "Failed to parse Hyperliquid public message");
            }
        }
    }
}

impl Actor for HyperliquidPublicWsActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "HyperliquidPublicWsActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 连接 WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
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

        tracing::info!("HyperliquidPublicWsActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
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

impl Message<Unsubscribe> for HyperliquidPublicWsActor {
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
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPublicWsActor {
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
                tracing::error!(error = %e, "WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 正常退出
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

    // 检查是否是订阅确认
    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            "subscriptionResponse" => {
                // 订阅响应，忽略
                return Ok(Vec::new());
            }
            "activeAssetCtx" => {
                // 资产上下文（包含资金费率）
                let data = &value["data"];
                match serde_json::from_value::<WsActiveAssetCtx>(data.clone()) {
                    Ok(ctx) => {
                        let mut events = Vec::new();

                        // 资金费率事件
                        let rate = ctx.to_funding_rate();
                        events.push(IncomeEvent {
                            exchange_ts: local_ts,
                            local_ts,
                            data: ExchangeEventData::FundingRate(rate),
                        });
                        return Ok(events);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, data = %data, "Failed to parse activeAssetCtx");
                    }
                }
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                match serde_json::from_value::<WsBbo>(data.clone()) {
                    Ok(bbo_data) => {
                        let bbo = bbo_data.to_bbo();
                        return Ok(vec![IncomeEvent {
                            exchange_ts: bbo.timestamp,
                            local_ts,
                            data: ExchangeEventData::BBO(bbo),
                        }]);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, data = %data, "Failed to parse bbo");
                    }
                }
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

/// 将 SubscriptionKind 转换为 Hyperliquid 订阅格式
fn kind_to_subscription(kind: &SubscriptionKind) -> serde_json::Value {
    match kind {
        SubscriptionKind::FundingRate { symbol } => {
            // 使用 activeAssetCtx 获取资金费率
            json!({
                "type": "activeAssetCtx",
                "coin": to_hyperliquid(symbol)
            })
        }
        SubscriptionKind::BBO { symbol } => {
            // 使用 bbo 获取最优买卖价
            json!({
                "type": "bbo",
                "coin": to_hyperliquid(symbol)
            })
        }
    }
}
