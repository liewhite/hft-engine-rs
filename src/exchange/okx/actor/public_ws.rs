//! OkxPublicWs - 管理 OKX 公开 WebSocket 连接
//!
//! 职责:
//! - 维护公开 WebSocket 连接（读循环是子任务）
//! - 处理订阅/退订命令
//! - 直接解析消息并发布到行情总线

use crate::domain::{now_ms, Symbol, SymbolMeta};
use crate::exchange::client::{ExchangeCmd, SubscriptionKind, WsError};
use crate::exchange::okx::codec::{
    resolve_meta, BboData, FundingRateData, IndexTickerData, MarkPriceData, TradeData, WsPush,
};
use crate::exchange::okx::{to_okx, to_okx_index, WS_PUBLIC_URL};
use crate::exchange::ws_loop::{ReconnectingWs, WsKeepalive};
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 公开 WebSocket 组件
pub(super) struct OkxPublicWs {
    inbox: Inbox<ExchangeCmd>,
    conn: ReconnectingWs,
    /// 行情总线（发布事件）
    market_bus: MarketBus,
    /// Symbol 元数据（用于过滤不存在的 symbol）
    symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
    /// 计价币种 (e.g., "USDT")
    quote: String,
    /// 已订阅的 kinds (用于去重)
    subscribed: HashSet<SubscriptionKind>,
}

impl OkxPublicWs {
    /// 在 `group` 下起公共 WS，等连接建好，返回命令地址
    pub(super) async fn spawn(
        group: &Group,
        market_bus: MarketBus,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        quote: String,
    ) -> anyhow::Result<Addr<ExchangeCmd>> {
        let (_, addr) = group
            .spawn_addressed("OkxPublicWs", move |scope: Scope, inbox| async move {
                // 启动时连不上即启动失败（受控退出）；运行中断线由 run 原地重连（#762）
                let conn = ReconnectingWs::connect(scope.group("conn"), "OkxPublicWs", WS_PUBLIC_URL, WsKeepalive::okx()).await?;
                tracing::info!("OkxPublicWs started");
                Ok(Self {
                    inbox,
                    conn,
                    market_bus,
                    symbol_metas,
                    quote,
                    subscribed: HashSet::new(),
                })
            })
            .await?;
        Ok(addr)
    }

    /// 订阅报文（一条 WebSocket 消息包含多个 args）
    fn subscribe_msg(args: Vec<serde_json::Value>) -> String {
        json!({
            "op": "subscribe",
            "args": args
        })
        .to_string()
    }

    /// 断线重连后要重发的全部订阅
    fn all_subscriptions(&self) -> Vec<String> {
        // subscribed 里只有订阅时 kind_to_arg 成功的 kind，这里不会过滤掉什么
        let args: Vec<_> = self.subscribed.iter().filter_map(|k| kind_to_arg(k, &self.quote)).collect();
        if args.is_empty() {
            return Vec::new();
        }
        vec![Self::subscribe_msg(args)]
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), WsError> {
        let arg = kind_to_arg(kind, &self.quote).ok_or_else(|| {
            WsError::ParseError("Candle kind reached OkxPublicWs unsubscribe (routing bug)".to_string())
        })?;
        let msg = json!({
            "op": "unsubscribe",
            "args": [arg]
        })
        .to_string();
        self.conn.send(msg).await;
        Ok(())
    }

    /// 解析并处理消息
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_public_message(raw, &self.quote, now_ms(), &self.symbol_metas)
            .with_context(|| format!("OKX 公共 WS 解析失败: {raw}"))?;
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
        let mut new_args = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in kinds {
            let symbol = kind.symbol();
            if !self.symbol_metas.contains_key(symbol) {
                tracing::warn!(
                    exchange = "OKX",
                    symbol = %symbol,
                    "Symbol not found in symbol_metas, ignoring subscription"
                );
                continue;
            }

            if self.subscribed.contains(&kind) {
                continue;
            }

            match kind_to_arg(&kind, &self.quote) {
                Some(arg) => {
                    new_args.push(arg);
                    new_kinds.push(kind);
                }
                None => {
                    tracing::error!(?kind, "Candle routed to OkxPublicWs (routing bug), skipping");
                }
            }
        }

        // 2. 批量发送订阅请求
        if !new_args.is_empty() {
            tracing::info!(
                exchange = "OKX",
                count = new_args.len(),
                "Batch subscribing to channels"
            );
            self.conn.send(Self::subscribe_msg(new_args)).await;
        }

        // 3. 记录已订阅的 kinds
        for kind in new_kinds {
            self.subscribed.insert(kind);
        }
        Ok(())
    }

    async fn unsubscribe(&mut self, kind: SubscriptionKind) -> anyhow::Result<()> {
        if !self.subscribed.remove(&kind) {
            return Ok(());
        }
        self.send_unsubscribe(&kind)
            .await
            .context("OKX 取消订阅发送失败")?;
        Ok(())
    }
}

impl Task for OkxPublicWs {
    async fn run(&mut self, scope: &Scope) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                cmd = self.inbox.recv() => match cmd {
                    Some(cmd) => self.handle_cmd(cmd).await?,
                    None => return Ok(()),
                },
                frame = self.conn.recv() => match frame {
                    Some(raw) => self.handle_message(&raw)?,
                    None => {
                        if !self.conn.reconnect(scope, self.all_subscriptions()).await {
                            return Ok(());
                        }
                    }
                },
            }
        }
    }
}

// ============================================================================
// 消息解析
// ============================================================================

/// `symbol_metas` 用于把 OKX 的**张数**口径折算为币本位 (与 `parse_private_message` 同一套路)。
pub(crate) fn parse_public_message(
    raw: &str,
    quote: &str,
    local_ts: u64,
    symbol_metas: &HashMap<Symbol, SymbolMeta>,
) -> Result<Vec<MarketEvent>, WsError> {
    // 应用层心跳应答：ws_loop 周期发文本 "ping"，OKX 回文本 "pong"（非 JSON）
    if raw == "pong" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| WsError::ParseError(e.to_string()))?;

    // 检查是否是事件响应（控制消息，返回空 Vec）
    if let Some(event) = value.get("event").and_then(|v| v.as_str()) {
        match event {
            "subscribe" | "unsubscribe" => return Ok(Vec::new()),
            "error" => {
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
                let msg = value.get("msg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(WsError::ParseError(format!(
                    "OKX error: code={}, msg={}",
                    code, msg
                )));
            }
            _ => return Ok(Vec::new()),
        }
    }

    // 获取频道
    let channel = value
        .get("arg")
        .and_then(|a| a.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| WsError::ParseError(format!("Missing channel: {}", raw)))?;

    match channel {
        "funding-rate" => {
            let push: WsPush<FundingRateData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("funding-rate parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let rate = data.to_funding_rate(quote, local_ts)
                    ?;
                events.push(MarketEvent {
                    exchange_ts: local_ts,
                    local_ts,
                    data: MarketData::FundingRate(rate),
                });
            }
            Ok(events)
        }
        "bbo-tbt" => {
            let push: WsPush<BboData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("bbo-tbt parse: {}", e)))?;
            let inst_id = push
                .arg
                .inst_id
                .as_ref()
                .ok_or_else(|| WsError::ParseError("Missing instId in bbo-tbt".into()))?;

            // 盘口量也是张数，需按 meta 折算；缺 meta 无法折算，丢弃并告警
            let Some(meta) = resolve_meta(inst_id, quote, symbol_metas) else {
                tracing::warn!(exchange = "OKX", %inst_id, "Missing SymbolMeta for bbo, dropping");
                return Ok(Vec::new());
            };

            let mut events = Vec::new();
            for data in &push.data {
                // Ok(None) = 单边盘口（稀薄品种的合法状态），跳过该条即可
                let Some(bbo) = data.to_bbo(meta)? else {
                    tracing::debug!(symbol = %meta.symbol, "OKX 单边盘口，跳过该条 BBO");
                    continue;
                };
                events.push(MarketEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: MarketData::BBO(bbo),
                });
            }
            Ok(events)
        }
        "trades" => {
            let push: WsPush<TradeData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("trades parse: {}", e)))?;
            let inst_id = push
                .arg
                .inst_id
                .as_ref()
                .ok_or_else(|| WsError::ParseError("Missing instId in trades".into()))?;

            let Some(meta) = resolve_meta(inst_id, quote, symbol_metas) else {
                tracing::warn!(exchange = "OKX", %inst_id, "Missing SymbolMeta for trades, dropping");
                return Ok(Vec::new());
            };

            let mut events = Vec::new();
            for data in &push.data {
                let trade = data.to_market_trade(meta)?;
                events.push(MarketEvent {
                    exchange_ts: trade.timestamp,
                    local_ts,
                    data: MarketData::MarketTrade(trade),
                });
            }
            Ok(events)
        }
        "mark-price" => {
            let push: WsPush<MarkPriceData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("mark-price parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let mp = data.to_mark_price(quote)
                    ?;
                events.push(MarketEvent {
                    exchange_ts: mp.timestamp,
                    local_ts,
                    data: MarketData::MarkPrice(mp),
                });
            }
            Ok(events)
        }
        "index-tickers" => {
            let push: WsPush<IndexTickerData> = serde_json::from_str(raw)
                .map_err(|e| WsError::ParseError(format!("index-tickers parse: {}", e)))?;

            let mut events = Vec::new();
            for data in &push.data {
                let ip = data.to_index_price(quote)
                    ?;
                events.push(MarketEvent {
                    exchange_ts: ip.timestamp,
                    local_ts,
                    data: MarketData::IndexPrice(ip),
                });
            }
            Ok(events)
        }
        _ => {
            tracing::warn!(channel, raw, "Unknown OKX public channel");
            Ok(Vec::new())
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 把订阅类型转成 OKX 频道 arg。
///
/// `Candle` 由 OkxNode 路由到 OkxBusinessWs，正常不会到达此处；返回 `None` 表示
/// 路由错误，由调用方记录并跳过（不 panic，遵循"框架层错误受控处理"约定）。
///
/// **Some/None 只准由 kind 变体决定**：quote/dex 只参与报文字符串拼装。
/// `supports_subscription`（能力查询）依赖此前提传占位参数 —— 若将来按 quote/dex
/// 分支决定支持与否，能力查询会静默失真。
pub(crate) fn kind_to_arg(kind: &SubscriptionKind, quote: &str) -> Option<serde_json::Value> {
    match kind {
        SubscriptionKind::FundingRate { symbol } => Some(json!({
            "channel": "funding-rate",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::BBO { symbol } => Some(json!({
            "channel": "bbo-tbt",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::Trades { symbol } => Some(json!({
            // `trades` 按主动单 + 成交价归集推送；`trades-all` 才是每笔明细，此处不需要
            "channel": "trades",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::MarkPrice { symbol } => Some(json!({
            "channel": "mark-price",
            "instId": to_okx(symbol, quote)
        })),
        SubscriptionKind::IndexPrice { symbol } => Some(json!({
            // OKX index-tickers 使用指数 ID 格式 (如 BTC-USDT)
            "channel": "index-tickers",
            "instId": to_okx_index(symbol, quote)
        })),
        SubscriptionKind::Candle { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, MarketTrade};
    use crate::domain::StepFormatter;

    const TRADES_PUSH: &str = r#"{
        "arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},
        "data":[{"instId":"BTC-USDT-SWAP","tradeId":"1","px":"62500.0",
                 "sz":"12","side":"buy","ts":"1785747728201","count":"3"}]
    }"#;

    /// BTC-USDT-SWAP: ctVal = 0.01 BTC/张
    fn metas(contract_size: f64) -> HashMap<Symbol, SymbolMeta> {
        HashMap::from([(
            "BTC".to_string(),
            SymbolMeta {
                exchange: Exchange::OKX,
                symbol: "BTC".to_string(),
                price_formatter: Arc::new(StepFormatter::new(0.1)),
                size_step: 0.01,
                min_order_size: 0.01,
                min_notional: 0.0,
                contract_size,
            },
        )])
    }

    fn trades_of(events: Vec<MarketEvent>) -> Vec<MarketTrade> {
        events
            .into_iter()
            .filter_map(|e| match e.data {
                MarketData::MarketTrade(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn trade_size_is_converted_from_contracts_to_coin() {
        let events = parse_public_message(TRADES_PUSH, "USDT", 0, &metas(0.01)).expect("parse");
        let trades = trades_of(events);
        assert_eq!(trades.len(), 1);
        // 12 张 x 0.01 = 0.12 BTC；若漏了折算会是 12（差 100 倍）
        assert!((trades[0].qty - 0.12).abs() < 1e-12, "got {}", trades[0].qty);
        assert_eq!(trades[0].price, 62500.0);
        assert!(!trades[0].is_buyer_maker, "side=buy 是主动买");
    }

    /// 缺 SymbolMeta 时必须丢弃，绝不能把张数当币数发下去
    #[test]
    fn trade_without_symbol_meta_is_dropped_not_passed_through() {
        let events = parse_public_message(TRADES_PUSH, "USDT", 0, &HashMap::new()).expect("parse");
        assert!(trades_of(events).is_empty());
    }

    #[test]
    fn trades_kind_maps_to_trades_channel() {
        let kind = SubscriptionKind::Trades { symbol: "BTC".to_string() };
        let arg = kind_to_arg(&kind, "USDT").expect("must map");
        assert_eq!(arg["channel"], "trades");
        assert_eq!(arg["instId"], "BTC-USDT-SWAP");
    }
}
