//! OkxPrivateWs - 管理 OKX 私有 WebSocket 连接
//!
//! 职责:
//! - 建立私有 WebSocket 连接并完成登录
//! - 自动订阅私有频道 (account, orders)
//! - 直接解析消息并发布到账户总线

use crate::domain::{now_ms, Balance, Exchange, ExchangeError, OrderStatus, Symbol, SymbolMeta};
use crate::exchange::okx::codec::{
    resolve_meta, AccountData as OkxAccountPush, OrderPushData, WsEvent, WsPush,
};
use crate::exchange::okx::{OkxCredentials, WS_PRIVATE_URL};
use crate::exchange::ws_loop::WsConn;
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, SymbolAccountData,
};
use crate::tasks::{ChildHandle, Group, Scope, Task};
use anyhow::Context as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 登录响应等待超时 (秒)。超时 → 启动失败受控退出，避免构造永久阻塞。
const LOGIN_TIMEOUT_SECS: u64 = 10;

/// 私有流必须全部确认的频道：account（余额）、orders（订单与成交）
const REQUIRED_CHANNELS: [&str; 2] = ["account", "orders"];

/// 私有 WebSocket 组件（没有收件箱：只读不收命令）
///
/// 构造完成（`spawn` 返回）前已登录、且 [`REQUIRED_CHANNELS`] 都已确认订阅生效。
pub(super) struct OkxPrivateWs {
    /// 账户私有事件总线（发布事件，标 Live）
    account_bus: AccountBus,
    /// Symbol 元数据（用于仓位转换）
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币：只有该计价币的永续映射到内部 symbol（见 `okx::symbol`）
    quote: String,
    conn: WsConn,
    /// 成交去重（见 [`LastTrades`]）
    last_trades: LastTrades,
}

impl OkxPrivateWs {
    /// 在 `group` 下起私有 WS，等登录成功、私有频道全部确认
    pub(super) async fn spawn(
        group: &Group,
        credentials: OkxCredentials,
        account_bus: AccountBus,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        quote: String,
    ) -> anyhow::Result<ChildHandle> {
        group
            .spawn("OkxPrivateWs", move |scope: Scope| async move {
                // 1. 连接私有 WebSocket（失败向上传播 → 启动期受控退出）
                let ws_stream = crate::exchange::ws_loop::connect(WS_PRIVATE_URL)
                    .await
                    .map_err(|e| ExchangeError::transport(Exchange::OKX, e.to_string()))?;
                let (mut write, mut read) = ws_stream.split();

                // 2. 登录（带超时；任何失败向上传播 → 启动期受控退出，不重试）
                login(&credentials, &mut write, &mut read).await?;

                // 3. 订阅私有频道
                //
                // **不订阅 `positions`**：持仓的维护模型是「启动期 REST 基线 + 之后全程 Fill 累加」，
                // 持仓推送既不是基线（基线只能来自 Manager）也不参与增量（增量走 `orders` 频道
                // 产出的 Fill）。要校验本地持仓是否漂移，走 PositionLedger 那条独立轮询通道
                // （见 crate::engine 的 PositionLedger），不在这里塞快照。
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
                    .map_err(|e| ExchangeError::new(format!("OKX send private subscribe: {e}")))?;

                // 4. 起读循环（心跳、回 Pong、断线判定都归它）
                let mut conn = WsConn::spawn(
                    &scope.group("conn"),
                    "OkxPrivateWsLoop",
                    read,
                    write,
                    crate::exchange::okx::KEEPALIVE,
                )?;

                // 5. 等两个频道都确认才算启动完成；期间收到的帧之后由 recv 先交出
                conn.await_subscriptions(Exchange::OKX, &REQUIRED_CHANNELS, subscription_confirmed)
                    .await?;

                tracing::info!("OkxPrivateWs started");
                Ok(Self {
                    account_bus,
                    symbol_metas,
                    quote,
                    conn,
                    last_trades: LastTrades::default(),
                })
            })
            .await
    }

    /// 解析并发布一帧
    fn handle_message(&mut self, raw: &str) -> anyhow::Result<()> {
        tracing::debug!(raw, "OKX private message received");
        let events = parse_private_message(
            raw,
            now_ms(),
            &self.quote,
            &self.symbol_metas,
            &mut self.last_trades,
        )
        .with_context(|| format!("OKX 私有 WS 解析失败: {raw}"))?;
        tracing::debug!(count = events.len(), "OKX private events parsed");
        for event in events {
            self.account_bus.publish(event);
        }
        Ok(())
    }
}

impl Task for OkxPrivateWs {
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        // 停机时已到手的回报（可能有成交）先处理完再走，见 WsConn::next
        while let Some(raw) = self.conn.next(scope).await? {
            self.handle_message(&raw)?;
        }
        Ok(())
    }
}

/// 发 login 并等登录响应（带超时）。读循环尚未起，期间服务端 Ping 由这里回 Pong。
async fn login(
    credentials: &OkxCredentials,
    write: &mut (impl SinkExt<WsMessage, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    read: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin),
) -> Result<(), ExchangeError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ExchangeError::new(format!("系统时钟早于 UNIX 纪元: {e}")))?
        .as_secs()
        .to_string();
    let sign = credentials.sign_ws_login(&timestamp);

    let login_msg = json!({
        "op": "login",
        "args": [{
            "apiKey": credentials.api_key,
            "passphrase": credentials.passphrase,
            "timestamp": timestamp,
            "sign": sign
        }]
    })
    .to_string();

    write
        .send(WsMessage::Text(login_msg))
        .await
        .map_err(|e| ExchangeError::new(format!("OKX send login: {e}")))?;

    let login_wait = async {
        loop {
            match read.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                        // 登录失败 OKX 回 event=error（也可能是 login 带非 0 code）：带上交易所原话
                        if event.event == "login" || event.event == "error" {
                            if event.event == "login" && event.code.as_deref() == Some("0") {
                                tracing::info!("OKX private login success");
                                return Ok(());
                            }
                            return Err(ExchangeError::api(
                                Exchange::OKX,
                                event.code.as_deref().unwrap_or("?"),
                                format!("私有 WS 登录失败: {}", event.msg.as_deref().unwrap_or("")),
                            ));
                        }
                    }
                }
                Some(Ok(WsMessage::Ping(data))) => {
                    write
                        .send(WsMessage::Pong(data))
                        .await
                        .map_err(|e| ExchangeError::new(format!("OKX pong during login: {e}")))?;
                }
                Some(Err(e)) => {
                    return Err(ExchangeError::new(format!(
                        "OKX WS error during login: {e}"
                    )));
                }
                None => {
                    return Err(ExchangeError::transport(
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
            ExchangeError::new(format!(
                "OKX private login timed out after {LOGIN_TIMEOUT_SECS}s"
            ))
        })?
}

// ============================================================================
// 消息解析
// ============================================================================

fn server_error(value: &serde_json::Value) -> ExchangeError {
    let code = value
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let msg = value
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    ExchangeError::new(format!("OKX error: code={code}, msg={msg}"))
}

/// 启动期订阅确认判据（见 [`WsConn::await_subscriptions`]）：订阅回执
/// `{"event":"subscribe","arg":{"channel":"orders",…}}` 或该频道数据到达都算；
/// `{"event":"error",…}`（如订阅被拒）启动失败
fn subscription_confirmed(raw: &str) -> Result<Option<String>, ExchangeError> {
    if raw == "pong" {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::new(e.to_string()))?;
    let channel = value
        .pointer("/arg/channel")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    match value.get("event").and_then(|v| v.as_str()) {
        Some("error") => Err(server_error(&value)),
        Some("subscribe") | None => Ok(channel),
        Some(_) => Ok(None),
    }
}

/// 各未结束订单最近一笔已上报成交的 `tradeId`，用来识别重复的成交字段。
///
/// OKX orders 频道的 `fillSz` / `tradeId` 是该单「最近一笔」成交，不是「这次推送带来的新成交」：
/// 同一张单之后的推送（如部分成交后被撤）若仍带着上一笔的成交字段，照样上报就会把同一笔算两次
/// —— 策略以为两所不平，去市价补出一笔真敞口，再被持仓对账停机。是否真会重发未核实（实盘
/// 没有部分成交后撤单的样本），按 `tradeId` 去重对两种情况都对。`tradeId` 在同一合约内唯一。
///
/// 订单不再能成交（已全部成交 / 已撤）时删掉它的记录，只保留挂着的单。
#[derive(Default)]
struct LastTrades(HashMap<String, String>);

impl LastTrades {
    /// 这条推送的成交字段是不是一笔还没上报过的成交（没有成交时为 false）。
    /// 没有 `tradeId` 的成交无从去重，当新成交。`status` 是同一条推送经 codec 解析出的订单状态
    /// （状态口径只在 codec 一处）。
    fn is_new_fill(&mut self, data: &OrderPushData, status: &OrderStatus) -> bool {
        // 解析不了的 fillSz 留给 to_fill 报错，这里当有成交
        let has_fill = crate::domain::parse_decimal(&data.fill_sz).map_or(true, |sz| !sz.is_zero());
        let repeated = has_fill
            && !data.trade_id.is_empty()
            && self.0.get(&data.ord_id) == Some(&data.trade_id);
        if repeated {
            tracing::warn!(
                ord_id = %data.ord_id, trade_id = %data.trade_id, state = %data.state, fill_sz = %data.fill_sz,
                "OKX 推送重复带了已上报过的成交，不再计入"
            );
        }
        if status.is_terminal() {
            self.0.remove(&data.ord_id);
        } else if has_fill && !data.trade_id.is_empty() {
            self.0.insert(data.ord_id.clone(), data.trade_id.clone());
        }
        has_fill && !repeated
    }
}

fn parse_private_message(
    raw: &str,
    local_ts: u64,
    quote: &str,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
    last_trades: &mut LastTrades,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    // 应用层心跳应答：ws_loop 周期发文本 "ping"，OKX 回文本 "pong"（非 JSON）
    if raw == "pong" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::new(e.to_string()))?;

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
        .ok_or_else(|| ExchangeError::new(format!("Missing channel: {}", raw)))?;

    match channel {
        "account" => {
            let push: WsPush<OkxAccountPush> = serde_json::from_str(raw)
                .map_err(|e| ExchangeError::new(format!("account parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let exchange_ts = data.u_time.parse::<u64>().map_err(|_| {
                    ExchangeError::new(format!(
                        "Failed to parse OKX account timestamp: {}",
                        data.u_time
                    ))
                })?;
                // 净值与持仓总价值不从这里取，由 EquityPoller 走 REST（见 codec::AccountData）。
                // 推送每个币种的现金余额 (用于修正 greeks delta)
                for detail in &data.details {
                    let cash_bal: f64 = detail.cash_bal.parse().map_err(|_| {
                        ExchangeError::new(format!(
                            "Failed to parse cash_bal '{}' for ccy {}",
                            detail.cash_bal, detail.ccy
                        ))
                    })?;
                    events.push(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts,
                        local_ts,
                        data: AccountData::Exchange(ExchangeAccountData::Balance(Balance {
                            exchange: Exchange::OKX,
                            asset: detail.ccy.clone(),
                            cash: cash_bal,
                        })),
                    });
                }
            }
            Ok(events)
        }
        "orders" => {
            let push: WsPush<OrderPushData> = serde_json::from_str(raw)
                .map_err(|e| ExchangeError::new(format!("orders parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                // 私有流订阅的是 `instType=SWAP` 全账户推送，别的计价币、币本位合约的单也会
                // 推过来。映射不到本引擎 symbol（或无 meta、无法折算张数）的整条丢弃（#760：目录外
                // 成交不理），不进任何 symbol 的持仓 —— 绝不能按张数下发或当成同名的 USDT 合约。
                // 市场元数据加载的是该计价币的全部永续，故「映射得到却无 meta」只剩字段异常被剔除的
                // 合约（加载处已 error 留痕）。
                let Some(meta) = resolve_meta(&data.inst_id, quote, symbol_metas) else {
                    continue;
                };
                // 数量折算由签名强制完成（含 status 内嵌的 PartiallyFilled）
                let order_update = data.to_order_update(meta)?;
                let new_fill = last_trades.is_new_fill(data, &order_update.status);

                // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
                if let Some(fill) = data.to_fill(meta)?.filter(|_| new_fill) {
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
        // account-greeks 已迁移到 REST 轮询 (OkxGreeksPoller)
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
    use crate::domain::PriceStep;
    use rust_decimal_macros::dec;

    /// 已接入 BTC-USDT-SWAP（每张 0.01 BTC）
    fn metas() -> HashMap<Symbol, SymbolMeta> {
        HashMap::from([(
            "BTC".to_string(),
            SymbolMeta {
                exchange: Exchange::OKX,
                symbol: "BTC".to_string(),
                price_rule: Arc::new(PriceStep::new(
                    crate::domain::PositiveDecimal::new(dec!(0.1)).unwrap(),
                )),
                size_step: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
                min_order_size: crate::domain::PositiveDecimal::new(dec!(1.0)).unwrap(),
                min_notional: 0.0,
                contract_size: crate::domain::PositiveDecimal::new(dec!(0.01)).unwrap(),
            },
        )])
    }

    fn order_push(inst_id: &str, fill_sz: &str) -> String {
        push(inst_id, "filled", fill_sz, "7")
    }

    fn push(inst_id: &str, state: &str, fill_sz: &str, trade_id: &str) -> String {
        format!(
            r#"{{"arg":{{"channel":"orders","instType":"SWAP"}},"data":[{{"instId":"{inst_id}","ordId":"1",
            "clOrdId":"x-a","side":"buy","state":"{state}","sz":"5","fillSz":"{fill_sz}","accFillSz":"{fill_sz}",
            "fillPx":"60000","fillFee":"-0.1","fillFeeCcy":"USDT","fillTime":"1","tradeId":"{trade_id}"}}]}}"#
        )
    }

    fn kinds(raw: &str) -> Vec<String> {
        kinds_with(raw, &mut LastTrades::default())
    }

    fn kinds_with(raw: &str, last_trades: &mut LastTrades) -> Vec<String> {
        parse_private_message(raw, 0, "USDT", &metas(), last_trades)
            .unwrap()
            .into_iter()
            .map(|e| match e.data {
                AccountData::Symbol(SymbolAccountData::Fill(f)) => {
                    format!("fill:{}:{}", f.symbol, f.size)
                }
                AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => {
                    format!("order:{}", u.symbol)
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// 启动期确认判据：订阅回执与频道数据都算；连接计数等事件不算；订阅被拒启动失败
    #[test]
    fn subscribe_ack_or_channel_data_confirms_a_channel() {
        let ack =
            r#"{"event":"subscribe","arg":{"channel":"orders","instType":"SWAP"},"connId":"a"}"#;
        assert_eq!(
            subscription_confirmed(ack).unwrap().as_deref(),
            Some("orders")
        );
        assert_eq!(
            subscription_confirmed(&order_push("BTC-USDT-SWAP", "0"))
                .unwrap()
                .as_deref(),
            Some("orders")
        );
        let count =
            r#"{"event":"channel-conn-count","channel":"orders","connCount":"2","connId":"a"}"#;
        assert_eq!(subscription_confirmed(count).unwrap(), None);
        assert_eq!(subscription_confirmed("pong").unwrap(), None);
        let rejected = r#"{"event":"error","code":"60018","msg":"bad channel","connId":"a"}"#;
        assert!(subscription_confirmed(rejected)
            .unwrap_err()
            .to_string()
            .contains("60018"));
    }

    /// 同名不同计价币 / 币本位的推送不串进 BTC：整条丢弃
    #[test]
    fn other_quote_and_inverse_pushes_do_not_leak_into_btc() {
        assert_eq!(
            kinds(&order_push("BTC-USDT-SWAP", "3")),
            ["fill:BTC:0.03", "order:BTC"]
        );
        assert!(kinds(&order_push("BTC-USDC-SWAP", "3")).is_empty());
        assert!(kinds(&order_push("BTC-USD-SWAP", "3")).is_empty());
    }

    /// 同一笔成交（同一 tradeId）在后续推送里重复出现只计一次；新的 tradeId 照计
    #[test]
    fn repeated_trade_id_is_counted_once() {
        let mut seen = LastTrades::default();
        let sym = "BTC-USDT-SWAP";
        assert_eq!(
            kinds_with(&push(sym, "live", "0", ""), &mut seen),
            ["order:BTC"]
        );
        assert_eq!(
            kinds_with(&push(sym, "partially_filled", "2", "10"), &mut seen),
            ["fill:BTC:0.02", "order:BTC"]
        );
        assert_eq!(
            kinds_with(&push(sym, "partially_filled", "1", "11"), &mut seen),
            ["fill:BTC:0.01", "order:BTC"]
        );
        // 部分成交后被撤，推送仍带上一笔的成交字段
        assert_eq!(
            kinds_with(&push(sym, "canceled", "1", "11"), &mut seen),
            ["order:BTC"]
        );
        assert!(seen.0.is_empty(), "结束的单不留记录");
        // 品种目录外的合约不进去重表
        assert!(kinds_with(
            &push("BTC-USDC-SWAP", "partially_filled", "3", "20"),
            &mut seen
        )
        .is_empty());
        assert!(seen.0.is_empty());
    }

    /// 不认识的订单状态按解析失败处理（不当成拒单终态）
    #[test]
    fn unknown_order_state_is_a_parse_error() {
        let raw = push("BTC-USDT-SWAP", "something_new", "0", "");
        assert!(
            parse_private_message(&raw, 0, "USDT", &metas(), &mut LastTrades::default()).is_err()
        );
        assert_eq!(
            kinds(&push("BTC-USDT-SWAP", "mmp_canceled", "0", "")),
            ["order:BTC"]
        );
    }

    /// 联网只读：真登录 OKX 私有流，启动（spawn 返回）即 account / orders 都已确认。
    /// 需要白名单 IP 与 OKX_API_KEY / OKX_SECRET / OKX_PASSPHRASE。
    #[tokio::test]
    #[ignore = "需要联网、OKX 凭证与白名单 IP"]
    async fn live_private_subscriptions_confirm_at_startup() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("缺 {k}"));
        let credentials = OkxCredentials {
            api_key: env("OKX_API_KEY"),
            secret: env("OKX_SECRET"),
            passphrase: env("OKX_PASSPHRASE"),
        };
        let root = crate::tasks::Root::new("t");
        let started = std::time::Instant::now();
        OkxPrivateWs::spawn(
            &root.group("g"),
            credentials,
            AccountBus::new("account"),
            Arc::new(metas()),
            "USDT".to_string(),
        )
        .await
        .expect("订阅应在启动期确认");
        println!("OKX 私有订阅确认耗时 {:?}", started.elapsed());
        root.shutdown().await.unwrap();
    }
}
