use std::time::Duration;
use tokio::time::sleep;

/// 重试配置
#[derive(Clone)]
pub struct RetryConfig {
    /// 初始重试间隔
    pub initial_interval: Duration,
    /// 最大重试间隔
    pub max_interval: Duration,
    /// 退避乘数
    pub multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_interval: Duration::from_secs(1),
            max_interval: Duration::from_secs(60),
            multiplier: 2.0,
        }
    }
}

/// 指数退避重试器
pub struct ExponentialBackoff {
    config: RetryConfig,
    current_interval: Duration,
    attempt: u32,
}

impl ExponentialBackoff {
    pub fn new(config: RetryConfig) -> Self {
        Self {
            current_interval: config.initial_interval,
            config,
            attempt: 0,
        }
    }

    /// 等待下一次重试
    pub async fn wait(&mut self) {
        tracing::warn!(
            attempt = self.attempt + 1,
            wait_secs = self.current_interval.as_secs(),
            "Waiting before reconnect"
        );

        sleep(self.current_interval).await;

        self.attempt += 1;
        self.current_interval = Duration::from_secs_f64(
            (self.current_interval.as_secs_f64() * self.config.multiplier)
                .min(self.config.max_interval.as_secs_f64())
        );
    }

    /// 重置退避状态 (连接成功后调用)
    pub fn reset(&mut self) {
        self.current_interval = self.config.initial_interval;
        self.attempt = 0;
    }
}

/// 解析消息，失败时 panic
///
/// 用于已知类型的消息解析，如果解析失败说明是逻辑漏洞
#[macro_export]
macro_rules! parse_or_panic {
    ($text:expr, $type:ty, $context:expr) => {{
        match serde_json::from_str::<$type>($text) {
            Ok(v) => v,
            Err(e) => {
                panic!(
                    "Failed to parse {} message: {}\nRaw: {}",
                    $context, e, $text
                );
            }
        }
    }};
}
