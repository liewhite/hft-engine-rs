//! BinancePrivateWsActor - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 管理 BinanceListenKeyActor 子 actor (定时刷新 ListenKey)
//! - 直接解析消息并发布到对应总线

use crate::actor_lifecycle::ChildGroup;
use super::listen_key::{BinanceListenKeyActor, BinanceListenKeyActorArgs};
use crate::domain::{now_ms, Exchange, ExchangeError};
use crate::engine::AccountPubSub;
use crate::exchange::binance::codec::{AccountUpdate, OrderTradeUpdate, WsResponse};
use crate::exchange::binance::{BinanceCredentials, WS_PRIVATE_URL};
use crate::exchange::client::WsError;
use crate::exchange::ws_loop;
use crate::messaging::{AccountData, AccountEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// BinancePrivateWsActor 初始化参数
pub struct BinancePrivateWsActorArgs {
    /// 凭证
    pub credentials: BinanceCredentials,
    /// REST API 基础 URL
    pub rest_base_url: String,
    /// 账户私有事件总线（发布事件，标 Live）
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// 计价币种 (e.g., "USDT")
    pub quote: String,
}

/// BinancePrivateWsActor - 私有 WebSocket Actor
pub struct BinancePrivateWsActor {
    /// 账户私有事件总线（发布事件，标 Live）
    account_pubsub: ActorRef<AccountPubSub>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
    /// 全部子 actor：谁 spawn 的谁负责等（见 [`crate::actor_lifecycle`]）
    children: ChildGroup,
}

impl BinancePrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        let events = parse_private_message(raw, &self.quote, local_ts)?;
        for event in events {
            if let Err(e) = self.account_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to AccountPubSub");
            }
        }
        Ok(())
    }
}

impl Actor for BinancePrivateWsActor {
    type Args = BinancePrivateWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 获取 ListenKey（失败向上传播 → 启动期受控退出；属鉴权语义）
        let listen_key = create_listen_key(&args.rest_base_url, &args.credentials.api_key)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "Failed to create Binance listen key");
                ExchangeError::AuthenticationFailed(Exchange::Binance)
            })?;

        // 2. 连接私有 WebSocket（迁移后用 query 参数传 listenKey；未带 events 让交易所推全部事件）
        // 失败向上传播 → 启动期受控退出，不重试/不重连
        let url = format!("{}?listenKey={}", WS_PRIVATE_URL, listen_key);
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 3. 创建出站消息 channel (Subscribe/Unsubscribe)
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 4. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 5. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(
            read,
            write,
            outgoing_rx,
            incoming_tx,
            ws_loop::WsKeepalive::binance(),
        ));

        // 6. spawn_link ListenKeyActor 并等就绪
        let mut children = ChildGroup::default();
        let listen_key_actor = children.spawn::<BinanceListenKeyActor, _>(
            &actor_ref,
            "BinanceListenKeyActor",
            BinanceListenKeyActorArgs {
                rest_base_url: args.rest_base_url,
                api_key: args.credentials.api_key,
            },
        )
        .await;
        listen_key_actor
            .wait_for_startup_result()
            .await
            .map_err(|e| {
                ExchangeError::Other(format!("BinanceListenKeyActor failed to start: {e}"))
            })?;

        tracing::info!("BinancePrivateWsActor started");

        Ok(Self {
            account_pubsub: args.account_pubsub,
            quote: args.quote,
            ws_tx: Some(outgoing_tx),
            children,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // Drop ws_tx 会导致 ws_loop 退出
        self.ws_tx.take();
        // 谁 spawn 的谁负责等（见 actor_lifecycle 模块文档）
        self.children.shutdown().await;
        tracing::info!("BinancePrivateWsActor stopped");
        Ok(())
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
        crate::dispatch_ws_stream_message!(self, msg, ctx, "Binance");
    }
}

// ============================================================================
// 消息解析
// ============================================================================

fn parse_private_message(
    raw: &str,
    quote: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, WsError> {
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
        .unwrap_or_else(|| {
            tracing::warn!(raw, "Missing exchange timestamp (E), using local_ts");
            local_ts
        });

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

            // **不处理 `a.P`（持仓）**：那是持仓**快照**，而持仓的维护模型是「启动期 REST
            // 基线 + 之后全程 Fill 累加」（见 crate::messaging::PositionBaseline）。快照既
            // 当不了基线（基线只能来自 ManagerActor），也不能参与增量（会与同一条推送里的
            // ORDER_TRADE_UPDATE 产出的 Fill 重复计算）。校验走 PositionReconcileActor 的独立轮询。

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                let balance = bal_data.to_balance()
                    ?;
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts,
                    local_ts,
                    data: AccountData::Balance(balance),
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
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts,
                    local_ts,
                    data: AccountData::Fill(fill),
                });
            }

            events.push(AccountEvent {
                // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                account: crate::domain::AccountId::Live,
                exchange_ts,
                local_ts,
                data: AccountData::OrderUpdate(
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
