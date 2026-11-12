//! BinancePrivateWs - 管理 Binance 私有 WebSocket 连接
//!
//! 职责:
//! - 获取 ListenKey 并建立私有 WebSocket 连接
//! - 子任务：读循环、ListenKey 定时续期（失败即整支停）
//! - 直接解析消息并发布到账户总线

use super::listen_key::ListenKeyRefresher;
use crate::domain::{now_ms, Exchange, ExchangeError};
use crate::exchange::binance::codec::{AccountUpdate, OrderTradeUpdate, WsResponse};
use crate::exchange::binance::{BinanceCredentials, WS_PRIVATE_URL};
use crate::exchange::client::WsError;
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{AccountBus, AccountData, AccountEvent, ExchangeAccountData, SymbolAccountData};
use crate::tasks::{ChildHandle, Group, Scope, Task};
use anyhow::Context as _;
use futures_util::StreamExt;

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
        credentials: BinanceCredentials,
        rest_base_url: String,
        account_bus: AccountBus,
        quote: String,
    ) -> anyhow::Result<ChildHandle> {
        group
            .spawn("BinancePrivateWs", move |scope: Scope| async move {
                // 1. 获取 ListenKey（失败向上传播 → 启动期受控退出；属鉴权语义）
                let listen_key = create_listen_key(&rest_base_url, &credentials.api_key)
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "Failed to create Binance listen key");
                        ExchangeError::AuthenticationFailed(Exchange::Binance)
                    })?;

                // 2. 连接私有 WebSocket（迁移后用 query 参数传 listenKey；未带 events 让交易所推全部事件）
                // 失败向上传播 → 启动期受控退出，不重试/不重连。
                // 订阅确认：币安私有流没有订阅请求，listenKey 在连接地址里，握手成功即订阅生效
                // （OKX / HL 要另等回执，见 WsConn::await_subscriptions）
                let url = format!("{}?listenKey={}", WS_PRIVATE_URL, listen_key);
                let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
                    .await
                    .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;
                let (write, read) = ws_stream.split();

                let children = scope.group("children");
                let conn = WsConn::spawn(&children, "BinancePrivateWsLoop", read, write, ws_loop::WsKeepalive::binance())?;
                ListenKeyRefresher::spawn(&children, rest_base_url, credentials.api_key)?;

                tracing::info!("BinancePrivateWs started");
                Ok(Self { account_bus, quote, conn })
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
        loop {
            tokio::select! {
                _ = scope.stopping() => {
                    // 已经收到的回报处理完再走（可能有成交）
                    while let Some(raw) = self.conn.try_recv() {
                        self.handle_message(&raw)?;
                    }
                    return Ok(());
                }
                frame = self.conn.recv() => self.handle_message(&frame?)?,
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
            // 当不了基线（基线只能来自 manager 的投产快照），也不能参与增量（会与同一条推送里的
            // ORDER_TRADE_UPDATE 产出的 Fill 重复计算）。校验走 PositionLedger 的独立轮询。

            // 处理所有 balance 更新
            for bal_data in &update.a.balances {
                let balance = bal_data.to_balance()
                    ?;
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
                .map_err(|e| WsError::ParseError(format!("ORDER_TRADE_UPDATE parse: {}", e)))?;

            let mut events = Vec::new();

            // 同账户里别的计价币、交割合约的单也会推过来：映射不到本引擎 symbol 的，订单状态
            // 不关心，成交以 UnmappedFill 原样上报，不进任何 symbol 的持仓
            let Some(symbol) = update.symbol(quote) else {
                if let Some(f) = update.to_unmapped_fill().map_err(WsError::ParseError)? {
                    events.push(AccountEvent {
                        account: crate::domain::AccountId::Live,
                        exchange_ts,
                        local_ts,
                        data: AccountData::Exchange(ExchangeAccountData::UnmappedFill(f)),
                    });
                }
                return Ok(events);
            };

            // Fill 事件先于 OrderUpdate（确保乐观更新 position 后再移除 pending order）
            if let Some(fill) = update.to_fill(symbol.clone())
                ? {
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
                    update.to_order_update(symbol)
                        ?,
                )),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn order_push(symbol: &str, last_filled: &str) -> String {
        format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","E":1,"o":{{"s":"{symbol}","c":"x-a","S":"SELL","i":7,"X":"FILLED",
            "q":"0.5","p":"0","z":"0.5","l":"{last_filled}","L":"60000","ap":"60000","rp":"0","n":"0.01"}}}}"#
        )
    }

    fn kinds(raw: &str) -> Vec<String> {
        parse_private_message(raw, "USDT", 0)
            .unwrap()
            .into_iter()
            .map(|e| match e.data {
                AccountData::Symbol(SymbolAccountData::Fill(f)) => format!("fill:{}", f.symbol),
                AccountData::Symbol(SymbolAccountData::OrderUpdate(u)) => format!("order:{}", u.symbol),
                AccountData::Exchange(ExchangeAccountData::UnmappedFill(f)) => format!("unmapped:{}", f.instrument),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// 别的计价币、交割合约的回报不再让解析报错停机，也不串进同名 symbol
    #[test]
    fn other_quote_and_delivery_fills_are_reported_unmapped() {
        assert_eq!(kinds(&order_push("BTCUSDT", "0.5")), ["fill:BTC", "order:BTC"]);
        assert_eq!(kinds(&order_push("BTCUSDC", "0.5")), ["unmapped:BTCUSDC"]);
        assert_eq!(kinds(&order_push("BTCUSDT_251226", "0.5")), ["unmapped:BTCUSDT_251226"]);
        assert!(kinds(&order_push("BTCUSDC", "0")).is_empty());
    }
}
