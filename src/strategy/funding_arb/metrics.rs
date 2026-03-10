//! FundingArb Metrics Actor - 订阅 Income 事件，推送账户和仓位指标到 Pushgateway
//!
//! 指标：
//! - `{prefix}_equity` (labels: exchange)
//! - `{prefix}_notional` (labels: exchange)
//! - `{prefix}_leverage` (labels: exchange)
//! - `{prefix}_position` (labels: exchange, symbol) — size × mid_price

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

/// FundingArbMetricsActor 初始化参数
pub struct FundingArbMetricsArgs {
    pub pushgateway_url: String,
    pub metric_prefix: String,
    pub push_interval_ms: u64,
}

pub struct FundingArbMetricsActor {
    pusher: MetricsPusher,
    equity_gauge: GaugeVec,
    notional_gauge: GaugeVec,
    leverage_gauge: GaugeVec,
    position_gauge: GaugeVec,
    price_cache: HashMap<(Exchange, Symbol), f64>,
}

impl Actor for FundingArbMetricsActor {
    type Args = FundingArbMetricsArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let pusher = MetricsPusher::new(args.pushgateway_url.clone(), args.metric_prefix.clone());

        let equity_gauge = GaugeVec::new(
            Opts::new(format!("{}_equity", args.metric_prefix), "Account equity by exchange"),
            &["exchange"],
        )?;
        pusher.registry.register(Box::new(equity_gauge.clone()))?;

        let notional_gauge = GaugeVec::new(
            Opts::new(format!("{}_notional", args.metric_prefix), "Account total position notional by exchange"),
            &["exchange"],
        )?;
        pusher.registry.register(Box::new(notional_gauge.clone()))?;

        let leverage_gauge = GaugeVec::new(
            Opts::new(format!("{}_leverage", args.metric_prefix), "Account leverage (notional/equity) by exchange"),
            &["exchange"],
        )?;
        pusher.registry.register(Box::new(leverage_gauge.clone()))?;

        let position_gauge = GaugeVec::new(
            Opts::new(format!("{}_position", args.metric_prefix), "Position notional (size * price) by exchange and symbol"),
            &["exchange", "symbol"],
        )?;
        pusher.registry.register(Box::new(position_gauge.clone()))?;

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
            "FundingArbMetricsActor started"
        );

        Ok(Self {
            pusher,
            equity_gauge,
            notional_gauge,
            leverage_gauge,
            position_gauge,
            price_cache: HashMap::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "FundingArbMetricsActor stopped");
        Ok(())
    }
}

impl Message<IncomeEvent> for FundingArbMetricsActor {
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
                self.notional_gauge.with_label_values(&[label]).set(*notional);
                let leverage = if *equity > 0.0 { notional / equity } else { 0.0 };
                self.leverage_gauge.with_label_values(&[label]).set(leverage);
            }
            ExchangeEventData::BBO(bbo) => {
                let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;
                self.price_cache
                    .insert((bbo.exchange, bbo.symbol.clone()), mid_price);
            }
            ExchangeEventData::Position(pos) => {
                let label = exchange_to_label(pos.exchange);
                let notional = self
                    .price_cache
                    .get(&(pos.exchange, pos.symbol.clone()))
                    .map(|price| pos.size * price)
                    .unwrap_or(0.0);
                self.position_gauge
                    .with_label_values(&[label, &pos.symbol])
                    .set(notional);
            }
            _ => {}
        }
    }
}

struct PushMetrics;

impl Message<PushMetrics> for FundingArbMetricsActor {
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
