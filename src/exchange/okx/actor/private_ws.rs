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
use crate::messaging::{SymbolAccountData, ExchangeAccountData, AccountData, AccountEvent};
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
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 登录响应等待超时 (秒)。超时 → 启动失败受控退出，避免 on_start 永久阻塞。
const LOGIN_TIMEOUT_SECS: u64 = 10;

/// 私有流必须全部确认的频道：account（余额）、orders（订单与成交）
const REQUIRED_CHANNELS: [&str; 2] = ["account", "orders"];

/// OkxPrivateWsActor 初始化参数
pub struct OkxPrivateWsActorArgs {
    /// 凭证
    pub credentials: OkxCredentials,
    /// 账户私有事件总线（发布事件，标 Live）
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// Symbol 元数据（用于仓位转换）
    pub symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币：只有该计价币的永续映射到内部 symbol（见 `okx::symbol`）
    pub quote: String,
}

/// OkxPrivateWsActor - 私有 WebSocket Actor
///
/// `on_start` 返回前已登录、且 [`REQUIRED_CHANNELS`] 都已确认订阅生效。
pub struct OkxPrivateWsActor {
    /// 账户私有事件总线（发布事件，标 Live）
    account_pubsub: ActorRef<AccountPubSub>,
    /// Symbol 元数据
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    quote: String,
    /// 发送消息到 ws_loop 的 channel
    ws_tx: Option<mpsc::Sender<String>>,
}

impl OkxPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&self, raw: &str) -> Result<(), WsError> {
        tracing::debug!(raw, "OKX private message received");
        let local_ts = now_ms();
        let events = parse_private_message(raw, local_ts, &self.quote, &self.symbol_metas)?;
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
                {"channel": REQUIRED_CHANNELS[0]},
                {"channel": REQUIRED_CHANNELS[1], "instType": "SWAP"}
            ]
        })
        .to_string();

        write
            .send(WsMessage::Text(subscribe_msg))
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("OKX send private subscribe: {e}")))?;

        // 5. 起 ws_loop（心跳、回 Pong、断线判定都归它）
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);
        tokio::spawn(ws_loop::run_ws_loop(
            read,
            write,
            outgoing_rx,
            incoming_tx,
            ws_loop::WsKeepalive::okx(),
        ));

        // 6. 等两个频道都确认才算启动完成；期间收到的帧排在后续数据之前交给本 actor
        let incoming = ws_loop::await_subscriptions(Exchange::OKX, incoming_rx, &REQUIRED_CHANNELS, subscription_confirmed).await?;
        actor_ref.attach_stream(incoming, (), ());

        tracing::info!("OkxPrivateWsActor started");

        Ok(Self {
            account_pubsub: args.account_pubsub,
            symbol_metas: args.symbol_metas,
            quote: args.quote,
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

fn server_error(value: &serde_json::Value) -> WsError {
    let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
    let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
    WsError::ParseError(format!("OKX error: code={code}, msg={msg}"))
}

/// 启动期订阅确认判据（见 [`ws_loop::await_subscriptions`]）：订阅回执
/// `{"event":"subscribe","arg":{"channel":"orders",…}}` 或该频道数据到达都算；
/// `{"event":"error",…}`（如订阅被拒）启动失败
fn subscription_confirmed(raw: &str) -> Result<Option<String>, WsError> {
    if raw == "pong" {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;
    let channel = value.pointer("/arg/channel").and_then(|c| c.as_str()).map(str::to_string);
    match value.get("event").and_then(|v| v.as_str()) {
        Some("error") => Err(server_error(&value)),
        Some("subscribe") | None => Ok(channel),
        Some(_) => Ok(None),
    }
}

fn parse_private_message(
    raw: &str,
    local_ts: u64,
    quote: &str,
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
            "error" => return Err(server_error(&value)),
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
                // 净值与持仓总价值不从这里取，由 EquityPollingActor 走 REST（见 codec::AccountData）。
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
                        data: AccountData::Exchange(ExchangeAccountData::Balance(Balance {
                            exchange: Exchange::OKX,
                            asset: detail.ccy.clone(),
                            available: cash_bal,
                        })),
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
                // 私有流订阅的是 `instType=SWAP` 全账户推送，别的计价币、币本位合约的单也会
                // 推过来。映射不到本引擎 symbol（或无 meta、无法折算张数）的：订单状态不关心，
                // 成交以 UnmappedFill 原样上报，不进任何 symbol 的持仓 —— 绝不能按张数下发
                // 或当成同名的 USDT 合约。市场元数据加载的是该计价币的全部永续，故「映射得到却无
                // meta」只剩字段异常被剔除的合约（加载处已 error 留痕）。
                let Some(meta) = resolve_meta(&data.inst_id, quote, symbol_metas) else {
                    if let Some(f) = data.to_unmapped_fill().map_err(WsError::ParseError)? {
                        events.push(AccountEvent {
                            account: crate::domain::AccountId::Live,
                            exchange_ts: local_ts,
                            local_ts,
                            data: AccountData::Exchange(ExchangeAccountData::UnmappedFill(f)),
                        });
                    }
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
                        data: AccountData::Symbol(SymbolAccountData::Fill(fill)),
                    });
                }

                // OrderUpdate 事件
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts: local_ts,
                    local_ts,
                    data: AccountData::Symbol(SymbolAccountData::OrderUpdate(order_update)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::StepFormatter;

    /// 已接入 BTC-USDT-SWAP（每张 0.01 BTC）
    fn metas() -> HashMap<Symbol, SymbolMeta> {
        HashMap::from([(
            "BTC".to_string(),
            SymbolMeta {
                exchange: Exchange::OKX,
                symbol: "BTC".to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 1.0,
                min_order_size: 1.0,
                min_notional: 0.0,
                contract_size: 0.01,
            },
        )])
    }

    fn order_push(inst_id: &str, fill_sz: &str) -> String {
        format!(
            r#"{{"arg":{{"channel":"orders","instType":"SWAP"}},"data":[{{"instId":"{inst_id}","ordId":"1",
            "clOrdId":"x-a","side":"buy","state":"filled","sz":"{fill_sz}","fillSz":"{fill_sz}",
            "fillPx":"60000","fillFee":"-0.1"}}]}}"#
        )
    }

    fn kinds(raw: &str) -> Vec<String> {
        parse_private_message(raw, 0, "USDT", &metas())
            .unwrap()
            .into_iter()
            .map(|e| match e.data {
                AccountData::Symbol(SymbolAccountData::Fill(f)) => format!("fill:{}:{}", f.symbol, f.size),
                AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => format!("order:{}", u.symbol),
                AccountData::Exchange(ExchangeAccountData::UnmappedFill(f)) => format!("unmapped:{}:{}", f.instrument, f.size),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// 启动期确认判据：订阅回执与频道数据都算；连接计数等事件不算；订阅被拒启动失败
    #[test]
    fn subscribe_ack_or_channel_data_confirms_a_channel() {
        let ack = r#"{"event":"subscribe","arg":{"channel":"orders","instType":"SWAP"},"connId":"a"}"#;
        assert_eq!(subscription_confirmed(ack).unwrap().as_deref(), Some("orders"));
        assert_eq!(subscription_confirmed(&order_push("BTC-USDT-SWAP", "0")).unwrap().as_deref(), Some("orders"));
        let count = r#"{"event":"channel-conn-count","channel":"orders","connCount":"2","connId":"a"}"#;
        assert_eq!(subscription_confirmed(count).unwrap(), None);
        assert_eq!(subscription_confirmed("pong").unwrap(), None);
        let rejected = r#"{"event":"error","code":"60018","msg":"bad channel","connId":"a"}"#;
        assert!(subscription_confirmed(rejected).unwrap_err().to_string().contains("60018"));
    }

    /// 同名不同计价币 / 币本位的成交不串进 BTC：原样以 UnmappedFill 上报（张数不折算）
    #[test]
    fn other_quote_and_inverse_fills_do_not_leak_into_btc() {
        assert_eq!(kinds(&order_push("BTC-USDT-SWAP", "3")), ["fill:BTC:0.03", "order:BTC"]);
        assert_eq!(kinds(&order_push("BTC-USDC-SWAP", "3")), ["unmapped:BTC-USDC-SWAP:3"]);
        assert_eq!(kinds(&order_push("BTC-USD-SWAP", "3")), ["unmapped:BTC-USD-SWAP:3"]);
        // 无成交的订单状态：不关心
        assert!(kinds(&order_push("BTC-USDC-SWAP", "0")).is_empty());
    }

    /// 联网只读：真登录 OKX 私有流，启动（on_start 返回）即 account / orders 都已确认。
    /// 需要白名单 IP 与 OKX_API_KEY / OKX_SECRET / OKX_PASSPHRASE。
    #[tokio::test]
    #[ignore = "需要联网、OKX 凭证与白名单 IP"]
    async fn live_private_subscriptions_confirm_at_startup() {
        use kameo::actor::Spawn;
        use kameo_actors::DeliveryStrategy;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("缺 {k}"));
        let credentials =
            OkxCredentials { api_key: env("OKX_API_KEY"), secret: env("OKX_SECRET"), passphrase: env("OKX_PASSPHRASE") };
        let pubsub = AccountPubSub::spawn(AccountPubSub::new(DeliveryStrategy::BestEffort));
        let started = std::time::Instant::now();
        let actor = OkxPrivateWsActor::spawn(OkxPrivateWsActorArgs {
            credentials,
            account_pubsub: pubsub,
            symbol_metas: Arc::new(metas()),
            quote: "USDT".to_string(),
        });
        actor.wait_for_startup_result().await.expect("订阅应在启动期确认");
        println!("OKX 私有订阅确认耗时 {:?}", started.elapsed());
        actor.stop_gracefully().await.unwrap();
    }
}
