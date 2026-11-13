//! HyperliquidPublicWs - 管理 Hyperliquid 公共 WebSocket 连接
//!
//! 职责:
//! - 维护 WebSocket 连接（读循环是子任务）
//! - 处理订阅/退订命令
//! - 直接解析消息并发布到行情总线

use crate::domain::ExchangeError;
use crate::domain::{now_ms, Symbol};
use crate::exchange::client::{ExchangeCmd, SubscriptionKind};
use crate::exchange::hyperliquid::codec::{aggregate_trades, WsActiveAssetCtx, WsBbo, WsTrade};
use crate::exchange::hyperliquid::markets::HlMarkets;
use crate::exchange::hyperliquid::WS_URL;
use crate::exchange::ws_loop::ReconnectingWs;
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{Addr, Group, Inbox, Scope, Task};
use anyhow::Context as _;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// 公共 WebSocket 组件
pub(super) struct HyperliquidPublicWs {
    inbox: Inbox<ExchangeCmd>,
    conn: ReconnectingWs,
    /// 行情总线（发布事件）
    market_bus: MarketBus,
    /// 订阅记账（品种目录、已订阅的 stream / kind、trades 订阅时刻）
    book: SubscriptionBook,
}

/// 订阅记账：纯状态，不碰连接和总线（单测直接构造它）
struct SubscriptionBook {
    /// 品种目录：订阅前查 coin，查不到的 symbol 不订阅
    markets: Arc<HlMarkets>,
    /// 已订阅的底层 stream (用于 WebSocket 去重)
    subscribed_streams: HashSet<String>,
    /// 已订阅的 kinds (用于事件分发和取消订阅)
    subscribed_kinds: HashSet<SubscriptionKind>,
    /// 各 symbol 的 trades 订阅时刻 (毫秒)，用于丢弃订阅前的历史回填
    ///
    /// Hyperliquid 在 trades 订阅成功后会先推一批**订阅前**的历史成交 (实测首条消息含
    /// 30 笔、跨 6.5 秒、最早一笔早于 WS 握手)。这些事件的 `exchange_ts` 落后数秒，若原样
    /// 下发，策略在启动瞬间会看到一段并不存在于"当下"的成交脉冲，成交流因子被污染。
    /// 这是 Hyperliquid 独有行为 (Binance/OKX 均只推订阅后的增量)，故在本适配层吸收，
    /// 不外泄给策略。
    trades_subscribed_at: HashMap<Symbol, u64>,
}

impl SubscriptionBook {
    fn new(markets: Arc<HlMarkets>) -> Self {
        Self {
            markets,
            subscribed_streams: HashSet::new(),
            subscribed_kinds: HashSet::new(),
            trades_subscribed_at: HashMap::new(),
        }
    }

    /// kind 对应的订阅报文；只对已过订阅期筛选的 kind 调用（Candle 与目录外的 symbol
    /// 在订阅时就被跳过），走到这里还查不到属路由 bug
    fn subscription(&self, kind: &SubscriptionKind) -> Result<serde_json::Value, ExchangeError> {
        let channel = kind_channel(kind).ok_or_else(|| {
            ExchangeError::new(format!(
                "{kind:?} reached HyperliquidPublicWs (routing bug)"
            ))
        })?;
        let coin = self.markets.coin(kind.symbol()).ok_or_else(|| {
            ExchangeError::new(format!(
                "{} 不在 HL 品种目录里 (routing bug)",
                kind.symbol()
            ))
        })?;
        Ok(json!({"type": channel, "coin": coin}))
    }

    fn subscribe_msg(&self, kind: &SubscriptionKind) -> Result<String, ExchangeError> {
        Ok(json!({"method": "subscribe", "subscription": self.subscription(kind)?}).to_string())
    }

    /// 全部订阅报文，每个底层 stream 一条（断线重连后重发）
    fn all_subscriptions(&self) -> Result<Vec<String>, ExchangeError> {
        let mut streams = HashSet::new();
        self.subscribed_kinds
            .iter()
            .filter(|kind| self.stream(kind).is_some_and(|s| streams.insert(s)))
            .map(|kind| self.subscribe_msg(kind))
            .collect()
    }

    /// 底层 stream 标识（同一 stream 可被多个 kind 共用，用于去重）
    fn stream(&self, kind: &SubscriptionKind) -> Option<String> {
        Some(format!(
            "{}:{}",
            kind_channel(kind)?,
            self.markets.coin(kind.symbol())?
        ))
    }

    /// 该事件是否为 trades 订阅前的历史回填 (仅 MarketTrade 适用)。
    ///
    /// 判据：成交时间早于本 symbol 的 trades 订阅时刻。无订阅记录时不判为回填
    /// (宁可放过也不误杀实时成交)。
    fn is_trade_backfill(&self, event: &MarketEvent) -> bool {
        let MarketData::MarketTrade(trade) = &event.data else {
            return false;
        };
        let Some(&subscribed_at) = self.trades_subscribed_at.get(&trade.symbol) else {
            return false;
        };
        if trade.timestamp >= subscribed_at {
            return false;
        }
        tracing::debug!(
            exchange = "Hyperliquid",
            symbol = %trade.symbol,
            trade_ts = trade.timestamp,
            subscribed_at,
            "Dropping pre-subscription trade backfill"
        );
        true
    }
}

impl HyperliquidPublicWs {
    /// 在 `group` 下起公共 WS，等连接建好，返回命令地址
    pub(super) async fn spawn(
        group: &Group,
        market_bus: MarketBus,
        markets: Arc<HlMarkets>,
    ) -> anyhow::Result<Addr<ExchangeCmd>> {
        let (_, addr) = group
            .spawn_addressed(
                "HyperliquidPublicWs",
                move |scope: Scope, inbox| async move {
                    // 启动时连不上即启动失败（受控退出）；运行中断线由 run 原地重连（#762）
                    let conn = ReconnectingWs::connect(
                        scope.group("conn"),
                        "HyperliquidPublicWs",
                        WS_URL,
                        crate::exchange::hyperliquid::KEEPALIVE,
                    )
                    .await?;
                    tracing::info!("HyperliquidPublicWs started");
                    Ok(Self {
                        inbox,
                        conn,
                        market_bus,
                        book: SubscriptionBook::new(markets),
                    })
                },
            )
            .await?;
        Ok(addr)
    }

    /// 批量发送订阅消息（发送多条消息，无速率限制）
    async fn send_subscribe_batch(&self, kinds: &[SubscriptionKind]) -> Result<(), ExchangeError> {
        for kind in kinds {
            self.conn.send(self.book.subscribe_msg(kind)?).await;
        }
        Ok(())
    }

    /// 发送取消订阅消息
    async fn send_unsubscribe(&self, kind: &SubscriptionKind) -> Result<(), ExchangeError> {
        let subscription = self.book.subscription(kind)?;
        let msg = json!({
            "method": "unsubscribe",
            "subscription": subscription
        })
        .to_string();
        self.conn.send(msg).await;
        Ok(())
    }

    /// 断线重连：重发全部订阅；trades 订阅时刻一并刷新（重订阅后 HL 又会先推一批历史回填）
    async fn reconnect(&mut self, scope: &Scope) -> anyhow::Result<bool> {
        let subscriptions = self
            .book
            .all_subscriptions()
            .context("Hyperliquid 重连时生成订阅报文")?;
        if !self.conn.reconnect(scope, subscriptions).await {
            return Ok(false);
        }
        let now = now_ms();
        for at in self.book.trades_subscribed_at.values_mut() {
            *at = now;
        }
        Ok(true)
    }

    /// 解析并处理消息
    fn handle_message(&self, raw: &str) -> anyhow::Result<()> {
        let events = parse_public_message(
            raw,
            now_ms(),
            &self.book.markets,
            &self.book.subscribed_kinds,
        )
        .with_context(|| format!("Hyperliquid 公共 WS 解析失败: {raw}"))?;
        for event in events {
            if self.book.is_trade_backfill(&event) {
                continue;
            }
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
        let mut new_stream_kinds = Vec::new();
        let mut new_kinds = Vec::new();

        for kind in kinds {
            if self.book.subscribed_kinds.contains(&kind) {
                continue;
            }

            // 支不支持这种订阅、品种在不在目录，投产时已校验（provisioning::validate_subscriptions）；
            // 到这里还对不上就是校验的 bug，报错停机，不静默跳过
            let stream = self.book.stream(&kind).ok_or_else(|| {
                anyhow::anyhow!("HL 不支持的订阅类型或品种不在目录里（投产校验漏了）: {kind:?}")
            })?;
            if !self.book.subscribed_streams.contains(&stream) {
                new_stream_kinds.push(kind.clone());
                self.book.subscribed_streams.insert(stream);
            }
            new_kinds.push(kind);
        }

        // 2. 批量发送订阅请求
        if !new_stream_kinds.is_empty() {
            tracing::info!(
                exchange = "Hyperliquid",
                count = new_stream_kinds.len(),
                "Batch subscribing to streams"
            );
            self.send_subscribe_batch(&new_stream_kinds)
                .await
                .context("Hyperliquid 批量订阅发送失败")?;
        }

        // 3. 记录已订阅的 kinds；trades 额外记录订阅时刻以过滤历史回填
        let subscribed_at = now_ms();
        for kind in new_kinds {
            if let SubscriptionKind::Trades { symbol } = &kind {
                self.book
                    .trades_subscribed_at
                    .insert(symbol.clone(), subscribed_at);
            }
            self.book.subscribed_kinds.insert(kind);
        }
        Ok(())
    }

    async fn unsubscribe(&mut self, kind: SubscriptionKind) -> anyhow::Result<()> {
        if let SubscriptionKind::Trades { symbol } = &kind {
            self.book.trades_subscribed_at.remove(symbol);
        }
        if !self.book.subscribed_kinds.remove(&kind) {
            return Ok(());
        }

        // 检查是否还有其他 kinds 使用同一个 stream
        // Candle 从不进入 subscribed_kinds（订阅期已跳过），此处若为 None 属路由 bug，记录并跳过
        let Some(stream) = self.book.stream(&kind) else {
            tracing::error!(
                ?kind,
                "Candle routed to HyperliquidPublicWs unsubscribe (routing bug), skipping"
            );
            return Ok(());
        };
        let stream_still_needed = self
            .book
            .subscribed_kinds
            .iter()
            .any(|k| self.book.stream(k).as_ref() == Some(&stream));

        if !stream_still_needed {
            self.send_unsubscribe(&kind)
                .await
                .context("Hyperliquid 取消订阅发送失败")?;
            self.book.subscribed_streams.remove(&stream);
        }
        Ok(())
    }
}

impl Task for HyperliquidPublicWs {
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
                        if !self.reconnect(scope).await? {
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

pub(crate) fn parse_public_message(
    raw: &str,
    local_ts: u64,
    markets: &HlMarkets,
    subscribed_kinds: &HashSet<SubscriptionKind>,
) -> Result<Vec<MarketEvent>, ExchangeError> {
    // 公共流只订阅品种目录里的 coin：推来目录外的说明订阅记账出了错，按解析失败处理
    let symbol_of = |coin: &str| {
        markets
            .symbol_of(coin)
            .ok_or_else(|| ExchangeError::new(format!("HL 公共流推来品种目录外的 coin: {coin}")))
    };
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::new(e.to_string()))?;

    if value.get("channel").is_some() {
        let channel = value["channel"].as_str().unwrap_or("");

        match channel {
            // 服务端拒绝/报错：**必须致命**。HL 的错误下发格式是
            // `{"channel":"error","data":"..."}` —— 此前这里检查的是顶层 `error` 键与
            // `subscriptionResponse.data.error`，两处都不是 HL 的真实格式（死代码），
            // 真实错误落进下面的未知分支被 debug 掉：订阅被拒而连接健康、ping/pong
            // 正常，该 symbol 的行情再也不来却无人知晓。
            //
            // 公共流不做私有流那样的逐条订阅记账（订阅是随策略增减的动态集合，记账
            // 成本高）：错误报文即致命已经覆盖"订阅被拒"这一主要来路。
            "error" => {
                return Err(ExchangeError::new(format!(
                    "Hyperliquid 公共流返回错误: {}",
                    value.get("data").unwrap_or(&value)
                )));
            }
            "subscriptionResponse" => {
                return Ok(Vec::new());
            }
            // 应用层心跳应答（HL 文档口径：{"channel":"pong"}）
            "pong" => {
                return Ok(Vec::new());
            }
            "activeAssetCtx" => {
                // 资产上下文（包含资金费率、标记价格、指数价格）
                let data = &value["data"];
                let ctx: WsActiveAssetCtx = serde_json::from_value(data.clone())
                    .map_err(|e| ExchangeError::new(format!("activeAssetCtx parse: {}", e)))?;

                let symbol = symbol_of(&ctx.coin)?;
                let mut events = Vec::new();

                // 根据订阅的 kinds 生成对应的事件
                if subscribed_kinds.contains(&SubscriptionKind::FundingRate {
                    symbol: symbol.clone(),
                }) {
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::FundingRate(
                            ctx.to_funding_rate(symbol.clone(), local_ts)?,
                        ),
                    });
                }
                if subscribed_kinds.contains(&SubscriptionKind::MarkPrice {
                    symbol: symbol.clone(),
                }) {
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::MarkPrice(ctx.to_mark_price(symbol.clone(), local_ts)?),
                    });
                }
                if subscribed_kinds.contains(&SubscriptionKind::IndexPrice {
                    symbol: symbol.clone(),
                }) {
                    events.push(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::IndexPrice(ctx.to_index_price(symbol, local_ts)?),
                    });
                }

                return Ok(events);
            }
            "bbo" => {
                // BBO 数据
                let data = &value["data"];
                let bbo_data: WsBbo = serde_json::from_value(data.clone())
                    .map_err(|e| ExchangeError::new(format!("bbo parse: {}", e)))?;

                // Ok(None) = 单边盘口（稀薄品种的合法状态），跳过即可
                let Some(bbo) = bbo_data.to_bbo(symbol_of(&bbo_data.coin)?)? else {
                    tracing::debug!(coin = %bbo_data.coin, "Hyperliquid 单边盘口，跳过该条 BBO");
                    return Ok(Vec::new());
                };
                return Ok(vec![MarketEvent {
                    exchange_ts: bbo.timestamp,
                    local_ts,
                    data: MarketData::BBO(bbo),
                }]);
            }
            "trades" => {
                // trades 一次推送一个数组
                let trades: Vec<WsTrade> = serde_json::from_value(value["data"].clone())
                    .map_err(|e| ExchangeError::new(format!("trades parse: {}", e)))?;

                // 归集为 aggTrade 口径（HL 线路逐笔，Binance/OKX 下发的已是归集结果）
                let events = aggregate_trades(&trades, markets)?
                    .into_iter()
                    .map(|trade| MarketEvent {
                        exchange_ts: trade.timestamp,
                        local_ts,
                        data: MarketData::MarketTrade(trade),
                    })
                    .collect();
                return Ok(events);
            }
            "allMids" => {
                // 所有中间价，当前不处理
                return Ok(Vec::new());
            }
            _ => {
                tracing::debug!(channel, "Unknown Hyperliquid public channel");
                return Ok(Vec::new());
            }
        }
    }

    // pong 消息
    if value.get("method").map(|v| v.as_str()) == Some(Some("pong")) {
        return Ok(Vec::new());
    }

    // 其他未知消息
    tracing::debug!(raw, "Unhandled Hyperliquid public message");
    Ok(Vec::new())
}

// ============================================================================
// 辅助函数
// ============================================================================

/// kind 对应的 HL 订阅频道。`None` = 本适配层未实现（如 Candle）。
///
/// 只由 kind 变体决定、与 symbol 无关 —— 能力查询 `supports_subscription` 直接用它。
pub(crate) fn kind_channel(kind: &SubscriptionKind) -> Option<&'static str> {
    match kind {
        // FundingRate、MarkPrice、IndexPrice 都使用同一个 activeAssetCtx 订阅
        SubscriptionKind::FundingRate { .. }
        | SubscriptionKind::MarkPrice { .. }
        | SubscriptionKind::IndexPrice { .. } => Some("activeAssetCtx"),
        SubscriptionKind::BBO { .. } => Some("bbo"),
        SubscriptionKind::Trades { .. } => Some("trades"),
        SubscriptionKind::Candle { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, MarketTrade};
    use rust_decimal_macros::dec;

    const SUBSCRIBED_AT: u64 = 1_785_747_800_000;

    fn trade_event(ts: u64) -> MarketEvent {
        MarketEvent {
            exchange_ts: ts,
            local_ts: SUBSCRIBED_AT,
            data: MarketData::MarketTrade(MarketTrade {
                exchange: Exchange::Hyperliquid,
                symbol: "BTC".to_string(),
                price: dec!(62_500.0),
                qty: dec!(0.1),
                is_buyer_maker: false,
                timestamp: ts,
            }),
        }
    }

    /// 订阅记账（不涉及连接和总线），可选带一条 BTC 的 trades 订阅时刻
    fn book_with_subscription(subscribed: Option<u64>) -> SubscriptionBook {
        let mut book =
            SubscriptionBook::new(Arc::new(crate::exchange::hyperliquid::testkit::markets(&[
                "", "xyz",
            ])));
        if let Some(ts) = subscribed {
            book.trades_subscribed_at.insert("BTC".to_string(), ts);
        }
        book
    }

    #[tokio::test]
    async fn pre_subscription_trade_is_dropped_as_backfill() {
        let book = book_with_subscription(Some(SUBSCRIBED_AT));
        assert!(book.is_trade_backfill(&trade_event(SUBSCRIBED_AT - 1)));
    }

    #[tokio::test]
    async fn trade_at_or_after_subscription_is_kept() {
        let book = book_with_subscription(Some(SUBSCRIBED_AT));
        assert!(!book.is_trade_backfill(&trade_event(SUBSCRIBED_AT)));
        assert!(!book.is_trade_backfill(&trade_event(SUBSCRIBED_AT + 1)));
    }

    /// 无订阅记录时不判为回填 —— 宁可放过也不误杀实时成交
    #[tokio::test]
    async fn trade_without_subscription_record_is_kept() {
        let book = book_with_subscription(None);
        assert!(!book.is_trade_backfill(&trade_event(SUBSCRIBED_AT - 10_000)));
    }

    #[tokio::test]
    async fn non_trade_event_is_never_backfill() {
        let book = book_with_subscription(Some(SUBSCRIBED_AT));
        let clock = MarketEvent {
            exchange_ts: 0,
            local_ts: 0,
            data: MarketData::Clock,
        };
        assert!(!book.is_trade_backfill(&clock));
    }

    #[tokio::test]
    async fn trades_kind_maps_to_trades_subscription() {
        let book = book_with_subscription(None);
        let kind = SubscriptionKind::Trades {
            symbol: "BTC".to_string(),
        };
        let sub = book.subscription(&kind).expect("must map");
        assert_eq!(sub["type"], "trades");
        assert_eq!(sub["coin"], "BTC");
    }

    /// 同一条公共 WS 订阅主站与 xyz：coin 各带各的前缀，目录外的 symbol 不订阅
    #[tokio::test]
    async fn one_ws_subscribes_across_dexes() {
        let book = book_with_subscription(None);
        let bbo = |s: &str| SubscriptionKind::BBO {
            symbol: s.to_string(),
        };
        assert_eq!(book.subscription(&bbo("TSLA")).unwrap()["coin"], "xyz:TSLA");
        assert_eq!(book.stream(&bbo("BTC")).as_deref(), Some("bbo:BTC"));
        assert_eq!(
            book.stream(&SubscriptionKind::FundingRate {
                symbol: "TSLA".into()
            })
            .as_deref(),
            Some("activeAssetCtx:xyz:TSLA")
        );
        assert!(book.stream(&bbo("AAPL")).is_none());
        assert!(book.subscription(&bbo("AAPL")).is_err());
    }

    /// 重连重发：共用一个 stream 的多个 kind（资金费率、标记价格都走 activeAssetCtx）只发一条
    #[tokio::test]
    async fn resubscription_sends_one_message_per_stream() {
        let mut book = book_with_subscription(None);
        for kind in [
            SubscriptionKind::BBO {
                symbol: "BTC".into(),
            },
            SubscriptionKind::FundingRate {
                symbol: "BTC".into(),
            },
            SubscriptionKind::MarkPrice {
                symbol: "BTC".into(),
            },
            SubscriptionKind::BBO {
                symbol: "TSLA".into(),
            },
        ] {
            book.subscribed_kinds.insert(kind);
        }
        let mut coins: Vec<(String, String)> = book
            .all_subscriptions()
            .unwrap()
            .iter()
            .map(|m| {
                let v: serde_json::Value = serde_json::from_str(m).unwrap();
                assert_eq!(v["method"], "subscribe");
                (
                    v["subscription"]["type"].as_str().unwrap().into(),
                    v["subscription"]["coin"].as_str().unwrap().into(),
                )
            })
            .collect();
        coins.sort();
        assert_eq!(
            coins,
            [
                ("activeAssetCtx", "BTC"),
                ("bbo", "BTC"),
                ("bbo", "xyz:TSLA")
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }
}
