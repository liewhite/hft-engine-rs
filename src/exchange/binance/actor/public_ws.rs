//! BinancePublicWs - 管理 Binance 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接（读循环是子任务）
//! - 处理订阅/退订命令
//! - 直接解析消息并发布到行情总线

use crate::domain::{now_ms, Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::codec::{AggTrade, BookTicker, MarkPriceUpdate, WsResponse};
use crate::exchange::binance::to_binance;
use crate::exchange::client::{ExchangeCmd, SubscriptionKind, WsError};
use crate::exchange::ws_loop::{self, WsConn};
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use futures_util::StreamExt;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 公开 WebSocket 组件
pub(crate) struct BinancePublicWs {
    inbox: Inbox<ExchangeCmd>,
    conn: WsConn,
    market_bus: MarketBus,
    /// Symbol 元数据（用于过滤不存在的 symbol）
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
}

impl BinancePublicWs {
    /// 在 `group` 下起一条公共 WS（迁移后 Binance 把公共流拆为 /public/ws 与 /market/ws，
    /// 由父节点决定本实例绑定哪个 URL），等连接建好，返回命令地址
    pub(crate) async fn spawn(
        group: &Group,
        name: &'static str,
        url: &'static str,
        market_bus: MarketBus,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        quote: String,
    ) -> anyhow::Result<Addr<ExchangeCmd>> {
        let (_, addr) = group
            .spawn_addressed(name, move |scope: Scope, inbox| async move {
                // 连接 WebSocket（失败向上传播 → 启动期受控退出，不重试/不重连）
                let (ws_stream, _) = tokio_tungstenite::connect_async(url)
                    .await
                    .map_err(|e| ExchangeError::ConnectionFailed(Exchange::Binance, e.to_string()))?;
                let (write, read) = ws_stream.split();
                let conn = WsConn::spawn(&scope.group("conn"), format!("{name}Loop"), read, write, ws_loop::WsKeepalive::binance())?;
                tracing::info!(url, "{name} started");
                Ok(Self {
                    inbox,
                    conn,
                    market_bus,
                    symbol_metas,
                    quote,
                    subscribed_streams: HashSet::new(),
                    subscribed_kinds: HashSet::new(),
                })
            })
            .await?;
        Ok(addr)
    }

    /// 批量发送订阅消息（一条 WebSocket 消息包含多个 streams）
    async fn send_subscribe_batch(&self, streams: Vec<String>) -> Result<(), WsError> {
        if streams.is_empty() {
            return Ok(());
        }
        let msg = json!({
            "method": "SUBSCRIBE",
            "params": streams,
            "id": 1
        })
        .to_string();
        self.conn.send(msg).await
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let stream = kind_to_stream(kind, &self.quote).ok_or_else(|| {
            WsError::ParseError(
                "Binance Candle 订阅未实现，无法取消订阅".to_string(),
            )
        })?;
        let msg = json!({
            "method": "UNSUBSCRIBE",
            "params": [stream],
            "id": 2
        })
        .to_string();
        self.conn.send(msg).await
    }

    /// 解析并处理消息
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_public_message(raw, &self.quote, now_ms(), &self.subscribed_kinds)
            .with_context(|| format!("Binance 公共 WS 解析失败: {raw}"))?;
        for event in events {
            self.market_bus.publish(event);
        }
        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: ExchangeCmd) -> anyhow::Result<()> {
        match cmd {
            ExchangeCmd::Subscribe(kind) => self.subscribe_batch(vec![kind]).await,
            ExchangeCmd::SubscribeBatch(kinds) => self.subscribe_batch(kinds).await,
            ExchangeCmd::Unsubscribe(kind) => self.unsubscribe(kind).await,
        }
    }

    async fn subscribe_batch(&mut self, kinds: Vec<SubscriptionKind>) -> anyhow::Result<()> {
        // 1. 过滤有效的 kinds（symbol 存在且未订阅）
        let mut new_streams = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in kinds {
            let symbol = kind.symbol();
            if !self.symbol_metas.contains_key(symbol) {
                tracing::warn!(
                    exchange = "Binance",
                    symbol = %symbol,
                    "Symbol not found in symbol_metas, ignoring subscription"
                );
                continue;
            }

            if self.subscribed_kinds.contains(&kind) {
                continue;
            }

            let stream = match kind_to_stream(&kind, &self.quote) {
                Some(s) => s,
                None => {
                    tracing::warn!(?kind, "Binance Candle 订阅未实现，忽略");
                    continue;
                }
            };
            if !self.subscribed_streams.contains(&stream) {
                new_streams.push(stream);
            }
            new_kinds.push(kind);
        }

        // 2. 批量发送订阅请求
        if !new_streams.is_empty() {
            // 去重
            let unique_streams: Vec<_> = new_streams
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            tracing::info!(
                exchange = "Binance",
                count = unique_streams.len(),
                "Batch subscribing to streams"
            );

            self.send_subscribe_batch(unique_streams.clone())
                .await
                .context("Binance 批量订阅发送失败")?;

            for stream in unique_streams {
                self.subscribed_streams.insert(stream);
            }
        }

        // 3. 记录已订阅的 kinds
        for kind in new_kinds {
            self.subscribed_kinds.insert(kind);
        }
        Ok(())
    }

    async fn unsubscribe(&mut self, kind: SubscriptionKind) -> anyhow::Result<()> {
        if !self.subscribed_kinds.remove(&kind) {
            return Ok(());
        }

        // 检查是否还有其他 kinds 使用同一个 stream
        let stream = match kind_to_stream(&kind, &self.quote) {
            Some(s) => s,
            None => {
                tracing::warn!(?kind, "Binance Candle 订阅未实现，忽略取消订阅");
                return Ok(());
            }
        };
        let stream_still_needed = self
            .subscribed_kinds
            .iter()
            .any(|k| kind_to_stream(k, &self.quote).as_deref() == Some(stream.as_str()));

        if !stream_still_needed {
            self.send_unsubscribe(&kind)
                .await
                .context("Binance 取消订阅发送失败")?;
            self.subscribed_streams.remove(&stream);
        }
        Ok(())
    }
}

impl Task for BinancePublicWs {
    async fn run(&mut self, _scope: &Scope) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                cmd = self.inbox.recv() => match cmd {
                    Some(cmd) => self.handle_cmd(cmd).await?,
                    None => return Ok(()),
                },
                frame = self.conn.recv() => self.handle_message(&frame?)?,
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

pub(crate) fn parse_public_message(
    raw: &str,
    quote: &str,
    local_ts: u64,
    subscribed_kinds: &HashSet<SubscriptionKind>,
) -> Result<Vec<MarketEvent>, WsError> {
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
        "markPriceUpdate" => {
            let update: MarkPriceUpdate = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("markPriceUpdate parse: {}", e)))?;
            let symbol = update.symbol(quote)
                ?;

            let mut events = Vec::new();

            // 根据订阅的 kinds 生成对应的事件
            if subscribed_kinds.contains(&SubscriptionKind::FundingRate { symbol: symbol.clone() })
            {
                events.push(MarketEvent {
                    exchange_ts,
                    local_ts,
                    data: MarketData::FundingRate(
                        update.to_funding_rate(quote, exchange_ts)
                            ?,
                    ),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::MarkPrice { symbol: symbol.clone() }) {
                events.push(MarketEvent {
                    exchange_ts,
                    local_ts,
                    data: MarketData::MarkPrice(
                        update.to_mark_price(quote, exchange_ts)
                            ?,
                    ),
                });
            }
            if subscribed_kinds.contains(&SubscriptionKind::IndexPrice { symbol }) {
                events.push(MarketEvent {
                    exchange_ts,
                    local_ts,
                    data: MarketData::IndexPrice(
                        update.to_index_price(quote, exchange_ts)
                            ?,
                    ),
                });
            }

            Ok(events)
        }
        "bookTicker" => {
            let ticker: BookTicker = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bookTicker parse: {}", e)))?;
            let bbo = ticker.to_bbo(quote)
                ?;
            Ok(vec![MarketEvent {
                exchange_ts,
                local_ts,
                data: MarketData::BBO(bbo),
            }])
        }
        "aggTrade" => {
            let agg: AggTrade = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("aggTrade parse: {}", e)))?;
            let trade = agg.to_market_trade(quote)
                ?;
            Ok(vec![MarketEvent {
                exchange_ts: trade.timestamp,
                local_ts,
                data: MarketData::MarketTrade(trade),
            }])
        }
        _ => {
            // 未知事件类型，记录警告但不报错
            tracing::warn!(event_type, raw, "Unknown Binance public event type");
            Ok(Vec::new())
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 把订阅类型转成 Binance stream 名。
///
/// Binance 的 K线（`Candle`）订阅尚未实现：父 actor 会把它路由到本 actor（`market_ws`），
/// 到达此处返回 `None`，由调用方记录 `warn!` 并跳过（不 panic，遵循"框架层错误受控处理"约定）。
///
/// **Some/None 只准由 kind 变体决定**：quote/dex 只参与报文字符串拼装。
/// `supports_subscription`（能力查询）依赖此前提传占位参数 —— 若将来按 quote/dex
/// 分支决定支持与否，能力查询会静默失真。
pub(crate) fn kind_to_stream(kind: &SubscriptionKind, quote: &str) -> Option<String> {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 markPrice@1s 流
        SubscriptionKind::FundingRate { symbol }
        | SubscriptionKind::MarkPrice { symbol }
        | SubscriptionKind::IndexPrice { symbol } => Some(format!(
            "{}@markPrice@1s",
            to_binance(symbol, quote).to_lowercase()
        )),
        SubscriptionKind::BBO { symbol } => Some(format!(
            "{}@bookTicker",
            to_binance(symbol, quote).to_lowercase()
        )),
        SubscriptionKind::Trades { symbol } => Some(format!(
            "{}@aggTrade",
            to_binance(symbol, quote).to_lowercase()
        )),
        SubscriptionKind::Candle { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::binance::actor::binance_actor::{pick_ws_target, WsTarget};

    /// 这两条是踩过坑的地方：aggTrade 订到 /public/ws 会被 ack 但永不推数据，
    /// 属"静默无数据"故障，故用单测把 kind -> 端点/stream 的映射钉住。
    #[test]
    fn trades_routes_to_market_ws_with_agg_trade_stream() {
        let kind = SubscriptionKind::Trades { symbol: "BTC".to_string() };
        assert_eq!(pick_ws_target(&kind), WsTarget::Market);
        assert_eq!(
            kind_to_stream(&kind, "USDT").as_deref(),
            Some("btcusdt@aggTrade")
        );
    }

    #[test]
    fn bbo_routes_to_public_high_freq_ws() {
        let kind = SubscriptionKind::BBO { symbol: "BTC".to_string() };
        assert_eq!(pick_ws_target(&kind), WsTarget::PublicHighFreq);
        assert_eq!(
            kind_to_stream(&kind, "USDT").as_deref(),
            Some("btcusdt@bookTicker")
        );
    }
}
