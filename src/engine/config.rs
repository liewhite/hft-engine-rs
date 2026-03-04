use serde::Deserialize;

/// 监控配置
#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringConfig {
    /// Prometheus Pushgateway URL
    pub pushgateway_url: String,
    /// Metric 前缀
    pub metric_prefix: String,
    /// 推送间隔（毫秒）
    pub push_interval_ms: u64,
    /// Slack channel
    pub slack_channel: String,
    /// Slack token
    pub slack_token: String,
}

/// 数据库配置
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
}
