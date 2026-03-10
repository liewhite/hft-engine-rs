//! SpreadArb Metrics Actor - 订阅 Income 事件，推送价差套利专属指标到 Pushgateway
//!
//! 指标：
//! - `{prefix}_equity` (labels: exchange)
//! - `{prefix}_leverage` (labels: exchange)
//! - `{prefix}_position` (labels: exchange, symbol) — 仓位数量
//! - `{prefix}_quote` (labels: exchange, symbol, side) — BBO bid/ask 价格
//! - `{prefix}_spread_pct` (labels: symbol, type) — open/close spread %

use crate::domain::{Exchange, Symbol};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::metrics_pusher::MetricsPusher;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use prometheus::{GaugeVec, Opts};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::interval;

/// 跨交易所价差对配置
pub struct SpreadPairConfig {
    /// 现货侧交易所 (e.g., IBKR)
    pub spot_exchange: Exchange,
    pub spot_symbol: Symbol,
    /// 永续侧交易所 (e.g., Hyperliquid)
    pub perp_exchange: Exchange,
    pub perp_symbol: Symbol,
}

/// SpreadArbMetricsActor 初始化参数
pub struct SpreadArbMetricsArgs {
    pub pushgateway_url: String,
    pub metric_prefix: String,
    pub push_interval_ms: u64,
    pub spread_pairs: Vec<SpreadPairConfig>,
}

pub struct SpreadArbMetricsActor {
    pusher: MetricsPusher,
    equity_gauge: GaugeVec,
    leverage_gauge: GaugeVec,
    position_gauge: GaugeVec,
    quote_gauge: GaugeVec,
    spread_pct_gauge: GaugeVec,
    /// BBO 缓存 (exchange, symbol) → (bid, ask)
    bbo_cache: HashMap<(Exchange, Symbol), (f64, f64)>,
    spread_pairs: Vec<SpreadPairConfig>,
}

impl Actor for SpreadArbMetricsActor {
    type Args = SpreadArbMetricsArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let pusher = MetricsPusher::new(args.pushgateway_url.clone(), args.metric_prefix.clone());
        let prefix = &args.metric_prefix;

        let equity_gauge = GaugeVec::new(
            Opts::new(format!("{prefix}_equity"), "Account equity by exchange"),
            &["exchange"],
        )?;
        pusher.registry.register(Box::new(equity_gauge.clone()))?;

        let leverage_gauge = GaugeVec::new(
            Opts::new(format!("{prefix}_leverage"), "Account leverage by exchange"),
            &["exchange"],
        )?;
        pusher.registry.register(Box::new(leverage_gauge.clone()))?;

        let position_gauge = GaugeVec::new(
            Opts::new(format!("{prefix}_position"), "Position size by exchange and symbol"),
            &["exchange", "symbol"],
        )?;
        pusher.registry.register(Box::new(position_gauge.clone()))?;

        let quote_gauge = GaugeVec::new(
            Opts::new(format!("{prefix}_quote"), "BBO quote price by exchange, symbol, and side"),
            &["exchange", "symbol", "side"],
        )?;
        pusher.registry.register(Box::new(quote_gauge.clone()))?;

        let spread_pct_gauge = GaugeVec::new(
            Opts::new(format!("{prefix}_spread_pct"), "Cross-exchange spread percentage by symbol and type"),
            &["symbol", "type"],
        )?;
        pusher.registry.register(Box::new(spread_pct_gauge.clone()))?;

        let push_interval_ms = args.push_interval_ms;
        let weak_ref = actor_ref.downgrade();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_millis(push_interval_ms));
            loop {
                interval.tick().await;
                if let Some(actor_ref) = weak_ref.upgrade() {
                    if actor_ref.tell(PushMetrics).send().await.is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
        });

        tracing::info!(
            pushgateway_url = %args.pushgateway_url,
            push_interval_ms,
            spread_pairs = args.spread_pairs.len(),
            "SpreadArbMetricsActor started"
        );

        Ok(Self {
            pusher,
            equity_gauge,
            leverage_gauge,
            position_gauge,
            quote_gauge,
            spread_pct_gauge,
            bbo_cache: HashMap::new(),
            spread_pairs: args.spread_pairs,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "SpreadArbMetricsActor stopped");
        Ok(())
    }
}

impl Message<IncomeEvent> for SpreadArbMetricsActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match &msg.data {
            ExchangeEventData::AccountInfo {
                exchange,
                equity,
                notional,
            } => {
                let label = exchange_to_label(*exchange);
                self.equity_gauge.with_label_values(&[label]).set(*equity);
                let leverage = if *equity > 0.0 { notional / equity } else { 0.0 };
                self.leverage_gauge.with_label_values(&[label]).set(leverage);
            }
            ExchangeEventData::BBO(bbo) => {
                self.bbo_cache.insert(
                    (bbo.exchange, bbo.symbol.clone()),
                    (bbo.bid_price, bbo.ask_price),
                );

                let ex = exchange_to_label(bbo.exchange);
                self.quote_gauge
                    .with_label_values(&[ex, &bbo.symbol, "bid"])
                    .set(bbo.bid_price);
                self.quote_gauge
                    .with_label_values(&[ex, &bbo.symbol, "ask"])
                    .set(bbo.ask_price);

                self.update_spread(bbo.exchange, &bbo.symbol);
            }
            ExchangeEventData::Position(pos) => {
                let label = exchange_to_label(pos.exchange);
                self.position_gauge
                    .with_label_values(&[label, &pos.symbol])
                    .set(pos.size);
            }
            _ => {}
        }
    }
}

impl SpreadArbMetricsActor {
    /// BBO 更新时重新计算关联 pair 的 spread
    fn update_spread(&self, exchange: Exchange, symbol: &str) {
        for pair in &self.spread_pairs {
            let is_spot = exchange == pair.spot_exchange && symbol == pair.spot_symbol;
            let is_perp = exchange == pair.perp_exchange && symbol == pair.perp_symbol;
            if !is_spot && !is_perp {
                continue;
            }

            let spot_bbo = self
                .bbo_cache
                .get(&(pair.spot_exchange, pair.spot_symbol.clone()));
            let perp_bbo = self
                .bbo_cache
                .get(&(pair.perp_exchange, pair.perp_symbol.clone()));

            if let (Some(&(spot_bid, spot_ask)), Some(&(perp_bid, perp_ask))) =
                (spot_bbo, perp_bbo)
            {
                if spot_ask > 0.0 {
                    let open = (perp_bid - spot_ask) / spot_ask * 100.0;
                    self.spread_pct_gauge
                        .with_label_values(&[pair.spot_symbol.as_str(), "open"])
                        .set(open);
                }
                if spot_bid > 0.0 {
                    let close = (perp_ask - spot_bid) / spot_bid * 100.0;
                    self.spread_pct_gauge
                        .with_label_values(&[pair.spot_symbol.as_str(), "close"])
                        .set(close);
                }
            }
        }
    }
}

struct PushMetrics;

impl Message<PushMetrics> for SpreadArbMetricsActor {
    type Reply = ();

    async fn handle(&mut self, _msg: PushMetrics, _ctx: &mut Context<Self, Self::Reply>) {
        self.pusher.push().await;
    }
}

fn exchange_to_label(exchange: Exchange) -> &'static str {
    match exchange {
        Exchange::Binance => "binance",
        Exchange::OKX => "okx",
        Exchange::Hyperliquid => "hyperliquid",
        Exchange::IBKR => "ibkr",
    }
}
