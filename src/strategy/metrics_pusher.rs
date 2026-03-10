//! MetricsPusher - Prometheus metrics 推送基础设施
//!
//! 各策略的 metrics actor 组合此结构体，获得 registry + 定时推送能力。

use prometheus::{Encoder, Registry, TextEncoder};

/// Prometheus metrics 推送器
///
/// 封装 Registry + HTTP Client + Pushgateway URL，
/// 各策略 metrics actor 通过组合（而非继承）复用推送逻辑。
pub struct MetricsPusher {
    /// Prometheus Registry（各 actor 在此注册自己的 gauges）
    pub registry: Registry,
    /// Pushgateway URL
    pushgateway_url: String,
    /// HTTP Client
    http_client: reqwest::Client,
    /// Job 名称（用于 Pushgateway URL path）
    job_name: String,
}

impl MetricsPusher {
    pub fn new(pushgateway_url: String, job_name: String) -> Self {
        Self {
            registry: Registry::new(),
            pushgateway_url,
            http_client: reqwest::Client::new(),
            job_name,
        }
    }

    /// 推送所有已注册的 metrics 到 Pushgateway
    pub async fn push(&self) {
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
