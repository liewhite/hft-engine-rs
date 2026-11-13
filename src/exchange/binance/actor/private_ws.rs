//! BinancePrivateWs - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 子任务：读循环、ListenKey 定时续期（连续续不上约 50 分钟才整支停）
//! - 直接解析消息并发布到账户总线

use super::listen_key::ListenKeyRefresher;
use crate::domain::{now_ms, Exchange, ExchangeError};
use crate::exchange::binance::codec::{AccountUpdate, OrderTradeUpdate, WsResponse};
use crate::exchange::binance::{BinanceAccount, WS_PRIVATE_URL};
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{
    AccountBus, AccountData, AccountEvent, ExchangeAccountData, SymbolAccountData,
};
use crate::tasks::{ChildHandle, Group, Scope, Task};
use anyhow::Context as _;
use futures_util::StreamExt;
use std::sync::Arc;

/// 私有 WebSocket 组件（没有收件箱：只读不收命令）
pub(super) struct BinancePrivateWs {
    account_bus: AccountBus,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    conn: WsConn,
}

impl BinancePrivateWs {
    /// 在 `group` 下起私有 WS，等连接建好、ListenKey 续期就位
    pub(super) async fn spawn(
        group: &Group,
        account: Arc<BinanceAccount>,
        account_bus: AccountBus,
        quote: String,
    ) -> anyhow::Result<ChildHandle> {
        group
            .spawn("BinancePrivateWs", move |scope: Scope| async move {
                // 1. 获取 ListenKey（失败向上传播 → 启动期受控退出）
                let listen_key = account
                    .create_listen_key()
                    .await
                    .context("Binance 创建 ListenKey 失败")?;

                // 2. 连接私有 WebSocket（迁移后用 query 参数传 listenKey；未带 events 让交易所推全部事件）
                // 失败向上传播 → 启动期受控退出，不重试/不重连。
                // 订阅确认：币安私有流没有订阅请求，listenKey 在连接地址里，握手成功即订阅生效
                // （OKX / HL 要另等回执，见 WsConn::await_subscriptions）
                let url = format!("{}?listenKey={}", WS_PRIVATE_URL, listen_key);
                let ws_stream = crate::exchange::ws_loop::connect(&url)
                    .await
                    .map_err(|e| ExchangeError::transport(Exchange::Binance, e))?;
                let (write, read) = ws_stream.split();

                let children = scope.group("children");
                let conn = WsConn::spawn(
                    &children,
                    "BinancePrivateWsLoop",
                    read,
                    write,
                    ws_loop::WsKeepalive::binance(),
                )?;
                ListenKeyRefresher::spawn(&children, account)?;

                tracing::info!("BinancePrivateWs started");
                Ok(Self {
                    account_bus,
                    quote,
                    conn,
                })
            })
            .await
    }

    /// 解析并发布一帧
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_private_message(raw, &self.quote, now_ms())
            .with_context(|| format!("Binance 私有 WS 解析失败: {raw}"))?;
        for event in events {
            self.account_bus.publish(event);
        }
        Ok(())
    }
}

impl Task for BinancePrivateWs {
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

fn parse_private_message(
    raw: &str,
    quote: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, ExchangeError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::new(e.to_string()))?;

    // 检查是否是订阅响应（控制消息，返回空 Vec）
    if value.get("id").is_some() {
        if let Ok(resp) = serde_json::from_str::<WsResponse>(raw) {
            if let Some(err) = resp.error {
                return Err(ExchangeError::new(format!(
                    "Subscribe error: code={}, msg={}",
                    err.code, err.msg
                )));
            }
        }
        return Ok(Vec::new());
    }

    // 提取交易所事件时间 (E 字段，毫秒)
    let exchange_ts = value.get("E").and_then(|v| v.as_u64()).unwrap_or_else(|| {
        tracing::warn!(raw, "Missing exchange timestamp (E), using local_ts");
        local_ts
    });

    // 根据事件类型解析
    let event_type = value
        .get("e")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ExchangeError::new(format!("Missing event type: {}", raw)))?;

    match event_type {
        "ACCOUNT_UPDATE" => {
            let update: AccountUpdate = serde_json::from_str(raw)
                .map_err(|e| ExchangeError::new(format!("ACCOUNT_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // **不处理 `a.P`（持仓）**：那是持仓**快照**，而持仓的维护模型是「启动期 REST
            // 基线 + 之后全程 Fill 累加」（见 crate::messaging::PositionBaseline）。快照既
            // 当不了基线（基线只能来自 manager 的投产快照），也不能参与增量（会与同一条推送里的
            // ORDER_TRADE_UPDATE 产出的 Fill 重复计算）。校验走 PositionLedger 的独立轮询。

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                let balance = bal_data.to_balance()?;
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts,
                    local_ts,
                    data: AccountData::Exchange(ExchangeAccountData::Balance(balance)),
                });
            }

            Ok(events)
        }
        "ORDER_TRADE_UPDATE" => {
            let update: OrderTradeUpdate = serde_json::from_str(raw)
                .map_err(|e| ExchangeError::new(format!("ORDER_TRADE_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // 同账户里别的计价币、交割合约的单也会推过来：映射不到本引擎 symbol 的整条丢弃
            // （目录外成交不理，#760），不进任何 symbol 的持仓
            let Some(symbol) = update.symbol(quote) else {
                return Ok(events);
            };

            // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
            if let Some(fill) = update.to_fill(symbol.clone(), quote)? {
                events.push(AccountEvent {
                    // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                    account: crate::domain::AccountId::Live,
                    exchange_ts,
                    local_ts,
                    data: AccountData::Symbol(SymbolAccountData::Fill(fill)),
                });
            }

            events.push(AccountEvent {
                // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                account: crate::domain::AccountId::Live,
                exchange_ts,
                local_ts,
                data: AccountData::Symbol(SymbolAccountData::OrderUpdate(
                    update.to_order_update(symbol)?,
                )),
            });

            Ok(events)
        }
        "listenKeyExpired" => {
            // ListenKey 过期：交易所随后不再推私有回报（连接可能还开着），按断线处理、整支停
            Err(ExchangeError::new(format!(
                "ListenKey 已过期，私有回报从此中断: {raw}"
            )))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn order_push(symbol: &str, last_filled: &str) -> String {
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":1,"o":{{"s":"{symbol}","c":"x-a","S":"SELL","i":7,"X":"FILLED",
            "q":"0.5","p":"0","z":"0.5","l":"{last_filled}","L":"60000","T":2,"ap":"60000","rp":"0","n":"0.01","N":"USDT"}}}}"#
        )
    }

    fn kinds(raw: &str) -> Vec<String> {
        parse_private_message(raw, "USDT", 0)
            .unwrap()
            .into_iter()
            .map(|e| match e.data {
                AccountData::Symbol(SymbolAccountData::Fill(f)) => format!("fill:{}", f.symbol),
                AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => {
                    format!("order:{}", u.symbol)
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// 别的计价币、交割合约的回报不让解析报错停机，也不串进同名 symbol：整条丢弃
    #[test]
    fn other_quote_and_delivery_pushes_are_dropped() {
        assert_eq!(
            kinds(&order_push("BTCUSDT", "0.5")),
            ["fill:BTC", "order:BTC"]
        );
        assert!(kinds(&order_push("BTCUSDC", "0.5")).is_empty());
        assert!(kinds(&order_push("BTCUSDT_251226", "0.5")).is_empty());
    }

    /// 不认识的订单状态按解析失败处理（不当成拒单终态）
    #[test]
    fn unknown_order_status_is_a_parse_error() {
        let raw = order_push("BTCUSDT", "0").replace(r#""X":"FILLED""#, r#""X":"SOMETHING_NEW""#);
        assert!(parse_private_message(&raw, "USDT", 0).is_err());
        let stp =
            order_push("BTCUSDT", "0").replace(r#""X":"FILLED""#, r#""X":"EXPIRED_IN_MATCH""#);
        assert_eq!(kinds(&stp), ["order:BTC"]);
    }

    /// ListenKey 过期即出错（私有回报从此中断），不当未知事件忽略
    #[test]
    fn listen_key_expired_is_an_error() {
        let raw = r#"{"e":"listenKeyExpired","E":1736996475556,"listenKey":"k"}"#;
        let err = parse_private_message(raw, "USDT", 0).unwrap_err();
        assert!(err.to_string().contains("ListenKey 已过期"), "{err}");
    }
}
