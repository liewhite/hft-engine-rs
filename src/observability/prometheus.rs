//! Prometheus pushgateway 推送：`MetricsActor` 的周期快照可选外送。
//!
//! 选 push 而非 pull（/metrics HTTP 端点）：不引入 HTTP server 依赖，且引擎可能跑在
//! 无入站网络的环境。文本用 Prometheus exposition format，端点为
//! `{url}/metrics/job/{job}`。

use std::fmt::Write as _;
use std::time::Duration;

/// 指标推送配置
#[derive(Debug, Clone)]
pub struct MetricsPushConfig {
    /// pushgateway 基址（如 `http://localhost:9091`）
    pub url: String,
    /// job 标签
    pub job: String,
}

impl MetricsPushConfig {
    /// 从环境变量装配：`PUSHGATEWAY_URL` 未设置即关闭（None）；
    /// `PUSHGATEWAY_JOB` 缺省 `hft_engine`
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("PUSHGATEWAY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let job = std::env::var("PUSHGATEWAY_JOB").unwrap_or_else(|_| "hft_engine".to_string());
        Some(Self { url, job })
    }
}

/// Prometheus 文本构建器（只支持 gauge —— 快照式指标全部是 gauge；
/// 累计量如 fills 也以 gauge 形式推送，因为进程重启后从零重计，counter 语义不成立）
#[derive(Default)]
pub struct PromText(String);

impl PromText {
    /// 追加一行 `name{labels} value`
    pub fn gauge(&mut self, name: &str, labels: &[(&str, &str)], value: f64) {
        let _ = write!(self.0, "{name}");
        if !labels.is_empty() {
            let _ = write!(self.0, "{{");
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    let _ = write!(self.0, ",");
                }
                // label 值转义：反斜杠、双引号、换行
                let escaped = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                let _ = write!(self.0, "{k}=\"{escaped}\"");
            }
            let _ = write!(self.0, "}}");
        }
        let _ = writeln!(self.0, " {value}");
    }

    pub fn into_body(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 异步推送一份快照（spawn，不阻塞调用方；失败只记 warn —— 观测出口绝不拖垮引擎）
pub fn spawn_push(config: MetricsPushConfig, body: String) {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    tokio::spawn(async move {
        let client = CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("默认 reqwest client 构建不该失败")
        });
        let endpoint = format!("{}/metrics/job/{}", config.url.trim_end_matches('/'), config.job);
        match client.post(&endpoint).body(body).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), %endpoint, "pushgateway 拒绝了本次推送");
            }
            Err(e) => {
                tracing::warn!(error = %e, %endpoint, "pushgateway 推送失败");
            }
            Ok(_) => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_lines_are_exposition_format() {
        let mut t = PromText::default();
        t.gauge("hft_equity", &[("exchange", "Binance")], 1234.5);
        t.gauge("hft_fills", &[], 42.0);
        t.gauge("hft_x", &[("a", "q\"uo\\te")], 1.0);
        let body = t.into_body();
        assert_eq!(
            body,
            "hft_equity{exchange=\"Binance\"} 1234.5\nhft_fills 42\nhft_x{a=\"q\\\"uo\\\\te\"} 1\n"
        );
    }
}
