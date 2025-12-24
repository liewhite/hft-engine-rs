use crate::config::MetricsConfig;
use crate::domain::Exchange;
use crate::exchange::{PrivateSinks, PublicSinks};
use prometheus::{GaugeVec, Opts, Registry};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const METRIC_PREFIX: &str = "funding_arb";

/// Prometheus metrics 集合
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    /// 资金费率 (labels: exchange, symbol)
    funding_rate: GaugeVec,
    /// USDT 余额 (labels: exchange)
    usdt_balance: GaugeVec,
    /// 仓位大小 (labels: exchange, symbol, side)
    position_size: GaugeVec,
}

impl Metrics {
    /// 创建 metrics
    pub fn new() -> Self {
        let registry = Registry::new();

        let funding_rate = GaugeVec::new(
            Opts::new(
                format!("{}_funding_rate", METRIC_PREFIX),
                "Funding rate for each exchange and symbol",
            ),
            &["exchange", "symbol"],
        )
        .expect("Failed to create funding_rate gauge");

        let usdt_balance = GaugeVec::new(
            Opts::new(
                format!("{}_usdt_balance", METRIC_PREFIX),
                "USDT balance for each exchange",
            ),
            &["exchange"],
        )
        .expect("Failed to create usdt_balance gauge");

        let position_size = GaugeVec::new(
            Opts::new(
                format!("{}_position_size", METRIC_PREFIX),
                "Position size for each exchange and symbol",
            ),
            &["exchange", "symbol", "side"],
        )
        .expect("Failed to create position_size gauge");

        registry
            .register(Box::new(funding_rate.clone()))
            .expect("Failed to register funding_rate");
        registry
            .register(Box::new(usdt_balance.clone()))
            .expect("Failed to register usdt_balance");
        registry
            .register(Box::new(position_size.clone()))
            .expect("Failed to register position_size");

        Self {
            registry,
            funding_rate,
            usdt_balance,
            position_size,
        }
    }

    /// 更新资金费率
    pub fn set_funding_rate(&self, exchange: Exchange, symbol: &str, rate: f64) {
        self.funding_rate
            .with_label_values(&[&exchange.to_string(), symbol])
            .set(rate);
    }

    /// 更新 USDT 余额
    pub fn set_usdt_balance(&self, exchange: Exchange, balance: f64) {
        self.usdt_balance
            .with_label_values(&[&exchange.to_string()])
            .set(balance);
    }

    /// 更新仓位大小
    pub fn set_position_size(&self, exchange: Exchange, symbol: &str, side: &str, size: f64) {
        self.position_size
            .with_label_values(&[&exchange.to_string(), symbol, side])
            .set(size);
    }

    /// 启动 metrics 订阅和 push 任务
    pub fn start(
        &self,
        config: &MetricsConfig,
        public_sinks: &[(Exchange, PublicSinks)],
        private_sinks: &[(Exchange, PrivateSinks)],
        cancel_token: CancellationToken,
    ) {
        if !config.enabled {
            tracing::info!("Metrics disabled");
            return;
        }

        // 启动订阅任务
        self.start_subscription(public_sinks, private_sinks, cancel_token.clone());

        // 启动 push 任务
        self.start_push(config, cancel_token);

        tracing::info!(
            pushgateway = %config.pushgateway_url,
            interval_secs = config.push_interval_secs,
            "Metrics started"
        );
    }

    /// 订阅数据流并更新 metrics
    fn start_subscription(
        &self,
        public_sinks: &[(Exchange, PublicSinks)],
        private_sinks: &[(Exchange, PrivateSinks)],
        cancel_token: CancellationToken,
    ) {
        // 订阅 public sinks (funding rate)
        for (exchange, sinks) in public_sinks {
            for (symbol, rx) in sinks.funding_rate_receivers() {
                let metrics = self.clone();
                let token = cancel_token.clone();
                let exchange = *exchange;
                let symbol = symbol.clone();
                tokio::spawn(async move {
                    let mut rx = rx;
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(rate) => {
                                        metrics.set_funding_rate(exchange, &symbol.to_string(), rate.rate);
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                }
                            }
                        }
                    }
                });
            }
        }

        // 订阅 private sinks (balance, position)
        for (exchange, sinks) in private_sinks {
            // Balance
            let metrics = self.clone();
            let token = cancel_token.clone();
            let exchange_clone = *exchange;
            let mut balance_rx = sinks.subscribe_balance();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        result = balance_rx.recv() => {
                            match result {
                                Ok(balance) => {
                                    if balance.asset == "USDT" {
                                        metrics.set_usdt_balance(exchange_clone, balance.available);
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            }
                        }
                    }
                }
            });

            // Positions
            for (symbol, rx) in sinks.position_receivers() {
                let metrics = self.clone();
                let token = cancel_token.clone();
                let exchange = *exchange;
                let symbol = symbol.clone();
                tokio::spawn(async move {
                    let mut rx = rx;
                    loop {
                        tokio::select! {
                            _ = token.cancelled() => break,
                            result = rx.recv() => {
                                match result {
                                    Ok(position) => {
                                        let side = match position.side {
                                            crate::domain::Side::Long => "long",
                                            crate::domain::Side::Short => "short",
                                        };
                                        metrics.set_position_size(exchange, &symbol.to_string(), side, position.size);
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    /// 定时 push 到 pushgateway
    fn start_push(&self, config: &MetricsConfig, cancel_token: CancellationToken) {
        let registry = self.registry.clone();
        let url = config.pushgateway_url.clone();
        let interval = Duration::from_secs(config.push_interval_secs);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(e) = push_metrics(&registry, &url).await {
                            tracing::warn!(error = %e, "Failed to push metrics to pushgateway");
                        }
                    }
                }
            }
        });
    }
}

/// Push metrics 到 pushgateway
async fn push_metrics(registry: &Registry, pushgateway_url: &str) -> Result<(), String> {
    use prometheus::Encoder;

    let metric_families = registry.gather();
    let mut buffer = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .map_err(|e| e.to_string())?;

    let url = format!("{}/metrics/job/{}", pushgateway_url, METRIC_PREFIX);

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("Content-Type", encoder.format_type())
        .body(buffer)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!(
            "Pushgateway returned status: {}",
            response.status()
        ));
    }

    Ok(())
}
