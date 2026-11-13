//! HyperliquidPrivateWs - 管理 Hyperliquid 账户 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接（读循环是子任务）
//! - 自动订阅账户频道 (orderUpdates, userFills)
//! - 直接解析消息并发布到账户总线
//!
//! 注意: Hyperliquid 的账户订阅不需要认证，只需要用户地址
//!
//! 不订阅 clearinghouseState：它按单个 dex 推送，统一账户下其中的净值恒为 0（余额在 spot
//! clearinghouse），多 dex 时也拼不出账户总值。净值与持仓总价值改由 REST 轮询
//! （`AccountClient::fetch_account_info`，见 `crate::exchange::equity_polling`）。

use crate::domain::{now_ms, Exchange, ExchangeError};
use crate::exchange::hyperliquid::codec::{WsOrderUpdate, WsUserFills};
use crate::exchange::hyperliquid::markets::HlMarkets;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{AccountBus, AccountData, AccountEvent, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, Scope, Task};
use anyhow::Context as _;
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;

/// 账户 WebSocket 组件（没有收件箱：只读不收命令）
///
/// 构造完成（`spawn` 返回）前两条订阅（见 [`REQUIRED_SUBSCRIPTIONS`]）都已确认生效（见
/// [`WsConn::await_subscriptions`]）—— **连接活着不等于订阅活着**：HL 的 keepalive 只证明
/// 链路通，订阅被悄悄拒掉时 ping/pong 照常。
pub(super) struct HyperliquidPrivateWs {
    /// 账户私有事件总线（发布事件，标 Live）
    account_bus: AccountBus,
    /// 品种目录：**账户级**推送（orderUpdates / userFills）下发该钱包在所有 dex 上的记录，
    /// 只收目录里的 coin（与 REST 侧持仓基线、对账读数同一判据 [`HlMarkets::symbol_of`]），
    /// 否则别的 dex 上的成交会被剥掉前缀后当成同名标的的 Fill，直接污染「基线 + Fill」
    /// 维护的持仓。
    markets: Arc<HlMarkets>,
    /// 计价币：成交手续费必须以它计
    quote: String,
    conn: WsConn,
}

/// 私有流必须全部确认的订阅（缺任何一条，本地账本都会从此落后于交易所）
const REQUIRED_SUBSCRIPTIONS: [&str; 2] = ["orderUpdates", "userFills"];

impl HyperliquidPrivateWs {
    /// 在 `group` 下起私有 WS，等两条订阅都确认
    pub(super) async fn spawn(
        group: &Group,
        wallet_address: String,
        account_bus: AccountBus,
        markets: Arc<HlMarkets>,
        quote: String,
    ) -> anyhow::Result<ChildHandle> {
        group
            .spawn("HyperliquidPrivateWs", move |scope: Scope| async move {
                // 1. 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
                let ws_stream = crate::exchange::ws_loop::connect(WS_URL)
                    .await
                    .map_err(|e| ExchangeError::transport(Exchange::Hyperliquid, e.to_string()))?;
                let (write, read) = ws_stream.split();

                // 2. 起读循环（心跳、回 Pong、断线判定都归它）
                let mut conn = WsConn::spawn(
                    &scope.group("conn"),
                    "HyperliquidPrivateWsLoop",
                    read,
                    write,
                    ws_loop::WsKeepalive::hyperliquid(),
                )?;

                // 3. 订阅账户频道：orderUpdates（订单更新）、userFills（成交）
                for sub_type in REQUIRED_SUBSCRIPTIONS {
                    let msg = json!({
                        "method": "subscribe",
                        "subscription": {"type": sub_type, "user": wallet_address}
                    })
                    .to_string();
                    conn.send(msg).await.map_err(|e| {
                        ExchangeError::new(format!("Hyperliquid send {sub_type}: {e}"))
                    })?;
                }

                // 4. 等两条订阅都确认才算启动完成；期间收到的帧之后由 recv 先交出
                conn.await_subscriptions(
                    Exchange::Hyperliquid,
                    &REQUIRED_SUBSCRIPTIONS,
                    subscription_confirmed,
                )
                .await?;

                tracing::info!(
                    wallet = %wallet_address,
                    dexes = ?markets.dexes(),
                    "HyperliquidPrivateWs started, orderUpdates and userFills confirmed"
                );
                Ok(Self {
                    account_bus,
                    markets,
                    quote,
                    conn,
                })
            })
            .await
    }

    /// 解析并发布一帧
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_private_message(raw, &self.markets, &self.quote, now_ms())
            .with_context(|| format!("Hyperliquid 私有 WS 解析失败: {raw}"))?;
        for event in events {
            self.account_bus.publish(event);
        }
        Ok(())
    }
}

impl Task for HyperliquidPrivateWs {
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        // 停机时已到手的回报（可能有成交）先处理完再走，见 WsConn::next
        while let Some(raw) = self.conn.next(scope).await? {
            self.handle_message(&raw)?;
        }
        Ok(())
    }
}

// ============================================================================
// 消息解析
// ============================================================================

/// 私有流一条报文的协议层含义（不涉及品种目录）。启动期的订阅确认与运行期的事件解析
/// 都从这里分流，HL 私有流报文格式只在这一处。
#[derive(Debug, PartialEq)]
enum Frame {
    /// 某个订阅的确认回执，值为订阅类型（如 `orderUpdates`）
    SubscriptionAck(String),
    /// 订阅频道的数据。**该频道有数据到达，就是订阅生效的最硬证据**（比确认回执更直接）
    Data {
        channel: &'static str,
        data: serde_json::Value,
    },
    /// pong、未知频道等与本组件无关的报文
    Ignored,
}

fn classify(raw: &str) -> Result<Frame, ExchangeError> {
    let mut value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::new(e.to_string()))?;
    let channel = value
        .get("channel")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    match channel.as_str() {
        // 服务端拒绝/报错：**必须致命**。此前它落进未知分支被 debug 掉，于是"订阅被拒
        // （地址格式、限流）"表现为：连接健康、ping/pong 正常、而订单与成交回报再也不来。
        "error" => Err(ExchangeError::new(format!(
            "Hyperliquid 私有流返回错误: {}",
            value.get("data").unwrap_or(&value)
        ))),
        // {"channel":"subscriptionResponse","data":{"method":"subscribe",
        //   "subscription":{"type":"orderUpdates","user":"0x..."}}}
        "subscriptionResponse" => Ok(Frame::SubscriptionAck(
            value
                .pointer("/data/subscription/type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        )),
        "orderUpdates" => Ok(Frame::Data {
            channel: "orderUpdates",
            data: value["data"].take(),
        }),
        "userFills" => Ok(Frame::Data {
            channel: "userFills",
            data: value["data"].take(),
        }),
        // 应用层心跳应答（{"channel":"pong"}）
        "pong" => Ok(Frame::Ignored),
        "" => {
            tracing::debug!(raw, "Unhandled Hyperliquid private message");
            Ok(Frame::Ignored)
        }
        other => {
            tracing::debug!(channel = other, "Unknown Hyperliquid private channel");
            Ok(Frame::Ignored)
        }
    }
}

/// 启动期订阅确认判据（见 [`WsConn::await_subscriptions`]）：回执或数据到达都算
fn subscription_confirmed(raw: &str) -> Result<Option<String>, ExchangeError> {
    Ok(match classify(raw)? {
        Frame::SubscriptionAck(sub) => Some(sub),
        Frame::Data { channel, .. } => Some(channel.to_string()),
        Frame::Ignored => None,
    })
}

fn parse_private_message(
    raw: &str,
    markets: &HlMarkets,
    quote: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    match classify(raw)? {
        Frame::Data {
            channel: "orderUpdates",
            data,
        } => parse_order_updates(&data, markets, local_ts),
        Frame::Data {
            channel: "userFills",
            data,
        } => parse_user_fills(&data, markets, quote, local_ts),
        Frame::Data { .. } | Frame::SubscriptionAck(_) | Frame::Ignored => Ok(Vec::new()),
    }
}

/// 解析 orderUpdates 消息。
///
/// orderUpdates 是账户级推送（一个钱包在所有 perp dex 上的订单都从这里下发），只收品种
/// 目录里的 coin（[`HlMarkets::symbol_of`] 一步判断并换成 symbol）：单剥前缀的话，
/// "xyz:AAPL" 会被剥成 "AAPL"，与主站的同名标的混在一起。
fn parse_order_updates(
    data: &serde_json::Value,
    markets: &HlMarkets,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    let mut events = Vec::new();

    // orderUpdates 必须是一个数组
    let updates = data
        .as_array()
        .ok_or_else(|| ExchangeError::new(format!("orderUpdates is not an array: {}", data)))?;

    for update in updates {
        let order_update: WsOrderUpdate = serde_json::from_value(update.clone())
            .map_err(|e| ExchangeError::new(format!("orderUpdate parse: {}", e)))?;
        let Some(symbol) = markets.symbol_of(&order_update.order.coin) else {
            // 目录外的订单不理（#760）：挂单本身不改变持仓，它的成交 userFills 那边同样丢弃
            tracing::debug!(coin = %order_update.order.coin, "HL 目录外订单，跳过");
            continue;
        };
        let update = order_update.to_order_update(symbol)?;
        events.push(AccountEvent {
            // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
            account: crate::domain::AccountId::Live,
            // 交易所时点取推送自带的 statusTimestamp（OrderUpdate 本身不再捎带时间戳 ——
            // 四所口径曾混装且无人读，见 crate::domain::OrderUpdate 的文档）
            exchange_ts: order_update.status_timestamp,
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::OrderUpdate(update)),
        });
    }

    Ok(events)
}

/// 解析 userFills 消息。
///
/// userFills 是账户级推送：已接入 dex 上目录内品种的成交进 Fill 流；**其余成交**（未接入的
/// dex、不可交易品种、现货）丢弃（#760：目录外成交不理），不进任何 symbol 的持仓 ——
/// 剥掉前缀后会被当成同名品种的 Fill，污染「基线 + Fill」维护的持仓。
/// 判据与持仓基线、对账读数相同（[`HlMarkets::symbol_of`]），两条通道口径一致。
/// 初始快照（`isSnapshot`）是历史成交，不入账：现存持仓由持仓查询给出。
fn parse_user_fills(
    data: &serde_json::Value,
    markets: &HlMarkets,
    quote: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    let user_fills: WsUserFills = serde_json::from_value(data.clone())
        .map_err(|e| ExchangeError::new(format!("userFills parse: {}", e)))?;

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
        let Some(symbol) = markets.symbol_of(&ws_fill.coin) else {
            tracing::debug!(coin = %ws_fill.coin, "HL 目录外成交，丢弃");
            continue;
        };
        events.push(AccountEvent {
            // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
            account: crate::domain::AccountId::Live,
            exchange_ts: ws_fill.time,
            local_ts,
            data: AccountData::Symbol(SymbolAccountData::Fill(ws_fill.to_fill(symbol, quote)?)),
        });
    }

    Ok(events)
}

/// 测试入口：一条 userFills 推送 → 进账本的事件（见 `hyperliquid::testkit`）
#[cfg(test)]
pub(crate) fn parse_user_fills_for_test(
    markets: &HlMarkets,
    raw: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    parse_private_message(raw, markets, "USDC", local_ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::hyperliquid::testkit;
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
            "feeToken": "USDC"
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

    /// **错账防线**：userFills 是账户级推送。已接入的每个 dex 的成交都进 Fill 流；
    /// 未接入 dex、目录外品种、现货的成交丢弃 —— 绝不剥前缀当成同名标的的 Fill
    /// （"flx:TSLA" 不能累进 xyz 的 TSLA 持仓）。
    #[test]
    fn fills_of_connected_dexes_flow_and_others_are_dropped() {
        let kinds = |m: &HlMarkets, fills: Vec<serde_json::Value>| -> Vec<String> {
            let data = json!({"user": "0xabc", "fills": fills});
            parse_user_fills(&data, m, "USDC", 1)
                .unwrap()
                .into_iter()
                .map(|e| match e.data {
                    AccountData::Symbol(SymbolAccountData::Fill(f)) => format!("fill:{}", f.symbol),
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        };
        let both = testkit::markets(&["", "xyz"]);
        assert_eq!(
            kinds(&both, vec![fill_json("BTC"), fill_json("xyz:TSLA")]),
            ["fill:BTC", "fill:TSLA"]
        );
        assert_eq!(
            kinds(&both, vec![fill_json("BTC"), fill_json("flx:TSLA")]),
            ["fill:BTC"]
        );
        assert!(kinds(&both, vec![fill_json("ETH")]).is_empty());
        assert!(kinds(&testkit::markets(&[""]), vec![fill_json("xyz:TSLA")]).is_empty());
        assert!(kinds(&both, vec![fill_json("@107"), fill_json("PURR/USDC")]).is_empty());
        assert!(kinds(&testkit::markets(&["xyz"]), vec![fill_json("BTC")]).is_empty());
    }

    /// 手续费不是计价币（USDC）的成交：手续费口径对不上，报错而不是当 USDC 记
    #[test]
    fn fill_fee_in_other_token_is_an_error() {
        let mut f = fill_json("BTC");
        f["feeToken"] = json!("HYPE");
        let data = json!({"user": "0xabc", "fills": [f]});
        assert!(parse_user_fills(&data, &testkit::markets(&[""]), "USDC", 1).is_err());
    }

    /// 初始快照是历史成交：不入账、不上报
    #[test]
    fn snapshot_fills_are_ignored() {
        let m = testkit::markets(&[""]);
        let data = json!({"user": "0xabc", "isSnapshot": true, "fills": [fill_json("BTC"), fill_json("flx:TSLA")]});
        assert!(parse_user_fills(&data, &m, "USDC", 1).unwrap().is_empty());
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
        let err = parse_private_message(raw, &testkit::markets(&[""]), "USDC", 1)
            .expect_err("服务端错误报文被吞掉了 —— 订阅被拒会表现为静默断流");
        assert!(
            err.to_string().contains("Invalid subscription"),
            "错误详情应保留以便定位: {err}"
        );
    }

    /// 启动期确认判据：回执与该频道数据都算确认（不依赖"HL 一定对每个订阅都回回执"）；
    /// pong 等无关报文不算
    #[test]
    fn ack_or_channel_data_confirms_a_subscription() {
        let ack = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe",
            "subscription":{"type":"orderUpdates","user":"0xabc"}}}"#;
        assert_eq!(
            subscription_confirmed(ack).unwrap().as_deref(),
            Some("orderUpdates")
        );
        let data = r#"{"channel":"userFills","data":{"user":"0xabc","fills":[]}}"#;
        assert_eq!(
            subscription_confirmed(data).unwrap().as_deref(),
            Some("userFills")
        );
        assert_eq!(
            subscription_confirmed(r#"{"channel":"pong"}"#).unwrap(),
            None
        );
        let rejected = r#"{"channel":"error","data":"Invalid subscription"}"#;
        assert!(
            subscription_confirmed(rejected).is_err(),
            "启动期被拒必须启动失败"
        );
    }

    /// clearinghouseState 不再订阅：即使推来也不产出任何事件（统一账户下其中净值恒为 0，
    /// 产出 AccountInfo 会和 REST 轮询的真实净值互相覆盖）
    #[test]
    fn clearinghouse_state_push_yields_no_account_info() {
        let raw = format!(
            r#"{{"channel":"clearinghouseState","data":{{"user":"0xabc","dex":"","clearinghouseState":{}}}}}"#,
            testkit::OWN_EMPTY_STATE
        );
        assert!(
            parse_private_message(&raw, &testkit::markets(&[""]), "USDC", 1)
                .unwrap()
                .is_empty()
        );
    }

    /// orderUpdates 同口径过滤
    #[test]
    fn order_updates_are_kept_for_connected_dexes_only() {
        let data = json!([
            order_update_json("BTC"),
            order_update_json("xyz:NVDA"),
            order_update_json("flx:NVDA")
        ]);
        let events = parse_order_updates(&data, &testkit::markets(&["", "xyz"]), 1).unwrap();
        let symbols: Vec<_> = events
            .iter()
            .map(|e| match &e.data {
                AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => u.symbol.clone(),
                other => panic!("expected order update, got {other:?}"),
            })
            .collect();
        assert_eq!(symbols, ["BTC", "NVDA"]);
        assert_eq!(
            parse_order_updates(&data, &testkit::markets(&["xyz"]), 1)
                .unwrap()
                .len(),
            1
        );
    }

    /// 联网只读：真连 HL，启动（spawn 返回）即两条订阅都已确认。
    /// `HL_WALLET=0x… cargo test --lib live_private_subscriptions_confirm_at_startup -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "需要联网；HL_WALLET 可选（缺省用零地址，订阅照样确认）"]
    async fn live_private_subscriptions_confirm_at_startup() {
        let wallet = std::env::var("HL_WALLET").unwrap_or_else(|_| format!("0x{}", "0".repeat(40)));
        let root = crate::tasks::Root::new("t");
        let started = std::time::Instant::now();
        HyperliquidPrivateWs::spawn(
            &root.group("g"),
            wallet,
            AccountBus::new("account"),
            Arc::new(testkit::markets(&[""])),
            "USDC".to_string(),
        )
        .await
        .expect("订阅应在启动期确认");
        println!("HL 私有订阅确认耗时 {:?}", started.elapsed());
        root.shutdown().await.unwrap();
    }
}
