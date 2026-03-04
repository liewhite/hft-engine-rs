//! IbkrPublicWsActor - 管理 IBKR WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 处理 BBO 订阅 (smd topic)
//! - 处理订单状态推送 (sor topic)
//! - 增量更新 bid/ask 缓存并发布 BBO 到 IncomePubSub

use crate::domain::{now_ms, Exchange, OrderStatus, OrderUpdate, Side, BBO};
use crate::engine::IncomePubSub;
use crate::exchange::client::{Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe, WsError};
use crate::exchange::ibkr::auth::IbkrAuth;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::{handshake::client::generate_key, http};

/// IbkrPublicWsActor 初始化参数
pub struct IbkrPublicWsActorArgs {
    /// 认证器 (共享，不可变)
    pub auth: Arc<dyn IbkrAuth>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// conid 映射 (symbol → conid)
    pub conids: HashMap<String, i64>,
    /// tickle 返回的 session_id (用于 WS Cookie)
    pub session_id: String,
}

/// 每个 conid 的 BBO 缓存 (IB 推送增量数据)
#[derive(Default)]
struct BboCache {
    bid_price: f64,
    ask_price: f64,
    bid_size: f64,
    ask_size: f64,
}

/// IbkrPublicWsActor
pub struct IbkrPublicWsActor {
    income_pubsub: ActorRef<IncomePubSub>,
    /// conid 映射 (symbol → conid)
    conids: HashMap<String, i64>,
    /// 反向映射 (conid → symbol)
    conid_to_symbol: HashMap<i64, String>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 已订阅的 kinds
    subscribed: HashSet<SubscriptionKind>,
    /// 每个 conid 的 BBO 缓存
    bbo_cache: HashMap<i64, BboCache>,
}

impl IbkrPublicWsActor {
    /// 发送 WebSocket 订阅消息
    async fn send_subscribe(&self, conid: i64) -> Result<(), WsError> {
        let msg = format!(
            "smd+{}+{{\"fields\":[\"84\",\"86\",\"85\",\"88\"]}}",
            conid
        );
        let tx = self
            .ws_tx
            .as_ref()
            .expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 发送 WebSocket 取消订阅消息
    async fn send_unsubscribe(&self, conid: i64) -> Result<(), WsError> {
        let msg = format!("umd+{}+{{}}", conid);
        let tx = self
            .ws_tx
            .as_ref()
            .expect("ws_tx must exist after on_start");
        tx.send(msg)
            .await
            .map_err(|_| WsError::Network("Channel closed".to_string()))
    }

    /// 解析并处理 WebSocket 消息
    async fn handle_message(&mut self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();

        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

        // 路由: topic 以 "sor" 开头 → 订单状态更新
        if let Some(topic) = value.get("topic").and_then(|v| v.as_str()) {
            if topic.starts_with("sor") {
                return self.handle_order_update(&value, local_ts).await;
            }
        }

        // 路由: 含 conid → BBO 行情
        let conid = match value.get("conid").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return Ok(()), // 心跳/状态消息
        };

        self.handle_bbo(&value, conid, local_ts).await
    }

    /// 处理 BBO 行情消息
    async fn handle_bbo(
        &mut self,
        value: &serde_json::Value,
        conid: i64,
        local_ts: u64,
    ) -> Result<(), WsError> {
        let symbol = match self.conid_to_symbol.get(&conid) {
            Some(s) => s.clone(),
            None => {
                tracing::warn!(conid, "Received data for unknown conid");
                return Ok(());
            }
        };

        let cache = self.bbo_cache.entry(conid).or_default();

        // IB 字段映射: "84"→bid_price, "86"→ask_price, "85"→ask_size, "88"→bid_size
        // IB 数字可能含逗号 "1,234.56"
        if let Some(v) = value.get("84") {
            if let Some(price) = parse_ib_number(v) {
                cache.bid_price = price;
            }
        }
        if let Some(v) = value.get("86") {
            if let Some(price) = parse_ib_number(v) {
                cache.ask_price = price;
            }
        }
        if let Some(v) = value.get("85") {
            if let Some(size) = parse_ib_number(v) {
                cache.ask_size = size;
            }
        }
        if let Some(v) = value.get("88") {
            if let Some(size) = parse_ib_number(v) {
                cache.bid_size = size;
            }
        }

        // 当 bid > 0 && ask > 0 时发布 BBO
        if cache.bid_price > 0.0 && cache.ask_price > 0.0 {
            let bbo = BBO {
                exchange: Exchange::IBKR,
                symbol,
                bid_price: cache.bid_price,
                bid_qty: cache.bid_size,
                ask_price: cache.ask_price,
                ask_qty: cache.ask_size,
                timestamp: local_ts,
            };

            let event = IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::BBO(bbo),
            };

            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }

        Ok(())
    }

    /// 处理 sor 订单状态更新
    ///
    /// IBKR WS `sor` topic 推送订单更新，包含 order_ref (= 我们的 cOID)。
    /// 跳过无 order_ref 的非策略订单。
    async fn handle_order_update(
        &mut self,
        value: &serde_json::Value,
        local_ts: u64,
    ) -> Result<(), WsError> {
        let args = match value.get("args").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return Ok(()),
        };

        for item in args {
            // order_ref 是我们下单时设置的 cOID
            let order_ref = match item.get("order_ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => r,
                _ => continue, // 跳过非策略订单
            };

            let order_id = item
                .get("orderId")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_default();

            let ib_status = match item.get("status").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };

            let filled_qty = item
                .get("filledQuantity")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            let status = match ib_status {
                "Submitted" if filled_qty > 0.0 => {
                    OrderStatus::PartiallyFilled { filled: filled_qty }
                }
                "PendingSubmit" | "PreSubmitted" | "Submitted" => OrderStatus::Pending,
                "Filled" => OrderStatus::Filled,
                "Cancelled" => OrderStatus::Cancelled,
                "Inactive" => OrderStatus::Rejected {
                    reason: "Inactive".to_string(),
                },
                other => {
                    tracing::debug!(
                        ib_status = other,
                        order_ref,
                        "IBKR unknown order status, ignoring"
                    );
                    continue;
                }
            };

            // 解析 symbol: 通过 conid 反查
            let conid = item.get("conid").and_then(|v| v.as_i64());
            let symbol = conid.and_then(|c| self.conid_to_symbol.get(&c).cloned());

            let symbol = match symbol {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        order_ref,
                        order_id,
                        ?conid,
                        "IBKR order update: cannot resolve symbol"
                    );
                    continue;
                }
            };

            let side_str = item.get("side").and_then(|v| v.as_str()).unwrap_or("");
            let side = match side_str {
                "BUY" | "B" => Side::Long,
                "SELL" | "S" => Side::Short,
                other => {
                    tracing::warn!(side = other, order_ref, "IBKR unknown order side, skipping");
                    continue;
                }
            };

            tracing::info!(
                symbol = %symbol,
                order_ref,
                order_id,
                ib_status,
                ?status,
                filled_qty,
                "IBKR order status update"
            );

            let update = OrderUpdate {
                order_id,
                client_order_id: Some(order_ref.to_string()),
                exchange: Exchange::IBKR,
                symbol,
                side,
                status,
                filled_quantity: filled_qty,
                fill_sz: 0.0, // sor 推送无增量 fill 信息
                timestamp: local_ts,
            };

            let event = IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::OrderUpdate(update),
            };

            let _ = self.income_pubsub.tell(Publish(event)).send().await;
        }

        Ok(())
    }
}

impl Actor for IbkrPublicWsActor {
    type Args = IbkrPublicWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 构建反向映射
        let conid_to_symbol: HashMap<i64, String> = args
            .conids
            .iter()
            .map(|(s, c)| (*c, s.clone()))
            .collect();

        // 连接 WebSocket (需要 Cookie + User-Agent + 标准 WebSocket 握手 header)
        let ws_url = args.auth.ws_url();
        let connector = args.auth.ws_connector();
        let cookie = args.auth.format_ws_cookie(&args.session_id);
        let uri: http::Uri = ws_url.parse().expect("Invalid WS URL");
        let host = uri.host().expect("WS URL missing host");
        let ws_key = generate_key();
        let ws_request = http::Request::builder()
            .uri(&ws_url)
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &ws_key)
            .header("Cookie", &cookie)
            .header("User-Agent", "ClientPortalGW/1")
            .body(())
            .expect("Failed to build WS request");

        tracing::info!(ws_url = %ws_url, cookie = %cookie, "Connecting IBKR WebSocket");

        let (ws_stream, _) = match connector {
            Some(conn) => {
                tokio_tungstenite::connect_async_tls_with_config(
                    ws_request,
                    None,
                    false,
                    Some(conn),
                )
                .await
                .expect("Failed to connect to IBKR WebSocket")
            }
            None => {
                tokio_tungstenite::connect_async(ws_request)
                    .await
                    .expect("Failed to connect to IBKR WebSocket")
            }
        };

        let (write, read) = ws_stream.split();

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        // 订阅订单状态推送 (sor topic)
        if let Err(e) = outgoing_tx.send("sor+{}".to_string()).await {
            tracing::error!(error = %e, "Failed to send IBKR sor subscription");
        } else {
            tracing::info!("IBKR sor (order status) subscription sent");
        }

        tracing::info!("IbkrPublicWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            conids: args.conids,
            conid_to_symbol,
            ws_tx: Some(outgoing_tx),
            subscribed: HashSet::new(),
            bbo_cache: HashMap::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("IbkrPublicWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

impl Message<Subscribe> for IbkrPublicWsActor {
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

impl Message<SubscribeBatch> for IbkrPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeBatch,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        for kind in msg.kinds {
            // IBKR 只支持 BBO，其他类型静默忽略
            let symbol = match &kind {
                SubscriptionKind::BBO { symbol } => symbol.clone(),
                _ => continue,
            };

            if self.subscribed.contains(&kind) {
                continue;
            }

            let conid = match self.conids.get(&symbol) {
                Some(c) => *c,
                None => {
                    tracing::warn!(
                        exchange = "IBKR",
                        symbol = %symbol,
                        "Symbol not found in conid mapping, ignoring"
                    );
                    continue;
                }
            };

            if let Err(e) = self.send_subscribe(conid).await {
                tracing::error!(error = %e, "Failed to send IBKR subscribe, killing actor");
                ctx.actor_ref().kill();
                return;
            }

            self.subscribed.insert(kind);
            tracing::info!(
                exchange = "IBKR",
                symbol = %symbol,
                conid,
                "Subscribed to BBO"
            );
        }
    }
}

impl Message<Unsubscribe> for IbkrPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Unsubscribe,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.subscribed.remove(&msg.kind) {
            return;
        }

        if let SubscriptionKind::BBO { ref symbol } = msg.kind {
            if let Some(&conid) = self.conids.get(symbol) {
                if let Err(e) = self.send_unsubscribe(conid).await {
                    tracing::error!(error = %e, "Failed to send IBKR unsubscribe, killing actor");
                    ctx.actor_ref().kill();
                }
            }
        }
    }
}

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for IbkrPublicWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Result<String, WsError>, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(Ok(data)) => {
                if let Err(e) = self.handle_message(&data).await {
                    tracing::error!(exchange = "IBKR", error = %e, raw = %data, "Public WS parse error, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "IBKR Public WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("IBKR WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("IBKR WebSocket stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 解析 IB 返回的数字 (可能含逗号 "1,234.56")
fn parse_ib_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let cleaned = s.replace(',', "");
            match cleaned.parse::<f64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    tracing::warn!(raw = %s, "Failed to parse IB number");
                    None
                }
            }
        }
        _ => None,
    }
}
