//! OkxBusinessWsActor - 管理 OKX Business WebSocket 连接 (K线数据)
//!
//! 职责:
//! - 维护 Business WebSocket 连接 (wss://ws.okx.com:8443/ws/v5/business)
//! - 处理 Candle 类型的 Subscribe/Unsubscribe 请求
//! - 收到订阅时先通过 REST 获取 100 根历史 K线并发布，再 WS 订阅实时推送
//! - 直接解析消息并发布到 IncomePubSub

use crate::domain::{CandleInterval, now_ms, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::okx::codec::{
    CandleRawData, WsPush, candle_interval_to_okx_bar, okx_channel_to_candle_interval,
    parse_candle_data,
};
use crate::exchange::okx::{to_okx, REST_BASE_URL, WS_BUSINESS_URL};
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// OkxBusinessWsActor 初始化参数
pub struct OkxBusinessWsActorArgs {
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// OkxBusinessWsActor - Business WebSocket Actor (K线)
pub struct OkxBusinessWsActor {
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
    /// HTTP 客户端 (用于获取历史 K线, 公开 API 无需签名)
    http_client: Client,
}

impl OkxBusinessWsActor {
    /// 发送 WS 订阅消息
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

    /// 发送 WS 取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_business_arg(kind, &self.quote);
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

    /// 获取历史 K线并以 HistoryCandles 事件发布
    async fn fetch_and_publish_history(
        &self,
        symbol: &Symbol,
        interval: CandleInterval,
    ) -> Result<(), WsError> {
        let inst_id = to_okx(symbol, &self.quote);
        let bar = candle_interval_to_okx_bar(interval);
        let url = format!(
            "{}/api/v5/market/candles?instId={}&bar={}&limit=100",
            REST_BASE_URL, inst_id, bar
        );

        #[derive(Deserialize)]
        struct Response {
            code: String,
            msg: String,
            data: Vec<CandleRawData>,
        }

        let resp = self.http_client.get(&url).send().await
            .map_err(|e| WsError::Network(format!("History candles request failed: {}", e)))?;

        let data: Response = resp.json().await
            .map_err(|e| WsError::ParseError(format!("History candles parse failed: {}", e)))?;

        if data.code != "0" {
            return Err(WsError::ParseError(format!(
                "OKX history candles API error: code={}, msg={}", data.code, data.msg
            )));
        }

        // OKX 返回的数据按时间倒序，反转为正序
        let candles: Vec<_> = data.data.iter().rev()
            .map(|raw| parse_candle_data(raw, &inst_id, interval))
            .collect();

        let count = candles.len();
        let local_ts = now_ms();
        let event = IncomeEvent {
            exchange_ts: local_ts,
            local_ts,
            data: ExchangeEventData::HistoryCandles(candles),
        };
        let _ = self.income_pubsub.tell(Publish(event)).send().await;

        tracing::info!(
            symbol = %symbol,
            interval = %interval,
            count,
            "Published history candles"
        );
        Ok(())
    }

    /// 解析并处理 Business WS 消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_business_message(raw, local_ts)?;
        for event in events {
            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }
        Ok(())
    }
}

impl Actor for OkxBusinessWsActor {
    type Args = OkxBusinessWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 连接 Business WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_BUSINESS_URL)
            .await
            .expect("Failed to connect to OKX business WebSocket");

        let (write, read) = ws_stream.split();

        // 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 创建入站消息 channel
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        tracing::info!("OkxBusinessWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            subscribed: HashSet::new(),
            http_client,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("OkxBusinessWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for OkxBusinessWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Subscribe,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle(SubscribeBatch { kinds: vec![msg.kind] }, ctx)
            .await
    }
}

impl Message<SubscribeBatch> for OkxBusinessWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut new_args = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in msg.kinds {
            let symbol = kind.symbol();
            if !self.symbol_metas.contains_key(symbol) {
                tracing::warn!(
                    exchange = "OKX",
                    symbol = %symbol,
                    "Symbol not found in symbol_metas, ignoring candle subscription"
                );
                continue;
            }

            if self.subscribed.contains(&kind) {
                continue;
            }

            new_args.push(kind_to_business_arg(&kind, &self.quote));
            new_kinds.push(kind);
        }

        // 1. 对每个新订阅先获取历史 K线（阻塞式，保证历史数据先于实时到达）
        for kind in &new_kinds {
            if let SubscriptionKind::Candle { symbol, interval } = kind {
                if let Err(e) = self.fetch_and_publish_history(symbol, *interval).await {
                    tracing::error!(error = %e, "Failed to fetch history candles, killing actor");
                    ctx.actor_ref().kill();
                    return;
                }
            }
        }

        // 2. WS 订阅实时推送（历史获取完毕后再订阅，确保顺序）
        if !new_args.is_empty() {
            tracing::info!(
                exchange = "OKX",
                count = new_args.len(),
                "Batch subscribing to business channels"
            );

            if let Err(e) = self.send_subscribe_batch(new_args).await {
                tracing::error!(error = %e, "Failed to send batch subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }
        }

        // 3. 记录已订阅
        for kind in new_kinds {
            self.subscribed.insert(kind);
        }
    }
}

impl Message<Unsubscribe> for OkxBusinessWsActor {
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
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for OkxBusinessWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Result<String, WsError>, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                if let Err(e) = self.handle_message(&data).await {
                    tracing::error!(exchange = "OKX", error = %e, raw = %data, "Business WS parse error, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "Business WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Business WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("Business WebSocket stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_business_message(raw: &str, local_ts: u64) -> Result<Vec<IncomeEvent>, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查事件响应（控制消息）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return Ok(Vec::new()),
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX business error: code={}, msg={}",
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

    // 匹配 candle 频道 (candle1m, candle5m, candle1H, ...)
    if let Some(interval) = okx_channel_to_candle_interval(channel) {
        let push: WsPush<CandleRawData> = serde_json::from_str(raw)
            .map_err(|e| WsError::ParseError(format!("{} parse: {}", channel, e)))?;
        let inst_id = push
            .arg
            .inst_id
            .as_ref()
            .ok_or_else(|| WsError::ParseError(format!("Missing instId in {}", channel)))?;

        let events = push
            .data
            .iter()
            .map(|raw_data| {
                let candle = parse_candle_data(raw_data, inst_id, interval);
                IncomeEvent {
                    exchange_ts: candle.open_time,
                    local_ts,
                    data: ExchangeEventData::Candle(candle),
                }
            })
            .collect();
        return Ok(events);
    }

    tracing::warn!(channel, raw, "Unknown OKX business channel");
    Ok(Vec::new())
}

// ============================================================================
// 辅助函数
// ============================================================================

fn kind_to_business_arg(kind: &SubscriptionKind, quote: &str) -> serde_json::Value {
    match kind {
        SubscriptionKind::Candle { symbol, interval } => {
            let bar = candle_interval_to_okx_bar(*interval);
            json!({
                "channel": format!("candle{}", bar),
                "instId": to_okx(symbol, quote)
            })
        }
        _ => panic!("BusinessWsActor only handles Candle subscriptions"),
    }
}
