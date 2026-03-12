//! BinancePrivateWsActor - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 管理 BinanceListenKeyActor 子 actor (定时刷新 ListenKey)
//! - 直接解析消息并发布到 IncomePubSub

use super::listen_key::{BinanceListenKeyActor, BinanceListenKeyActorArgs};
use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::engine::IncomePubSub;
use crate::exchange::binance::codec::{AccountUpdate, OrderTradeUpdate, WsResponse};
use crate::exchange::binance::BinanceCredentials;
use crate::exchange::client::WsError;
use crate::exchange::ws_loop;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::mailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Private WebSocket URL 基础
const WS_PRIVATE_BASE: &str = "wss://fstream.binance.com/ws";

/// BinancePrivateWsActor 初始化参数
pub struct BinancePrivateWsActorArgs {
    /// 凭证
    pub credentials: BinanceCredentials,
    /// REST API 基础 URL
    pub rest_base_url: String,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据（用于仓位转换）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// BinancePrivateWsActor - 私有 WebSocket Actor
pub struct BinancePrivateWsActor {
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// ListenKey 子 actor
    listen_key_actor: Option<ActorRef<BinanceListenKeyActor>>,
    /// 子 actor ID 映射
    listen_key_actor_id: Option<ActorId>,
}

impl BinancePrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_private_message(raw, &self.quote, local_ts, &self.symbol_metas)?;
        for event in events {
            if let Err(e) = self.income_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to IncomePubSub");
            }
        }
        Ok(())
    }
}

impl Actor for BinancePrivateWsActor {
    type Args = BinancePrivateWsActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 获取 ListenKey
        let listen_key = create_listen_key(&args.rest_base_url, &args.credentials.api_key)
            .await
            .expect("Failed to create listen key");

        // 2. 连接私有 WebSocket
        let url = format!("{}/{}", WS_PRIVATE_BASE, listen_key);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("Failed to connect to Binance private WebSocket");

        let (write, read) = ws_stream.split();

        // 3. 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 4. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 5. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(read, write, outgoing_rx, incoming_tx));

        // 6. spawn_link ListenKeyActor
        let listen_key_actor = BinanceListenKeyActor::spawn_link_with_mailbox(
            &actor_ref,
            BinanceListenKeyActorArgs {
                rest_base_url: args.rest_base_url,
                api_key: args.credentials.api_key,
            },
            mailbox::unbounded(),
        )
        .await;
        let listen_key_actor_id = listen_key_actor.id();

        tracing::info!("BinancePrivateWsActor started");

        Ok(Self {
            income_pubsub: args.income_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            listen_key_actor: Some(listen_key_actor),
            listen_key_actor_id: Some(listen_key_actor_id),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        tracing::info!("BinancePrivateWsActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        // ListenKeyActor 死亡 -> 级联退出
        if Some(id) == self.listen_key_actor_id {
            tracing::error!(reason = ?reason, "BinanceListenKeyActor died, shutting down");
            self.listen_key_actor = None;
            self.listen_key_actor_id = None;
            return Ok(ControlFlow::Break(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }));
        }

        tracing::warn!(actor_id = ?id, reason = ?reason, "Unknown linked actor died");
        Ok(ControlFlow::Continue(()))
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for BinancePrivateWsActor {
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
                    tracing::error!(exchange = "Binance", error = %e, raw = %data, "Private WS parse error, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Next(Err(e)) => {
                tracing::error!(error = %e, "Private WebSocket loop exited, killing actor");
                ctx.actor_ref().kill();
            }
            StreamMessage::Started(_) => {
                tracing::debug!("WsIncoming stream started");
            }
            StreamMessage::Finished(_) => {
                // ws_loop 异常退出，kill actor 触发级联退出
                tracing::error!("Private WebSocket stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_private_message(
    raw: &str,
    quote: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
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
        "ACCOUNT_UPDATE" => {
            let update: AccountUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("ACCOUNT_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // 处理所有 position 更新
            for pos_data in &update.a.positions {
                let mut position = pos_data.to_position(quote)
                    ?;
                // qty 归一化: 张 -> 币
                // Binance 私有 WS 只会推送已配置 symbol 的仓位（因为只订阅了这些 symbol），
                // 因此 symbol_metas 查找不会失败
                let meta = symbol_metas
                    .get(&position.symbol)
                    .expect("SymbolMeta not found: Binance private WS should only push configured symbols");
                position.size = meta.qty_to_coin(position.size);
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::Position(position),
                });
            }

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                let balance = bal_data.to_balance()
                    ?;
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::Balance(balance),
                });
            }

            Ok(events)
        }
        "ORDER_TRADE_UPDATE" => {
            let update: OrderTradeUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("ORDER_TRADE_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
            if let Some(fill) = update.to_fill(quote)
                ? {
                events.push(IncomeEvent {
                    exchange_ts,
                    local_ts,
                    data: ExchangeEventData::Fill(fill),
                });
            }

            events.push(IncomeEvent {
                exchange_ts,
                local_ts,
                data: ExchangeEventData::OrderUpdate(
                    update.to_order_update(quote)
                        ?,
                ),
            });

            Ok(events)
        }
        "TRADE_LITE" => {
            // 忽略 TRADE_LITE 消息（轻量成交通知，已在 ORDER_TRADE_UPDATE 中处理）
            Ok(Vec::new())
        }
        _ => {
            // 未知事件类型，记录警告但不报错
            tracing::warn!(event_type, raw, "Unknown Binance private event type");
            Ok(Vec::new())
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

async fn create_listen_key(rest_base_url: &str, api_key: &str) -> Result<String, WsError> {
    #[derive(serde::Deserialize)]
    struct Response {
        #[serde(rename = "listenKey")]
        listen_key: String,
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/fapi/v1/listenKey", rest_base_url))
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(WsError::AuthFailed(format!(
            "Failed to create listen key: {}",
            text
        )));
    }

    let data: Response = resp
        .json()
        .await
        .map_err(|e| WsError::AuthFailed(e.to_string()))?;

    Ok(data.listen_key)
}
