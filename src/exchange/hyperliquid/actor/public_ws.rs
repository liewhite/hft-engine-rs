//! HyperliquidPublicWsActor - 管理 Hyperliquid WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发布到对应总线

use crate::domain::{now_ms, Exchange, ExchangeError, Symbol};
use crate::messaging::MarketPubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::hyperliquid::codec::{aggregate_trades, WsActiveAssetCtx, WsBbo, WsTrade};
use crate::exchange::hyperliquid::markets::HlMarkets;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use crate::messaging::{MarketData, MarketEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
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
    /// 行情总线（发布事件）
    pub market_pubsub: ActorRef<MarketPubSub>,
    /// 已接入 dex 的品种目录（symbol → 带 dex 前缀的 coin）
    pub markets: Arc<HlMarkets>,
}

/// HyperliquidPublicWsActor - WebSocket Actor
pub struct HyperliquidPublicWsActor {
    /// 行情总线（发布事件）
    market_pubsub: ActorRef<MarketPubSub>,
    /// 品种目录：订阅前查 coin，查不到的 symbol 不订阅
    markets: Arc<HlMarkets>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
    /// 各 symbol 的 trades 订阅时刻 (毫秒)，用于丢弃订阅前的历史回填
    ///
    /// Hyperliquid 在 trades 订阅成功后会先推一批**订阅前**的历史成交 (实测首条消息含
    /// 30 笔、跨 6.5 秒、最早一笔早于 WS 握手)。这些事件的 `exchange_ts` 落后数秒，若原样
    /// 下发，策略在启动瞬间会看到一段并不存在于"当下"的成交脉冲，成交流因子被污染。
    /// 这是 Hyperliquid 独有行为 (Binance/OKX 均只推订阅后的增量)，故在本适配层吸收，
    /// 不外泄给策略。
    trades_subscribed_at: HashMap<Symbol, u64>,
}

impl HyperliquidPublicWsActor {
    /// 批量发送订阅消息（发送多条消息，无速率限制）
    async fn send_subscribe_batch(&self, kinds: &[SubscriptionKind]) -> Result<(), WsError> {
        let tx = self
            .ws_tx
            .as_ref()
            .ok_or_else(|| WsError::Network("ws_tx unavailable (actor stopped)".to_string()))?;

        for kind in kinds {
            let subscription = self.subscription(kind)?;
            let msg = json!({
                "method": "subscribe",
                "subscription": subscription
            })
            .to_string();

            tx.send(msg)
                .await
                .map_err(|_| WsError::Network("Channel closed".to_string()))?;
        }

        Ok(())
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let subscription = self.subscription(kind)?;
        let msg = json!({
            "method": "unsubscribe",
            "subscription": subscription
        })
        .to_string();

        let tx = self
            .ws_tx
            .as_ref()
            .ok_or_else(|| WsError::Network("ws_tx unavailable (actor stopped)".to_string()))?;
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// kind 对应的订阅报文；只对已过订阅期筛选的 kind 调用（Candle 与目录外的 symbol
    /// 在 SubscribeBatch 里就被跳过），走到这里还查不到属路由 bug
    fn subscription(&self, kind: &SubscriptionKind) -> Result<serde_json::Value, WsError> {
        let channel = kind_channel(kind).ok_or_else(|| {
            WsError::ParseError(format!("{kind:?} reached HyperliquidPublicWsActor (routing bug)"))
        })?;
        let coin = self.markets.coin(kind.symbol()).ok_or_else(|| {
            WsError::ParseError(format!("{} 不在 HL 品种目录里 (routing bug)", kind.symbol()))
        })?;
        Ok(json!({"type": channel, "coin": coin}))
    }

    /// 底层 stream 标识（同一 stream 可被多个 kind 共用，用于去重）
    fn stream(&self, kind: &SubscriptionKind) -> Option<String> {
        Some(format!("{}:{}", kind_channel(kind)?, self.markets.coin(kind.symbol())?))
    }

    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_public_message(raw, local_ts, &self.subscribed_kinds)?;
        for event in events {
            if self.is_trade_backfill(&event) {
                continue;
            }
            if let Err(e) = self.market_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to MarketPubSub");
            }
        }
        Ok(())
    }
}

impl HyperliquidPublicWsActor {
    /// 该事件是否为 trades 订阅前的历史回填 (仅 MarketTrade 适用)。
    ///
    /// 判据：成交时间早于本 symbol 的 trades 订阅时刻。无订阅记录时不判为回填
    /// (宁可放过也不误杀实时成交)。
    fn is_trade_backfill(&self, event: &MarketEvent) -> bool {
        let MarketData::MarketTrade(trade) = &event.data else {
            return false;
        };
        let Some(&subscribed_at) = self.trades_subscribed_at.get(&trade.symbol) else {
            return false;
        };
        if trade.timestamp >= subscribed_at {
            return false;
        }
        tracing::debug!(
            exchange = "Hyperliquid",
            symbol = %trade.symbol,
            trade_ts = trade.timestamp,
            subscribed_at,
            "Dropping pre-subscription trade backfill"
        );
        true
    }
}

impl Actor for HyperliquidPublicWsActor {
    type Args = HyperliquidPublicWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(
            read,
            write,
            outgoing_rx,
            incoming_tx,
            ws_loop::WsKeepalive::hyperliquid(),
        ));

        tracing::info!("HyperliquidPublicWsActor started");

        Ok(Self {
            market_pubsub: args.market_pubsub,
            markets: args.markets,
            ws_tx: Some(outgoing_tx),
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
            trades_subscribed_at: HashMap::new(),
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
        // 委托给批量订阅
        self.handle(SubscribeBatch { kinds: vec![msg.kind] }, ctx)
            .await
    }
}

impl Message<SubscribeBatch> for HyperliquidPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 1. 过滤有效的 kinds（symbol 存在且未订阅）
        let mut new_stream_kinds = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in msg.kinds {
            if self.subscribed_kinds.contains(&kind) {
                continue;
            }

            let stream = match self.stream(&kind) {
                Some(stream) => stream,
                None => {
                    tracing::warn!(
                        exchange = "Hyperliquid",
                        ?kind,
                        "不支持的订阅类型或 symbol 不在 HL 品种目录里，忽略"
                    );
                    continue;
                }
            };
            if !self.subscribed_streams.contains(&stream) {
                new_stream_kinds.push(kind.clone());
                self.subscribed_streams.insert(stream);
            }
            new_kinds.push(kind);
        }

        // 2. 批量发送订阅请求
        if !new_stream_kinds.is_empty() {
            tracing::info!(
                exchange = "Hyperliquid",
                count = new_stream_kinds.len(),
                "Batch subscribing to streams"
            );

            if let Err(e) = self.send_subscribe_batch(&new_stream_kinds).await {
                tracing::error!(error = %e, "Failed to send batch subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }
        }

        // 3. 记录已订阅的 kinds；trades 额外记录订阅时刻以过滤历史回填
        let subscribed_at = now_ms();
        for kind in new_kinds {
            if let SubscriptionKind::Trades { symbol } = &kind {
                self.trades_subscribed_at
                    .insert(symbol.clone(), subscribed_at);
            }
            self.subscribed_kinds.insert(kind);
        }
    }
}

impl Message<Unsubscribe> for HyperliquidPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let SubscriptionKind::Trades { symbol } = &msg.kind {
            self.trades_subscribed_at.remove(symbol);
        }
        if !self.subscribed_kinds.remove(&msg.kind) {
            return;
        }

        // 检查是否还有其他 kinds 使用同一个 stream
        // Candle 从不进入 subscribed_kinds（订阅期已跳过），此处若为 None 属路由 bug，记录并跳过
        let stream = match self.stream(&msg.kind) {
            Some(stream) => stream,
            None => {
                tracing::error!(kind = ?msg.kind, "Candle routed to HyperliquidPublicWsActor unsubscribe (routing bug), skipping");
                return;
            }
        };
        let stream_still_needed = self
            .subscribed_kinds
            .iter()
            .any(|k| self.stream(k).as_ref() == Some(&stream));

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
        crate::dispatch_ws_stream_message!(self, msg, ctx, "Hyperliquid");
    }
}

// ============================================================================
// 消息解析
// ============================================================================

pub(crate) fn parse_public_message(
    raw: &str,
    local_ts: u64,
    subscribed_kinds: &HashSet<SubscriptionKind>,
) -> Result<Vec<MarketEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            // 服务端拒绝/报错：**必须致命**。HL 的错误下发格式是
            // `{"channel":"error","data":"..."}` —— 此前这里检查的是顶层 `error` 键与
            // `subscriptionResponse.data.error`，两处都不是 HL 的真实格式（死代码），
            // 真实错误落进下面的未知分支被 debug 掉：订阅被拒而连接健康、ping/pong
            // 正常，该 symbol 的行情再也不来却无人知晓。
            //
            // 公共流不做私有流那样的逐条订阅记账（订阅是随策略增减的动态集合，记账
            // 成本高）：错误报文即致命已经覆盖"订阅被拒"这一主要来路。
            "error" => {
                return Err(WsError::ParseError(format!(
                    "Hyperliquid 公共流返回错误: {}",
                    value.get("data").unwrap_or(&value)
                )));
            }
            "subscriptionResponse" => {
                return Ok(Vec::new());
            }
            // 应用层心跳应答（HL 文档口径：{"channel":"pong"}）
            "pong" => {
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
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::FundingRate(
                            ctx.to_funding_rate(local_ts)
                                ?,
                        ),
                    });
                }
                if subscribed_kinds
                    .contains(&SubscriptionKind::MarkPrice { symbol: symbol.clone() })
                {
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::MarkPrice(
                            ctx.to_mark_price(local_ts)
                                ?,
                        ),
                    });
                }
                if subscribed_kinds.contains(&SubscriptionKind::IndexPrice { symbol }) {
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::IndexPrice(
                            ctx.to_index_price(local_ts)
                                ?,
                        ),
                    });
                }

                return Ok(events);
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                let bbo_data: WsBbo = serde_json::from_value(data.clone())
                    .map_err(|e| WsError::ParseError(format!("bbo parse: {}", e)))?;

                // Ok(None) = 单边盘口（稀薄品种的合法状态），跳过即可
                let Some(bbo) = bbo_data.to_bbo()? else {
                    tracing::debug!(coin = %bbo_data.coin, "Hyperliquid 单边盘口，跳过该条 BBO");
                    return Ok(Vec::new());
                };
                return Ok(vec![MarketEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: MarketData::BBO(bbo),
                }]);
            }
            "trades" => {
                // trades 一次推送一个数组
                let trades: Vec<WsTrade> = serde_json::from_value(value["data"].clone())
                    .map_err(|e| WsError::ParseError(format!("trades parse: {}", e)))?;

                // 归集为 aggTrade 口径（HL 线路逐笔，Binance/OKX 下发的已是归集结果）
                let events = aggregate_trades(&trades)?
                    .into_iter()
                    .map(|trade| MarketEvent {
                        exchange_ts: trade.timestamp,
                        local_ts,
                        data: MarketData::MarketTrade(trade),
                    })
                    .collect();
                return Ok(events);
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

/// kind 对应的 HL 订阅频道。`None` = 本适配层未实现（如 Candle）。
///
/// 只由 kind 变体决定、与 symbol 无关 —— 能力查询 `supports_subscription` 直接用它。
pub(crate) fn kind_channel(kind: &SubscriptionKind) -> Option<&'static str> {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 activeAssetCtx 订阅
        SubscriptionKind::FundingRate { .. }
        | SubscriptionKind::MarkPrice { .. }
        | SubscriptionKind::IndexPrice { .. } => Some("activeAssetCtx"),
        SubscriptionKind::BBO { .. } => Some("bbo"),
        SubscriptionKind::Trades { .. } => Some("trades"),
        SubscriptionKind::Candle { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, MarketTrade};

    const SUBSCRIBED_AT: u64 = 1_785_747_800_000;

    fn trade_event(ts: u64) -> MarketEvent {
        MarketEvent {
            exchange_ts: ts,
            local_ts: SUBSCRIBED_AT,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Hyperliquid,
                symbol: "BTC".to_string(),
                price: 62_500.0,
                qty: 0.1,
                is_buyer_maker: false,
                timestamp: ts,
            }),
        }
    }

    /// 只构造过滤判定所需的字段；其余字段不参与 `is_trade_backfill`
    fn actor_with_subscription(subscribed: Option<u64>) -> HyperliquidPublicWsActor {
        HyperliquidPublicWsActor {
            market_pubsub: unreachable_pubsub(),
            markets: Arc::new(crate::exchange::hyperliquid::testkit::markets(&["", "xyz"])),
            ws_tx: None,
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
            trades_subscribed_at: match subscribed {
                Some(ts) => HashMap::from([("BTC".to_string(), ts)]),
                None => HashMap::new(),
            },
        }
    }

    /// 过滤判定不触碰 pubsub，故这里只需要一个类型正确的引用
    fn unreachable_pubsub() -> ActorRef<MarketPubSub> {
        use kameo::actor::Spawn;
        use kameo_actors::DeliveryStrategy;
        MarketPubSub::spawn(MarketPubSub::new(DeliveryStrategy::BestEffort))
    }

    #[tokio::test]
    async fn pre_subscription_trade_is_dropped_as_backfill() {
        let actor = actor_with_subscription(Some(SUBSCRIBED_AT));
        assert!(actor.is_trade_backfill(&trade_event(SUBSCRIBED_AT - 1)));
    }

    #[tokio::test]
    async fn trade_at_or_after_subscription_is_kept() {
        let actor = actor_with_subscription(Some(SUBSCRIBED_AT));
        assert!(!actor.is_trade_backfill(&trade_event(SUBSCRIBED_AT)));
        assert!(!actor.is_trade_backfill(&trade_event(SUBSCRIBED_AT + 1)));
    }

    /// 无订阅记录时不判为回填 —— 宁可放过也不误杀实时成交
    #[tokio::test]
    async fn trade_without_subscription_record_is_kept() {
        let actor = actor_with_subscription(None);
        assert!(!actor.is_trade_backfill(&trade_event(SUBSCRIBED_AT - 10_000)));
    }

    #[tokio::test]
    async fn non_trade_event_is_never_backfill() {
        let actor = actor_with_subscription(Some(SUBSCRIBED_AT));
        let clock = MarketEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: MarketData::Clock,
        };
        assert!(!actor.is_trade_backfill(&clock));
    }

    #[tokio::test]
    async fn trades_kind_maps_to_trades_subscription() {
        let actor = actor_with_subscription(None);
        let kind = SubscriptionKind::Trades { symbol: "BTC".to_string() };
        let sub = actor.subscription(&kind).expect("must map");
        assert_eq!(sub["type"], "trades");
        assert_eq!(sub["coin"], "BTC");
    }

    /// 同一个 actor 订阅主站与 xyz：coin 各带各的前缀，目录外的 symbol 不订阅
    #[tokio::test]
    async fn one_actor_subscribes_across_dexes() {
        let actor = actor_with_subscription(None);
        let bbo = |s: &str| SubscriptionKind::BBO { symbol: s.to_string() };
        assert_eq!(actor.subscription(&bbo("TSLA")).unwrap()["coin"], "xyz:TSLA");
        assert_eq!(actor.stream(&bbo("BTC")).as_deref(), Some("bbo:BTC"));
        assert_eq!(
            actor.stream(&SubscriptionKind::FundingRate { symbol: "TSLA".into() }).as_deref(),
            Some("activeAssetCtx:xyz:TSLA")
        );
        assert!(actor.stream(&bbo("AAPL")).is_none());
        assert!(actor.subscription(&bbo("AAPL")).is_err());
    }
}
