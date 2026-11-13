//! Prometheus pushgateway 推送：`Metrics` 的周期快照可选外送。
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
    /// `PUSHGATEWAY_JOB` 缺省为程序名。推送用 PUT 整组替换，两个进程同 job 会互相清空，
    /// 所以缺省不用固定名字：不同程序天然不撞，同一程序起多份时才需要各配一个
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("PUSHGATEWAY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let job = std::env::var("PUSHGATEWAY_JOB").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "hft_engine".to_string())
        });
        Some(Self { url, job })
    }
}

/// 一个带标签的瞬时读数。策略经 [`crate::strategy::Strategy::gauges`] 交出自己的读数时用它，
/// 框架负责送进指标出口（与 [`PromText`] 的一行一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub struct Gauge {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Gauge {
    /// 名字与标签名是否合 Prometheus 文本格式、且不占用框架的 `hft_` 前缀。
    /// 外部（策略）交来的读数推送前过这一关：pushgateway 整份收或整份拒，一行坏的会连带
    /// 框架自己的指标一起推不出去，看板停在旧值上
    pub fn validate(&self) -> Result<(), String> {
        let ident = |s: &str, colon: bool| {
            let mut chars = s.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || (colon && c == ':'))
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || (colon && c == ':'))
        };
        if !ident(&self.name, true) {
            return Err(format!("指标名非法: {:?}", self.name));
        }
        if self.name.starts_with("hft_") {
            return Err(format!("{} 占用了框架的 hft_ 前缀", self.name));
        }
        if let Some((k, _)) = self.labels.iter().find(|(k, _)| !ident(k, false) || k.starts_with("__")) {
            return Err(format!("{} 的标签名非法: {k:?}", self.name));
        }
        Ok(())
    }

    pub fn new(name: impl Into<String>, labels: &[(&str, &str)], value: f64) -> Self {
        Self {
            name: name.into(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            value,
        }
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
        // 文本格式只认 NaN / +Inf / -Inf；Rust 写成 inf 会让整份推送被拒
        let _ = match value {
            v if v.is_nan() => writeln!(self.0, " NaN"),
            v if v == f64::INFINITY => writeln!(self.0, " +Inf"),
            v if v == f64::NEG_INFINITY => writeln!(self.0, " -Inf"),
            v => writeln!(self.0, " {v}"),
        };
    }

    /// 追加一个 [`Gauge`]，另加 `extra` 标签（如实例所属账户）。调用方须先用
    /// [`Gauge::validate`] 把关：一行非法会让 pushgateway 拒掉整份推送
    pub fn push(&mut self, g: &Gauge, extra: &[(&str, &str)]) {
        let labels: Vec<(&str, &str)> = g
            .labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(extra.iter().copied())
            .collect();
        self.gauge(&g.name, &labels, g.value);
    }

    pub fn into_body(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 异步推送一份快照（spawn，不阻塞调用方；失败只记 warn —— 观测出口绝不拖垮引擎）。
///
/// 用 PUT 整组替换：这一份就是本 job 的全部指标。POST 只替换同名指标，某个标签组合
/// 不再出现时（币平仓了、挂单没了）旧值会一直留在 pushgateway，看板与告警读到的是假的现状。
pub fn spawn_push(config: MetricsPushConfig, body: String) {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    tokio::spawn(async move {
        let client = CLIENT.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                // 不带 UA 的请求会被部分 CDN 拒
                .user_agent(concat!("hft-engine-rs/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("默认 reqwest client 构建不该失败")
        });
        let endpoint = format!("{}/metrics/job/{}", config.url.trim_end_matches('/'), config.job);
        match client.put(&endpoint).body(body).send().await {
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
        t.gauge("hft_inf", &[], f64::INFINITY);
        let body = t.into_body();
        assert_eq!(
            body,
            "hft_equity{exchange=\"Binance\"} 1234.5\nhft_fills 42\nhft_x{a=\"q\\\"uo\\\\te\"} 1\nhft_inf +Inf\n"
        );
    }
}
