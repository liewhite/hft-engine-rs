//! OkxPublicWsActor - 管理 OKX 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接
//! - 处理 Subscribe/Unsubscribe 请求
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{now_ms, Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::okx::codec::{
    resolve_meta, BboData, FundingRateData, IndexTickerData, MarkPriceData, TradeData, WsPush,
};
use crate::exchange::okx::{to_okx, to_okx_index, WS_PUBLIC_URL};
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
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

        let tx = self
            .ws_tx
            .as_ref()
            .ok_or_else(|| WsError::Network("ws_tx unavailable (actor stopped)".to_string()))?;
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind, &self.quote).ok_or_else(|| {
            WsError::ParseError("Candle kind reached OkxPublicWsActor unsubscribe (routing bug)".to_string())
        })?;
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
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

    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_public_message(raw, local_ts, &self.symbol_metas)?;
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
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PUBLIC_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

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
            ws_loop::WsKeepalive::okx(),
        ));

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

            match kind_to_arg(&kind, &self.quote) {
                Some(arg) => {
                    new_args.push(arg);
                    new_kinds.push(kind);
                }
                None => {
                    tracing::error!(?kind, "Candle routed to OkxPublicWsActor (routing bug), skipping");
                }
            }
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
        crate::dispatch_ws_stream_message!(self, msg, ctx, "OKX");
    }
}

// ============================================================================
// 消息解析
// ============================================================================

/// `symbol_metas` 用于把 OKX 的**张数**口径折算为币本位 (与 `parse_private_message` 同一套路)。
pub(crate) fn parse_public_message(
    raw: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<IncomeEvent>, WsError> {
    // 应用层心跳应答：ws_loop 周期发文本 "ping"，OKX 回文本 "pong"（非 JSON）
    if raw == "pong" {
        return Ok(Vec::new());
    }
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
                    ?;
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

            // 盘口量也是张数，需按 meta 折算；缺 meta 无法折算，丢弃并告警
            let Some(meta) = resolve_meta(inst_id, symbol_metas) else {
                tracing::warn!(exchange = "OKX", %inst_id, "Missing SymbolMeta for bbo, dropping");
                return Ok(Vec::new());
            };

            let mut events = Vec::new();
            for data in &push.data {
                // Ok(None) = 单边盘口（稀薄品种的合法状态），跳过该条即可
                let Some(bbo) = data.to_bbo(meta)? else {
                    tracing::debug!(symbol = %meta.symbol, "OKX 单边盘口，跳过该条 BBO");
                    continue;
                };
                events.push(IncomeEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: ExchangeEventData::BBO(bbo),
                });
            }
            Ok(events)
        }
        "trades" => {
            let push: WsPush<TradeData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("trades parse: {}", e)))?;
            let inst_id = push
                .arg
                .inst_id
                .as_ref()
                .ok_or_else(|| WsError::ParseError("Missing instId in trades".into()))?;

            let Some(meta) = resolve_meta(inst_id, symbol_metas) else {
                tracing::warn!(exchange = "OKX", %inst_id, "Missing SymbolMeta for trades, dropping");
                return Ok(Vec::new());
            };

            let mut events = Vec::new();
            for data in &push.data {
                let trade = data.to_market_trade(meta)?;
                events.push(IncomeEvent {
                    exchange_ts: trade.timestamp,
                    local_ts,
                    data: ExchangeEventData::MarketTrade(trade),
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
                    ?;
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
                    ?;
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

/// 把订阅类型转成 OKX 频道 arg。
///
/// `Candle` 由 OkxActor 路由到 BusinessWsActor，正常不会到达此处；返回 `None` 表示
/// 路由错误，由调用方记录并跳过（不 panic，遵循"框架层错误受控处理"约定）。
pub(crate) fn kind_to_arg(kind: &SubscriptionKind, quote: &str) -> Option<serde_json::Value> {
    match kind {
        SubscriptionKind::FundingRate { symbol } => Some(json!({
            "channel": "funding-rate",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::BBO { symbol } => Some(json!({
            "channel": "bbo-tbt",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::Trades { symbol } => Some(json!({
            // `trades` 按主动单 + 成交价归集推送；`trades-all` 才是每笔明细，此处不需要
            "channel": "trades",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::MarkPrice { symbol } => Some(json!({
            "channel": "mark-price",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::IndexPrice { symbol } => Some(json!({
            // OKX index-tickers 使用指数 ID 格式 (如 BTC-USDT)
            "channel": "index-tickers",
            "instId": to_okx_index(symbol, quote)
        })),
        SubscriptionKind::Candle { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, MarketTrade};
    use crate::exchange::utils::StepFormatter;

    const TRADES_PUSH: &str = r#"{
        "arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},
        "data":[{"instId":"BTC-USDT-SWAP","tradeId":"1","px":"62500.0",
                 "sz":"12","side":"buy","ts":"1785747728201","count":"3"}]
    }"#;

    /// BTC-USDT-SWAP: ctVal = 0.01 BTC/张
    fn metas(contract_size: f64) -> HashMap<Symbol, SymbolMeta> {
        HashMap::from([(
            "BTC".to_string(),
            SymbolMeta {
                exchange: Exchange::OKX,
                symbol: "BTC".to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 0.01,
                min_order_size: 0.01,
                contract_size,
            },
        )])
    }

    fn trades_of(events: Vec<IncomeEvent>) -> Vec<MarketTrade> {
        events
            .into_iter()
            .filter_map(|e| match e.data {
                ExchangeEventData::MarketTrade(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn trade_size_is_converted_from_contracts_to_coin() {
        let events = parse_public_message(TRADES_PUSH, 0, &metas(0.01)).expect("parse");
        let trades = trades_of(events);
        assert_eq!(trades.len(), 1);
        // 12 张 x 0.01 = 0.12 BTC；若漏了折算会是 12（差 100 倍）
        assert!((trades[0].qty - 0.12).abs() < 1e-12, "got {}", trades[0].qty);
        assert_eq!(trades[0].price, 62500.0);
        assert!(!trades[0].is_buyer_maker, "side=buy 是主动买");
    }

    /// 缺 SymbolMeta 时必须丢弃，绝不能把张数当币数发下去
    #[test]
    fn trade_without_symbol_meta_is_dropped_not_passed_through() {
        let events = parse_public_message(TRADES_PUSH, 0, &HashMap::new()).expect("parse");
        assert!(trades_of(events).is_empty());
    }

    #[test]
    fn trades_kind_maps_to_trades_channel() {
        let kind = SubscriptionKind::Trades { symbol: "BTC".to_string() };
        let arg = kind_to_arg(&kind, "USDT").expect("must map");
        assert_eq!(arg["channel"], "trades");
        assert_eq!(arg["instId"], "BTC-USDT-SWAP");
    }
}
