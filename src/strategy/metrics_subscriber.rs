//! MetricsSubscriberActor - 订阅 Income 事件，更新 Prometheus metrics 并推送到 Pushgateway
//!
//! 职责：
//! - 订阅 Equity 和 Position 事件
//! - 维护 Gauge metrics（equity 按 exchange 标签，position 按 exchange + symbol 标签）
//! - 定时推送 metrics 到 Pushgateway

use crate::domain::{Exchange, Symbol};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use prometheus::{Encoder, GaugeVec, Opts, Registry, TextEncoder};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::interval;

/// 跨交易所价差对配置
///
/// 用于计算 spread: open = (perp_bid - spot_ask) / spot_ask,
///                   close = (perp_ask - spot_bid) / spot_bid
pub struct SpreadPairConfig {
    /// 现货侧交易所 (e.g., IBKR)
    pub spot_exchange: Exchange,
    pub spot_symbol: Symbol,
    /// 永续侧交易所 (e.g., Hyperliquid)
    pub perp_exchange: Exchange,
    pub perp_symbol: Symbol,
}

/// MetricsSubscriberActor 初始化参数
pub struct MetricsSubscriberArgs {
    /// Pushgateway URL
    pub pushgateway_url: String,
    /// Metric 前缀
    pub metric_prefix: String,
    /// 推送间隔（毫秒）
    pub push_interval_ms: u64,
    /// 可选：价差对配置（用于 spread_pct 和 quote 指标）
    pub spread_pairs: Vec<SpreadPairConfig>,
}

/// MetricsSubscriberActor - 订阅 Income 事件，推送 metrics 到 Pushgateway
pub struct MetricsSubscriberActor {
    /// Prometheus Registry
    registry: Registry,
    /// Equity Gauge (labels: exchange)
    equity_gauge: GaugeVec,
    /// Notional Gauge (labels: exchange)
    notional_gauge: GaugeVec,
    /// Leverage Gauge (labels: exchange)
    leverage_gauge: GaugeVec,
    /// Position Gauge (labels: exchange, symbol)
    position_gauge: GaugeVec,
    /// Quote Gauge (labels: exchange, symbol, side)
    quote_gauge: Option<GaugeVec>,
    /// Spread Gauge (labels: symbol, type)
    spread_pct_gauge: Option<GaugeVec>,
    /// Pushgateway URL
    pushgateway_url: String,
    /// HTTP Client
    http_client: reqwest::Client,
    /// Job 名称（用于 Pushgateway）
    job_name: String,
    /// 价格缓存 (用于计算 position notional)
    price_cache: HashMap<(Exchange, Symbol), f64>,
    /// BBO 缓存 (exchange, symbol) → (bid, ask)，用于 quote 和 spread 计算
    bbo_cache: HashMap<(Exchange, Symbol), (f64, f64)>,
    /// 价差对配置
    spread_pairs: Vec<SpreadPairConfig>,
}

impl MetricsSubscriberActor {
    /// 推送 metrics 到 Pushgateway
    async fn push_metrics(&self) {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();

        if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
            tracing::error!(error = %e, "Failed to encode metrics");
            return;
        }

        let url = format!(
            "{}/metrics/job/{}",
            self.pushgateway_url.trim_end_matches('/'),
            self.job_name
        );

        match self
            .http_client
            .post(&url)
            .header("Content-Type", encoder.format_type())
            .body(buffer)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Metrics pushed successfully");
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "Pushgateway returned non-success status");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to push metrics to Pushgateway");
            }
        }
    }
}

impl Actor for MetricsSubscriberActor {
    type Args = MetricsSubscriberArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let registry = Registry::new();

        // 创建 equity gauge (labels: exchange)
        let equity_opts = Opts::new(
            format!("{}_equity", args.metric_prefix),
            "Account equity by exchange",
        );
        let equity_gauge = GaugeVec::new(equity_opts, &["exchange"])?;
        registry.register(Box::new(equity_gauge.clone()))?;

        // 创建 notional gauge (labels: exchange)
        let notional_opts = Opts::new(
            format!("{}_notional", args.metric_prefix),
            "Account total position notional by exchange",
        );
        let notional_gauge = GaugeVec::new(notional_opts, &["exchange"])?;
        registry.register(Box::new(notional_gauge.clone()))?;

        // 创建 leverage gauge (labels: exchange)
        let leverage_opts = Opts::new(
            format!("{}_leverage", args.metric_prefix),
            "Account leverage (notional/equity) by exchange",
        );
        let leverage_gauge = GaugeVec::new(leverage_opts, &["exchange"])?;
        registry.register(Box::new(leverage_gauge.clone()))?;

        // 创建 position gauge (labels: exchange, symbol)
        let position_opts = Opts::new(
            format!("{}_position", args.metric_prefix),
            "Position notional (size * price) by exchange and symbol",
        );
        let position_gauge = GaugeVec::new(position_opts, &["exchange", "symbol"])?;
        registry.register(Box::new(position_gauge.clone()))?;

        // 有 spread pairs 时创建 quote 和 spread_pct gauges
        let has_spread_pairs = !args.spread_pairs.is_empty();
        let quote_gauge = if has_spread_pairs {
            let opts = Opts::new(
                format!("{}_quote", args.metric_prefix),
                "BBO quote price by exchange, symbol, and side",
            );
            let gauge = GaugeVec::new(opts, &["exchange", "symbol", "side"])?;
            registry.register(Box::new(gauge.clone()))?;
            Some(gauge)
        } else {
            None
        };

        let spread_pct_gauge = if has_spread_pairs {
            let opts = Opts::new(
                format!("{}_spread_pct", args.metric_prefix),
                "Cross-exchange spread percentage by symbol and type",
            );
            let gauge = GaugeVec::new(opts, &["symbol", "type"])?;
            registry.register(Box::new(gauge.clone()))?;
            Some(gauge)
        } else {
            None
        };

        let pushgateway_url = args.pushgateway_url.clone();
        let push_interval_ms = args.push_interval_ms;

        let actor = Self {
            registry,
            equity_gauge,
            notional_gauge,
            leverage_gauge,
            position_gauge,
            quote_gauge,
            spread_pct_gauge,
            pushgateway_url,
            http_client: reqwest::Client::new(),
            job_name: args.metric_prefix.clone(),
            price_cache: HashMap::new(),
            bbo_cache: HashMap::new(),
            spread_pairs: args.spread_pairs,
        };

        // 启动定时推送任务
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
            push_interval_ms = push_interval_ms,
            "MetricsSubscriberActor started"
        );

        Ok(actor)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "MetricsSubscriberActor stopped");
        Ok(())
    }
}

// ============================================================================
// Messages
// ============================================================================

/// 处理 Income 事件
impl Message<IncomeEvent> for MetricsSubscriberActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match &msg.data {
            ExchangeEventData::AccountInfo {
                exchange,
                equity,
                notional,
            } => {
                let exchange_label = exchange_to_label(*exchange);

                self.equity_gauge
                    .with_label_values(&[exchange_label])
                    .set(*equity);

                self.notional_gauge
                    .with_label_values(&[exchange_label])
                    .set(*notional);

                // 计算杠杆率 (notional / equity)
                let leverage = if *equity > 0.0 {
                    notional / equity
                } else {
                    0.0
                };
                self.leverage_gauge
                    .with_label_values(&[exchange_label])
                    .set(leverage);

                tracing::debug!(
                    exchange = %exchange_label,
                    equity = %equity,
                    notional = %notional,
                    leverage = %format!("{:.2}", leverage),
                    "Updated account metrics"
                );
            }
            ExchangeEventData::BBO(bbo) => {
                // 更新价格缓存（中间价）
                let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;
                self.price_cache
                    .insert((bbo.exchange, bbo.symbol.clone()), mid_price);

                // 更新 BBO 缓存并刷新 quote + spread 指标
                self.bbo_cache.insert(
                    (bbo.exchange, bbo.symbol.clone()),
                    (bbo.bid_price, bbo.ask_price),
                );

                if let Some(ref quote_gauge) = self.quote_gauge {
                    let ex = exchange_to_label(bbo.exchange);
                    quote_gauge
                        .with_label_values(&[ex, &bbo.symbol, "bid"])
                        .set(bbo.bid_price);
                    quote_gauge
                        .with_label_values(&[ex, &bbo.symbol, "ask"])
                        .set(bbo.ask_price);
                }

                if let Some(ref spread_gauge) = self.spread_pct_gauge {
                    for pair in &self.spread_pairs {
                        // 只在当前 BBO 属于该 pair 时重新计算
                        let is_spot =
                            bbo.exchange == pair.spot_exchange && bbo.symbol == pair.spot_symbol;
                        let is_perp =
                            bbo.exchange == pair.perp_exchange && bbo.symbol == pair.perp_symbol;
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
                            // open_spread = (perp_bid - spot_ask) / spot_ask
                            if spot_ask > 0.0 {
                                let open = (perp_bid - spot_ask) / spot_ask * 100.0;
                                spread_gauge
                                    .with_label_values(&[pair.spot_symbol.as_str(), "open"])
                                    .set(open);
                            }
                            // close_spread = (perp_ask - spot_bid) / spot_bid
                            if spot_bid > 0.0 {
                                let close = (perp_ask - spot_bid) / spot_bid * 100.0;
                                spread_gauge
                                    .with_label_values(&[pair.spot_symbol.as_str(), "close"])
                                    .set(close);
                            }
                        }
                    }
                }
            }
            ExchangeEventData::Position(position) => {
                let exchange_label = exchange_to_label(position.exchange);
                let symbol_label = &position.symbol;
                // 使用缓存的中间价计算 notional
                let notional = self
                    .price_cache
                    .get(&(position.exchange, position.symbol.clone()))
                    .map(|price| position.size * price)
                    .unwrap_or(0.0);
                self.position_gauge
                    .with_label_values(&[exchange_label, symbol_label])
                    .set(notional);
            }
            _ => {
                // 忽略其他事件
            }
        }
    }
}

/// 定时推送 metrics
struct PushMetrics;

impl Message<PushMetrics> for MetricsSubscriberActor {
    type Reply = ();

    async fn handle(&mut self, _msg: PushMetrics, _ctx: &mut Context<Self, Self::Reply>) {
        self.push_metrics().await;
    }
}

/// 将 Exchange 枚举转换为标签字符串
fn exchange_to_label(exchange: Exchange) -> &'static str {
    match exchange {
        Exchange::Binance => "binance",
        Exchange::OKX => "okx",
        Exchange::Hyperliquid => "hyperliquid",
        Exchange::IBKR => "ibkr",
    }
}
