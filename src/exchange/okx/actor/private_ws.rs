//! OkxPrivateWsActor - 管理 OKX 私有 WebSocket 连接
//!
//! 职责:
//! - 建立私有 WebSocket 连接并完成登录
//! - 自动订阅私有频道 (positions, account, orders)
//! - 直接解析消息并发布到对应总线

use crate::domain::{now_ms, Balance, Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::messaging::AccountPubSub;
use crate::exchange::client::WsError;
use crate::exchange::okx::codec::{resolve_meta, AccountData as OkxAccountPush, OrderPushData, WsEvent, WsPush};
use crate::exchange::okx::{OkxCredentials, WS_PRIVATE_URL};
use crate::exchange::ws_loop;
use crate::messaging::{AccountData, AccountEvent};
use futures_util::{SinkExt, StreamExt};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 登录响应等待超时 (秒)。超时 → 启动失败受控退出，避免 on_start 永久阻塞。
const LOGIN_TIMEOUT_SECS: u64 = 10;

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs {
    /// 凭证
    pub credentials: OkxCredentials,
    /// 账户私有事件总线（发布事件，标 Live）
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// Symbol 元数据（用于仓位转换）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
}

/// OkxPrivateWsActor - 私有 WebSocket Actor
pub struct OkxPrivateWsActor {
    /// 账户私有事件总线（发布事件，标 Live）
    account_pubsub: ActorRef<AccountPubSub>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
}

impl OkxPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        tracing::debug!(raw, "OKX private message received");
        let local_ts = now_ms();
        let events = parse_private_message(raw, local_ts, &self.symbol_metas)?;
        tracing::debug!(count = events.len(), "OKX private events parsed");
        for event in events {
            if let Err(e) = self.account_pubsub.tell(Publish(event)).send().await {
                tracing::error!(error = %e, "Failed to publish to AccountPubSub");
            }
        }
        Ok(())
    }
}

impl Actor for OkxPrivateWsActor {
    type Args = OkxPrivateWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 连接私有 WebSocket（失败向上传播 → 启动期受控退出）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_PRIVATE_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::OKX, e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // 2. 发送 login 消息
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let sign = args.credentials.sign_ws_login(&timestamp);

        let login_msg = json!({
            "op": "login",
            "args": [{
                "apiKey": args.credentials.api_key,
                "passphrase": args.credentials.passphrase,
                "timestamp": timestamp,
                "sign": sign
            }]
        })
        .to_string();

        write
            .send(WsMessage::Text(login_msg))
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("OKX send login: {e}")))?;

        // 3. 等待 login 响应（带超时；任何失败向上传播 → 启动期受控退出，不重试）
        let login_wait = async {
            loop {
                match read.next().await {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                            if event.event == "login" {
                                if event.code.as_deref() == Some("0") {
                                    tracing::info!("OKX private login success");
                                    return Ok(());
                                } else {
                                    return Err(ExchangeError::AuthenticationFailed(Exchange::OKX));
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        write.send(WsMessage::Pong(data)).await.map_err(|e| {
                            ExchangeError::WebSocketError(format!("OKX pong during login: {e}"))
                        })?;
                    }
                    Some(Err(e)) => {
                        return Err(ExchangeError::WebSocketError(format!(
                            "OKX WS error during login: {e}"
                        )));
                    }
                    None => {
                        return Err(ExchangeError::ConnectionFailed(
                            Exchange::OKX,
                            "WS closed during login".to_string(),
                        ));
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(LOGIN_TIMEOUT_SECS), login_wait)
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!(
                    "OKX private login timed out after {LOGIN_TIMEOUT_SECS}s"
                ))
            })??;

        // 4. 订阅私有频道
        //
        // **不订阅 `positions`**：持仓的维护模型是「启动期 REST 基线 + 之后全程 Fill 累加」，
        // 持仓推送既不是基线（基线只能来自 ManagerActor）也不参与增量（增量走 `orders` 频道
        // 产出的 Fill）。要校验本地持仓是否漂移，走 PositionLedgerActor 那条独立轮询通道
        // （见 crate::engine::PositionLedgerActor），不在这里塞快照。
        let subscribe_msg = json!({
            "op": "subscribe",
            "args": [
                {"channel": "account"},
                {"channel": "orders", "instType": "SWAP"}
            ]
        })
        .to_string();

        write
            .send(WsMessage::Text(subscribe_msg))
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("OKX send private subscribe: {e}")))?;

        // 5. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 6. 创建入站消息 channel (收到的数据/错误)
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 7. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(
            read,
            write,
            outgoing_rx,
            incoming_tx,
            ws_loop::WsKeepalive::okx(),
        ));

        tracing::info!("OkxPrivateWsActor started");

        Ok(Self {
            account_pubsub: args.account_pubsub,
            symbol_metas: args.symbol_metas,
            ws_tx: Some(outgoing_tx),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("OkxPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for OkxPrivateWsActor {
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

fn parse_private_message(
    raw: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<AccountEvent>, WsError> {
    // 应用层心跳应答：ws_loop 周期发文本 "ping"，OKX 回文本 "pong"（非 JSON）
    if raw == "pong" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是事件响应（控制消息，返回空 Vec）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => {
                return Ok(Vec::new());
            }
            "channel-conn-count" => {
                // OKX 连接计数事件，忽略
                return Ok(Vec::new());
            }
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => {
                tracing::warn!(event, raw, "OKX unknown event");
                return Ok(Vec::new());
            }
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing channel: {}", raw)))?;

    match channel {
        "account" => {
            let push: WsPush<OkxAccountPush> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("account parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let exchange_ts = data
                    .u_time
                    .parse::<u64>()
                    .map_err(|_| WsError::ParseError(format!("Failed to parse OKX account timestamp: {}", data.u_time)))?;
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts,
                    local_ts,
                    data: AccountData::AccountInfo {
                        exchange: Exchange::OKX,
                        equity: data.to_equity()?,
                        notional: data.to_notional()?,
                    },
                });

                // 推送每个币种的现金余额 (用于修正 greeks delta)
                for detail in &data.details {
                    let cash_bal: f64 = detail.cash_bal.parse()
                        .map_err(|_| WsError::ParseError(format!(
                            "Failed to parse cash_bal '{}' for ccy {}", detail.cash_bal, detail.ccy)))?;
                    events.push(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts,
                        local_ts,
                        data: AccountData::Balance(Balance {
                            exchange: Exchange::OKX,
                            asset: detail.ccy.clone(),
                            available: cash_bal,
                        }),
                    });
                }
            }
            Ok(events)
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("orders parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                // 缺 meta 无法把张数折为币本位，绝不能按张数下发（会差 contract_size 倍）。
                //
                // 丢弃在这里是**安全**的，因为"本引擎在交易的 symbol 缺 meta"已经不可能
                // 走到运行期：投产期 `validate_subscriptions` 会拒绝任何没有 SymbolMeta
                // 的订阅，元数据加载处的剔除也已改为 error 留痕。所以走到这条分支只剩一种
                // 解释 —— **该 instId 不归本引擎管**：OKX 私有流订阅的是 `instType=SWAP`
                // 全账户推送，同账户里人工下的单、别的进程/分桶实例的成交都会推过来。
                // 那些成交本就不该进本实例的账本，丢弃是正确处理，不是数据丢失。
                //
                // 唯一的残余角落 —— "被元数据剔除的合约上恰好有手工持仓" —— 由另一条
                // 路径响亮兜住：该 symbol 一旦进入持仓/对账流程，`contract_size_of`
                // 查不到乘数会直接报错（见 okx/client.rs 的 contract_size_of）。
                let Some(meta) = resolve_meta(&data.inst_id, symbol_metas) else {
                    tracing::debug!(
                        exchange = "OKX",
                        inst_id = %data.inst_id,
                        "私有回报的 instId 不在本实例的交易范围内（同账户的其他单），忽略"
                    );
                    continue;
                };
                // 数量折算由签名强制完成（含 status 内嵌的 PartiallyFilled）
                let order_update = data.to_order_update(meta)?;

                // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
                if let Some(fill) = data.to_fill(meta)
                    ? {
                    events.push(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts: local_ts,
                        local_ts,
                        data: AccountData::Fill(fill),
                    });
                }

                // OrderUpdate 事件
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts: local_ts,
                    local_ts,
                    data: AccountData::OrderUpdate(order_update),
                });
            }
            Ok(events)
        }
        // account-greeks 已迁移到 REST 轮询 (OkxGreeksPollingActor)
        "account-greeks" => Ok(Vec::new()),
        _ => {
            tracing::warn!(channel, raw, "Unknown OKX private channel");
            Ok(Vec::new())
        }
    }
}
