//! HyperliquidPrivateWsActor - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接
//! - 自动订阅账户频道 (webData3, orderUpdates)
//! - 直接解析消息并发布到对应总线
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址

use crate::domain::{now_ms, Balance, Exchange, ExchangeError};
use crate::messaging::AccountPubSub;
use crate::exchange::client::WsError;
use crate::exchange::hyperliquid::codec::{ClearinghouseState, WsOrderUpdate, WsUserFills};
use crate::exchange::hyperliquid::symbol::belongs_to_dex;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop;
use crate::messaging::{AccountData, AccountEvent};
use futures_util::StreamExt;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde_json::json;
use std::collections::HashSet;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// HyperliquidPrivateWsActor 初始化参数
pub struct HyperliquidPrivateWsActorArgs {
    /// 用户钱包地址 (0x...)
    pub wallet_address: String,
    /// 账户私有事件总线（发布事件，标 Live）
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// Perp DEX 名称 ("" = 默认 perp DEX)
    pub dex: String,
}

/// HyperliquidPrivateWsActor - 账户 WebSocket Actor
pub struct HyperliquidPrivateWsActor {
    /// 账户私有事件总线（发布事件，标 Live）
    account_pubsub: ActorRef<AccountPubSub>,
    /// 本 client 接入的 perp DEX：**账户级**推送（orderUpdates / userFills）不按 dex
    /// 下发，必须自己过滤（与 REST 侧 `belongs_to_dex` 同口径），否则同一钱包在其他
    /// dex 上的成交会被剥掉前缀后当成本 dex 同名标的的 Fill，直接污染「基线 + Fill」
    /// 维护的持仓。
    dex: String,
    /// 尚未确认生效的订阅。三条订阅（见 [`REQUIRED_SUBSCRIPTIONS`]）必须全部确认，
    /// 否则 [`SubscriptionDeadline`] 到期即自杀 —— **连接活着不等于订阅活着**：HL 的
    /// keepalive 只证明链路通，订阅被悄悄拒掉时 ping/pong 照常。
    ///
    /// 确认有两个等效来源（见 [`Self::confirm_subscription`]）：服务端的
    /// `subscriptionResponse` 回执，或该频道的数据到达。后者是更硬的证据，也让记账
    /// 不依赖"HL 一定对这三个订阅都回回执"这个未经实盘验证的假设。
    pending_subscriptions: HashSet<&'static str>,
    /// 发送消息到 ws_loop 的 channel
    #[allow(dead_code)]
    ws_tx: Option<mpsc::Sender<String>>,
}

/// 私有流必须全部确认的订阅（缺任何一条，本地账本都会从此落后于交易所）
const REQUIRED_SUBSCRIPTIONS: [&str; 3] = ["clearinghouseState", "orderUpdates", "userFills"];

/// 订阅确认的等待上限：超时未集齐即判定订阅失败并自杀（由 supervisor 决定重启与否）
const SUBSCRIPTION_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl HyperliquidPrivateWsActor {
    /// 解析并处理消息
    async fn handle_message(&mut self, raw: &str) -> Result<(), WsError> {
        let local_ts = now_ms();
        match parse_private_message(raw, &self.dex, local_ts)? {
            PrivateMessage::SubscriptionAck(sub_type) => {
                self.confirm_subscription(&sub_type);
            }
            PrivateMessage::Events { channel, events } => {
                // 数据到达即订阅生效 —— 不依赖"HL 一定会对这三个订阅都回确认回执"这个
                // 假设（`clearinghouseState` 不在 HL 公开文档的订阅主列表里，回执格式
                // 未经实盘验证）。两个判据取或：收到回执、或收到该频道的数据。
                self.confirm_subscription(channel);
                for event in events {
                    if let Err(e) = self.account_pubsub.tell(Publish(event)).send().await {
                        tracing::error!(error = %e, "Failed to publish to AccountPubSub");
                    }
                }
            }
        }
        Ok(())
    }

    /// 记下"这个订阅确实活着"。两个来源等效：服务端的确认回执，或该频道的数据到达。
    fn confirm_subscription(&mut self, subscription: &str) {
        if self.pending_subscriptions.remove(subscription) {
            tracing::info!(
                subscription,
                remaining = self.pending_subscriptions.len(),
                "Hyperliquid 私有订阅已确认生效"
            );
        }
    }
}

/// 订阅确认的截止检查（on_start 延时自投递一次）
pub struct SubscriptionDeadline;

impl Message<SubscriptionDeadline> for HyperliquidPrivateWsActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: SubscriptionDeadline,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self.pending_subscriptions.is_empty() {
            return;
        }
        // 只要有一条没确认，本 actor 提供的账本就是残缺的 —— 与其带病运行，不如
        // 立刻死掉让上层看见（与"预热失败即致命"同一姿态）。
        tracing::error!(
            missing = ?self.pending_subscriptions,
            timeout_s = SUBSCRIPTION_ACK_TIMEOUT.as_secs(),
            "Hyperliquid 私有订阅未在超时内全部确认，私有回报不可信，退出"
        );
        ctx.actor_ref().kill();
    }
}

impl Actor for HyperliquidPrivateWsActor {
    type Args = HyperliquidPrivateWsActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
        let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
            .await
            .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Hyperliquid, e.to_string()))?;

        let (write, read) = ws_stream.split();

        // 2. 创建出站消息 channel
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<String>(100);

        // 3. 创建入站消息 channel
        let (incoming_tx, incoming_rx) = mpsc::channel::<Result<String, WsError>>(100);

        // attach_stream 监控入站消息
        let incoming_stream = ReceiverStream::new(incoming_rx);
        actor_ref.attach_stream(incoming_stream, (), ());

        // 4. 启动 ws_loop
        tokio::spawn(ws_loop::run_ws_loop(
            read,
            write,
            outgoing_rx,
            incoming_tx,
            ws_loop::WsKeepalive::hyperliquid(),
        ));

        // 5. 订阅账户频道
        // clearinghouseState: perp 账户状态 (positions, equity, margin)
        let subscribe_clearinghouse = json!({
            "method": "subscribe",
            "subscription": {
                "type": "clearinghouseState",
                "user": args.wallet_address,
                "dex": args.dex
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_clearinghouse)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send clearinghouseState: {e}")))?;

        // orderUpdates: 订单更新
        let subscribe_orders = json!({
            "method": "subscribe",
            "subscription": {
                "type": "orderUpdates",
                "user": args.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_orders)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send orderUpdates: {e}")))?;

        // userFills: 成交推送
        let subscribe_fills = json!({
            "method": "subscribe",
            "subscription": {
                "type": "userFills",
                "user": args.wallet_address
            }
        })
        .to_string();

        outgoing_tx
            .send(subscribe_fills)
            .await
            .map_err(|e| ExchangeError::WebSocketError(format!("Hyperliquid send userFills: {e}")))?;

        // 6. 订阅确认的截止检查：请求发出去 ≠ 订阅生效（见 pending_subscriptions）
        let deadline_ref = actor_ref.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SUBSCRIPTION_ACK_TIMEOUT).await;
            // actor 已停机时投递失败是预期（本检查已无意义）
            let _ = deadline_ref.tell(SubscriptionDeadline).send().await;
        });

        tracing::info!(
            wallet = %args.wallet_address,
            "HyperliquidPrivateWsActor started, subscribed to clearinghouseState, orderUpdates and userFills"
        );

        Ok(Self {
            account_pubsub: args.account_pubsub,
            dex: args.dex,
            pending_subscriptions: REQUIRED_SUBSCRIPTIONS.into_iter().collect(),
            ws_tx: Some(outgoing_tx),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.ws_tx.take();
        tracing::info!("HyperliquidPrivateWsActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// WebSocket 入站消息处理
impl Message<StreamMessage<Result<String, WsError>, (), ()>> for HyperliquidPrivateWsActor {
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

/// 私有流一条报文对本 actor 的语义。
///
/// 解析保持纯函数，订阅记账（需要跨报文的状态）留给 actor —— 见
/// [`HyperliquidPrivateWsActor::pending_subscriptions`]。
enum PrivateMessage {
    /// 携带零到多条事件。`channel` 供订阅记账使用 —— **该频道有数据到达，就是订阅
    /// 生效的最硬证据**（比确认回执更直接）。pong / 未知频道给空串。
    Events {
        channel: &'static str,
        events: Vec<AccountEvent>,
    },
    /// 某个订阅的确认回执，值为订阅类型（如 `orderUpdates`）
    SubscriptionAck(String),
}

fn parse_private_message(raw: &str, dex: &str, local_ts: u64) -> Result<PrivateMessage, WsError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            // 服务端拒绝/报错：**必须致命**。此前它落进下面的未知分支被 debug 掉，
            // 于是"订阅被拒（地址格式、dex 参数、限流）"表现为：连接健康、ping/pong
            // 正常、而订单与成交回报再也不来 —— 系统全绿地瞎跑。
            "error" => {
                return Err(WsError::ParseError(format!(
                    "Hyperliquid 私有流返回错误: {}",
                    value.get("data").unwrap_or(&value)
                )));
            }
            "subscriptionResponse" => {
                // {"channel":"subscriptionResponse","data":{"method":"subscribe",
                //   "subscription":{"type":"orderUpdates","user":"0x..."}}}
                let sub_type = value
                    .pointer("/data/subscription/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                return Ok(PrivateMessage::SubscriptionAck(sub_type.to_string()));
            }
            // 应用层心跳应答（HL 文档口径：{"channel":"pong"}）
            "pong" => {
                return Ok(PrivateMessage::Events { channel: "", events: Vec::new() });
            }
            "clearinghouseState" => {
                // perp 账户状态：只取 equity / notional / withdrawable，持仓不入事件流
                let data = &value["data"];
                return parse_clearinghouse_state(data, local_ts).map(|events| {
                    PrivateMessage::Events { channel: "clearinghouseState", events }
                });
            }
            "orderUpdates" => {
                // 订单更新
                let data = &value["data"];
                return parse_order_updates(data, dex, local_ts)
                    .map(|events| PrivateMessage::Events { channel: "orderUpdates", events });
            }
            "userFills" => {
                // 成交推送
                let data = &value["data"];
                return parse_user_fills(data, dex, local_ts)
                    .map(|events| PrivateMessage::Events { channel: "userFills", events });
            }
            _ => {
                tracing::debug!(channel, "Unknown Hyperliquid private channel");
                return Ok(PrivateMessage::Events { channel: "", events: Vec::new() });
            }
        }
    }

    // pong 消息
    if value.get("method").map(|v| v.as_str()) == Some(Some("pong")) {
        return Ok(PrivateMessage::Events { channel: "", events: Vec::new() });
    }

    // 其他未知消息
    tracing::debug!(raw, "Unhandled Hyperliquid private message");
    Ok(PrivateMessage::Events { channel: "", events: Vec::new() })
}

/// 解析 clearinghouseState 消息 (perp 账户状态)
///
/// **只取账户级读数（equity / notional / withdrawable），不产出持仓事件。**
/// 本推送里的 `assetPositions` 是持仓**快照**，而持仓的维护模型是「启动期 REST 基线 +
/// 之后全程 Fill 累加」（见 [`crate::messaging::PositionBaseline`]）：快照既当不了基线
/// （基线只能来自 ManagerActor），也不能参与增量（会与 Fill 重复计算）。要校验本地持仓
/// 是否漂移，走 `PositionLedgerActor` 那条独立轮询通道。
///
/// 之所以不能像 OKX 那样干脆退订整个频道：equity / notional / withdrawable 都只有这里有。
fn parse_clearinghouse_state(
    data: &serde_json::Value,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, WsError> {
    let mut events = Vec::new();

    // clearinghouseState 订阅返回结构: { clearinghouseState: {...}, user: "...", dex: "..." }
    // 需要提取 clearinghouseState 字段
    let state_value = data.get("clearinghouseState").unwrap_or(data);
    let state: ClearinghouseState = serde_json::from_value(state_value.clone())
        .map_err(|e| WsError::ParseError(format!("clearinghouseState parse: {}", e)))?;

    tracing::debug!(
        positions_count = state.asset_positions.len(),
        coins = ?state.asset_positions.iter().map(|w| &w.position.coin).collect::<Vec<_>>(),
        "Hyperliquid clearinghouseState received (持仓部分不入事件流，见函数文档)"
    );

    // 解析账户信息 (equity + notional)
    let equity = f64::from_str(&state.margin_summary.account_value)
        .map_err(|_| WsError::ParseError(format!("Failed to parse Hyperliquid accountValue: {}", state.margin_summary.account_value)))?;
    let notional = f64::from_str(&state.margin_summary.total_ntl_pos)
        .map_err(|_| WsError::ParseError(format!("Failed to parse Hyperliquid total_ntl_pos: {}", state.margin_summary.total_ntl_pos)))?;
    events.push(AccountEvent {
        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
        account: crate::domain::AccountId::Live,
        exchange_ts: local_ts,
        local_ts,
        data: AccountData::AccountInfo {
            exchange: Exchange::Hyperliquid,
            equity,
            notional,
        },
    });

    // 解析可用余额
    let withdrawable = f64::from_str(&state.withdrawable)
        .map_err(|_| WsError::ParseError(format!("invalid withdrawable: {}", state.withdrawable)))?;
    events.push(AccountEvent {
        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
        account: crate::domain::AccountId::Live,
        exchange_ts: local_ts,
        local_ts,
        data: AccountData::Balance(Balance {
            exchange: Exchange::Hyperliquid,
            asset: "USDC".to_string(),
            available: withdrawable,
        }),
    });

    Ok(events)
}

/// 解析 orderUpdates 消息。
///
/// **按 dex 过滤在剥前缀之前**：orderUpdates 是账户级推送（一个钱包在所有 perp dex 上
/// 的订单都从这里下发），与 REST 侧同口径（[`belongs_to_dex`]）。不过滤的话，xyz dex
/// 的 "xyz:AAPL" 会被剥成 "AAPL"，与默认 dex 的同名标的混在一起。
fn parse_order_updates(
    data: &serde_json::Value,
    dex: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, WsError> {
    let mut events = Vec::new();

    // orderUpdates 必须是一个数组
    let updates = data.as_array()
        .ok_or_else(|| WsError::ParseError(format!("orderUpdates is not an array: {}", data)))?;

    for update in updates {
        let order_update: WsOrderUpdate = serde_json::from_value(update.clone())
            .map_err(|e| WsError::ParseError(format!("orderUpdate parse: {}", e)))?;
        if !belongs_to_dex(&order_update.order.coin, dex) {
            tracing::debug!(
                coin = %order_update.order.coin,
                dex,
                "orderUpdate 属于其他 perp dex，跳过"
            );
            continue;
        }
        let update = order_update.to_order_update()?;
        events.push(AccountEvent {
            // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
            account: crate::domain::AccountId::Live,
            // 交易所时点取推送自带的 statusTimestamp（OrderUpdate 本身不再捎带时间戳 ——
            // 四所口径曾混装且无人读，见 crate::domain::OrderUpdate 的文档）
            exchange_ts: order_update.status_timestamp,
            local_ts,
            data: AccountData::OrderUpdate(update),
        });
    }

    Ok(events)
}

/// 解析 userFills 消息。
///
/// 与 [`parse_order_updates`] 同理，**必须按 dex 过滤**：userFills 是账户级推送，
/// 其他 dex 的成交混进来会直接污染「基线 + Fill」维护的持仓 —— 这正是持仓对账通道
/// 要抓的那类错账，不该由适配层制造。
fn parse_user_fills(
    data: &serde_json::Value,
    dex: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, WsError> {
    let user_fills: WsUserFills = serde_json::from_value(data.clone())
        .map_err(|e| WsError::ParseError(format!("userFills parse: {}", e)))?;

    // 忽略 snapshot（初始快照），只处理增量更新
    if user_fills.is_snapshot {
        tracing::debug!(
            user = %user_fills.user,
            fills_count = user_fills.fills.len(),
            "Received userFills snapshot, ignoring"
        );
        return Ok(Vec::new());
    }

    let mut events = Vec::new();
    for ws_fill in &user_fills.fills {
        if !belongs_to_dex(&ws_fill.coin, dex) {
            tracing::debug!(coin = %ws_fill.coin, dex, "fill 属于其他 perp dex，跳过");
            continue;
        }
        let fill = ws_fill.to_fill()
            ?;
        events.push(AccountEvent {
            // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
            account: crate::domain::AccountId::Live,
            exchange_ts: fill.timestamp,
            local_ts,
            data: AccountData::Fill(fill),
        });
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fill_json(coin: &str) -> serde_json::Value {
        json!({
            "coin": coin,
            "px": "100.0",
            "sz": "1.0",
            "side": "B",
            "time": 1u64,
            "oid": 1u64,
            "fee": "0.01",
            "closedPnl": "0.0"
        })
    }

    fn order_update_json(coin: &str) -> serde_json::Value {
        json!({
            "order": {
                "coin": coin,
                "side": "B",
                "limitPx": "100.0",
                "sz": "1.0",
                "oid": 1u64,
                "timestamp": 1u64,
                "origSz": "1.0"
            },
            "status": "open",
            "statusTimestamp": 1u64
        })
    }

    /// **错账防线**：userFills 是账户级推送，其他 perp dex 的成交必须被过滤 ——
    /// 否则 "xyz:AAPL" 会被剥前缀成 "AAPL"，当成本 dex 同名标的的 Fill 累进持仓。
    #[test]
    fn fills_from_other_dexes_are_filtered_out() {
        let data = json!({
            "user": "0xabc",
            "fills": [fill_json("ETH"), fill_json("xyz:AAPL")]
        });
        // 默认 dex 只收裸 coin
        let events = parse_user_fills(&data, "", 1).unwrap();
        assert_eq!(events.len(), 1);
        // 具名 dex 只收自己前缀的，裸 coin（默认 dex 的同名标的）也不能收
        let events = parse_user_fills(&data, "xyz", 1).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].data {
            AccountData::Fill(f) => assert_eq!(f.symbol, "AAPL"),
            other => panic!("expected fill, got {other:?}"),
        }
    }

    /// **Critical 回归防线**：服务端的错误报文必须致命。
    ///
    /// HL 的下发格式是 `{"channel":"error","data":"..."}`；此前代码检查的是顶层
    /// `error` 键与 `subscriptionResponse.data.error`（都不是 HL 的真实格式，死代码），
    /// 真实错误落进未知分支被 debug 掉 —— 订阅被拒而 ping/pong 照常，私有回报再也
    /// 不来却系统全绿。
    #[test]
    fn server_error_message_is_fatal() {
        let raw = r#"{"channel":"error","data":"Invalid subscription {\"type\":\"userFills\"}"}"#;
        let err = parse_private_message(raw, "", 1)
            .err()
            .expect("服务端错误报文被吞掉了 —— 订阅被拒会表现为静默断流");
        assert!(
            err.to_string().contains("Invalid subscription"),
            "错误详情应保留以便定位: {err}"
        );
    }

    /// 订阅确认回执带回订阅类型，供 actor 记账（凑齐三条才算订阅生效）
    #[test]
    fn subscription_response_is_reported_as_ack() {
        let raw = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe",
            "subscription":{"type":"orderUpdates","user":"0xabc"}}}"#;
        match parse_private_message(raw, "", 1).unwrap() {
            PrivateMessage::SubscriptionAck(t) => assert_eq!(t, "orderUpdates"),
            _ => panic!("订阅确认必须能被识别，否则记账永远凑不齐、启动 15 秒后自杀"),
        }
    }

    /// 数据到达同样算订阅生效：不依赖回执格式的假设（clearinghouseState 的回执未经
    /// 实盘验证；若它不回回执而只推数据，仅靠回执记账会在 15 秒后误杀）
    #[test]
    fn incoming_data_also_confirms_the_subscription() {
        let raw = r#"{"channel":"userFills","data":{"user":"0xabc","fills":[]}}"#;
        match parse_private_message(raw, "", 1).unwrap() {
            PrivateMessage::Events { channel, .. } => assert_eq!(
                channel, "userFills",
                "数据报文必须带回频道名，否则无法用它推进订阅记账"
            ),
            _ => panic!("userFills 数据应解析为 Events"),
        }
    }

    /// 三条必需订阅缺一不可 —— 常量与记账初值是同一出处
    #[test]
    fn all_three_private_subscriptions_are_required() {
        let pending: HashSet<&'static str> = REQUIRED_SUBSCRIPTIONS.into_iter().collect();
        assert_eq!(pending.len(), 3);
        for sub in ["clearinghouseState", "orderUpdates", "userFills"] {
            assert!(pending.contains(sub), "{sub} 未纳入订阅确认记账");
        }
    }

    /// orderUpdates 同口径过滤
    #[test]
    fn order_updates_from_other_dexes_are_filtered_out() {
        let data = json!([order_update_json("ETH"), order_update_json("xyz:AAPL")]);
        let events = parse_order_updates(&data, "", 1).unwrap();
        assert_eq!(events.len(), 1);
        let events = parse_order_updates(&data, "xyz", 1).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].data {
            AccountData::OrderUpdate(u) => assert_eq!(u.symbol, "AAPL"),
            other => panic!("expected order update, got {other:?}"),
        }
    }
}
