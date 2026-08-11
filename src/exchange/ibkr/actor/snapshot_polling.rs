//! IbkrSnapshotPollingActor - 定时轮询 IBKR 借券费/可借量 + 现汇汇率
//!
//! 与 account_polling / status_polling 同范式：IbkrActor 的 spawn_link 子 actor，定时拉取——
//! 借券费/可借量走 `/iserver/marketdata/snapshot`（按 config 字段 tag + borrow_symbol conid），
//! 汇率走专用端点 `/iserver/exchangerate`（按 config 的 source/target 货币）——组装成
//! `BorrowFee`/`ExchangeRate` 直接 Publish 到 market_pubsub（策略与监控经同一 income 流消费）。
//!
//! **没有兜底常量、绝不用假数据**：拉取/解析失败即 ERROR；借券费或汇率距上次成功更新超过
//! `max_staleness_ms`（含启动宽限）即判定数据源失效 → `kill()` 自己 → 经 IbkrActor 的
//! link 监管上抛 → ManagerActor `on_link_died` → 整个系统致命退出（宁可重启，绝不糊弄）。

use crate::domain::{now_ms, BorrowFee, Exchange, ExchangeRate};
use crate::engine::MarketPubSub;
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::{MarketData, MarketEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

fn default_max_staleness_ms() -> u64 {
    300_000
}

/// IBKR snapshot 轮询配置：借券费/可借量/汇率的**唯一来源**，无兜底常量。
///
/// 字段 tag / conid 是 IB 账户/合约事实，由使用方填真值。启用后若拉不到或数据过期超过
/// `max_staleness_ms`，poller 致命退出（见模块文档）。
#[derive(Debug, Clone, Deserialize)]
pub struct IbkrSnapshotConfig {
    /// 拉券源读数的 base symbol（须在 IBKR symbols 内，用它解析 conid），如 "SKHY"
    pub borrow_symbol: String,
    /// snapshot 可借量字段 tag（如 "7636"）
    pub shortable_field: String,
    /// snapshot 借券费率字段 tag（如 "7637"，年化百分数）
    pub fee_field: String,
    /// 费率原值 × fee_scale = 年化小数（如 0.84 → ×0.01 = 0.0084）
    pub fee_scale: f64,
    /// 汇率目标货币（`/iserver/exchangerate` 的 target，即计价货币，如 "KRW"）
    pub fx_target: String,
    /// 汇率源货币（`/iserver/exchangerate` 的 source，即基准货币，如 "USD"）
    pub fx_source: String,
    /// 轮询间隔 (ms)
    pub poll_interval_ms: u64,
    /// 数据最大过期时间 (ms)：借券费或汇率距上次成功更新超过此值即致命退出（含启动宽限）。默认 300s。
    #[serde(default = "default_max_staleness_ms")]
    pub max_staleness_ms: u64,
}

/// 初始化参数
pub struct IbkrSnapshotPollingActorArgs {
    pub client: Arc<IbkrClient>,
    pub market_pubsub: ActorRef<MarketPubSub>,
    pub cfg: IbkrSnapshotConfig,
}

/// IbkrSnapshotPollingActor
pub struct IbkrSnapshotPollingActor {
    client: Arc<IbkrClient>,
    market_pubsub: ActorRef<MarketPubSub>,
    cfg: IbkrSnapshotConfig,
    /// borrow_symbol 解析到的 conid（on_start 解析，解析不到即致命）
    borrow_conid: i64,
    /// actor 启动时刻（用于启动宽限内的过期判定）
    started_ms: u64,
    /// 借券费/汇率最近一次成功更新时刻（None = 启动后尚未成功）
    last_borrow_ok_ms: Option<u64>,
    last_fx_ok_ms: Option<u64>,
    /// 最近一次 snapshot 观测到的借券行情是否处于冻结态（美股休市，可借量 7636 不返回）。
    /// 冻结态下豁免借券腿的新鲜度校验（见 `is_stale`）；未知可用性时保守视为非冻结（强制新鲜）。
    borrow_frozen: bool,
}

impl IbkrSnapshotPollingActor {
    /// 拉借券费 + 可借量 → BorrowFee 事件；成功则更新 last_borrow_ok_ms
    async fn poll_borrow(&mut self) {
        match self
            .client
            .fetch_snapshot_raw(
                self.borrow_conid,
                &[
                    self.cfg.shortable_field.as_str(),
                    self.cfg.fee_field.as_str(),
                    MD_AVAILABILITY_FIELD,
                ],
                // 必需字段只声明费率：活市与冻结态都返回它；可借量在冻结态本就不返回，由下面的
                // borrow_frozen 豁免处理。**不能把 6509 算进必需**——IBKR 总会返回它，那样重试
                // 判据恒真、空壳会被当成拿到 (曾因此每轮报"字段缺失")。
                &[self.cfg.fee_field.as_str()],
            )
            .await
        {
            Ok(obj) => {
                // 先记录冻结态：美股休市时可借量(7636)不返回，属预期缺口而非数据源失效。
                self.borrow_frozen = is_market_frozen(&obj);
                let shortable = parse_field(&obj, &self.cfg.shortable_field);
                let fee_raw = parse_field(&obj, &self.cfg.fee_field);
                match (shortable, fee_raw) {
                    // 有效性校验：借券费/可借量须有限非负。可解析但无效(NaN/负)**不算成功**——
                    // 否则 poller 自认健康永不 kill，而下游拿到无效值只能丢弃 → 静默降级。
                    (Some(sh), Some(fe))
                        if sh.is_finite() && sh >= 0.0 && fe.is_finite() && fe >= 0.0 =>
                    {
                        let ts = now_ms();
                        self.publish(MarketData::BorrowFee(BorrowFee {
                            exchange: Exchange::IBKR,
                            symbol: self.cfg.borrow_symbol.clone(),
                            fee_annual: fe * self.cfg.fee_scale,
                            shortable_shares: sh,
                            timestamp: ts,
                        }))
                        .await;
                        self.last_borrow_ok_ms = Some(ts);
                    }
                    (Some(sh), Some(fe)) => tracing::error!(
                        shortable = sh,
                        fee = fe,
                        "IBKR 券源 snapshot 值无效 (shortable/fee 应为有限非负)，不计入成功"
                    ),
                    // 冻结态豁免：休市时可借量本就不返回，沿用最后已知值、不当失败（不触发致命）。
                    _ if self.borrow_frozen => tracing::info!(
                        obj = %obj,
                        "IBKR 券源冻结态 (6509=Z/Y)：可借量休市不返回，豁免新鲜度校验，沿用最后已知值"
                    ),
                    _ => tracing::error!(
                        obj = %obj,
                        "IBKR 券源 snapshot 字段解析失败 (shortable/fee 缺失)"
                    ),
                }
            }
            Err(e) => {
                // 拉取失败无法确认是否冻结。真正的冻结态一定返回 Ok(obj)（请求带 6509，ready 必真），
                // 故 Err 绝不代表「仍在冻结」——保守置非冻结，避免旧冻结值掩盖活市期借券端点单独断源。
                self.borrow_frozen = false;
                tracing::error!(error = %e, "IBKR 券源 snapshot 拉取失败");
            }
        }
    }

    /// 直读现汇参考汇率 (source→target) → ExchangeRate 事件；成功则更新 last_fx_ok_ms。
    ///
    /// 用专用汇率端点而非 snapshot last：受限货币休市/冻结时 last 为空、bid/ask 不返回，
    /// 只有 `/iserver/exchangerate` 盘外仍给参考汇率——否则 7×24 常驻每逢休市即被 is_stale 误杀。
    async fn poll_fx(&mut self) {
        match self
            .client
            .fetch_exchange_rate(&self.cfg.fx_target, &self.cfg.fx_source)
            .await
        {
            // 有效性校验：汇率须为正(与下游 er.rate>0 采纳条件一致)。拿到但无效不算成功。
            Ok(rate) if rate.is_finite() && rate > 0.0 => {
                let ts = now_ms();
                self.publish(MarketData::ExchangeRate(ExchangeRate {
                    exchange: Exchange::IBKR,
                    base: self.cfg.fx_source.clone(),
                    quote: self.cfg.fx_target.clone(),
                    rate,
                    timestamp: ts,
                }))
                .await;
                self.last_fx_ok_ms = Some(ts);
            }
            Ok(rate) => tracing::error!(rate, "IBKR 汇率值无效 (应为正)，不计入成功"),
            Err(e) => tracing::error!(error = %e, "IBKR 汇率拉取失败"),
        }
    }

    async fn publish(&self, data: MarketData) {
        let ts = now_ms();
        if let Err(e) = self
            .market_pubsub
            .tell(Publish(MarketEvent {
                exchange_ts: ts,
                local_ts: ts,
                data,
            }))
            .send()
            .await
        {
            tracing::error!(error = %e, "Failed to publish IBKR snapshot event to MarketPubSub");
        }
    }

    /// 过期判定：借券费或汇率距上次成功更新（未成功过则从启动时刻算）超过 max_staleness_ms
    /// 即返回 true（数据源失效）。
    fn is_stale(&self, now: u64) -> bool {
        eval_stale(
            now,
            self.last_borrow_ok_ms,
            self.last_fx_ok_ms,
            self.started_ms,
            self.borrow_frozen,
            self.cfg.max_staleness_ms,
        )
    }
}

impl Actor for IbkrSnapshotPollingActor {
    type Args = IbkrSnapshotPollingActorArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 配置即时校验：解析不到 borrow conid、或 fx_target/fx_source 未配置 → 致命（不静默降级）
        let borrow_conid = args.client.conid_of(&args.cfg.borrow_symbol).ok_or_else(|| {
            anyhow::anyhow!(
                "IBKR snapshot poller: borrow_symbol '{}' 无法解析到 conid",
                args.cfg.borrow_symbol
            )
        })?;
        if args.cfg.fx_target.trim().is_empty() || args.cfg.fx_source.trim().is_empty() {
            anyhow::bail!("IBKR snapshot poller: fx_target/fx_source 未配置 (不能为空)");
        }
        // 轮询间隔必须显著小于过期阈值，否则一 tick 就过期 → 必然重启循环
        if args.cfg.poll_interval_ms >= args.cfg.max_staleness_ms {
            anyhow::bail!(
                "IBKR snapshot poller: poll_interval_ms ({}) 必须 < max_staleness_ms ({})",
                args.cfg.poll_interval_ms,
                args.cfg.max_staleness_ms
            );
        }

        let interval = Duration::from_millis(args.cfg.poll_interval_ms.max(1_000));
        actor_ref.attach_stream(IntervalStream::new(tokio::time::interval(interval)), (), ());

        tracing::info!(
            exchange = "IBKR",
            borrow_symbol = %args.cfg.borrow_symbol,
            borrow_conid,
            fx_source = %args.cfg.fx_source,
            fx_target = %args.cfg.fx_target,
            interval_ms = interval.as_millis() as u64,
            max_staleness_ms = args.cfg.max_staleness_ms,
            "IbkrSnapshotPollingActor started"
        );

        Ok(Self {
            client: args.client,
            market_pubsub: args.market_pubsub,
            cfg: args.cfg,
            borrow_conid,
            started_ms: now_ms(),
            last_borrow_ok_ms: None,
            last_fx_ok_ms: None,
            borrow_frozen: false,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "IbkrSnapshotPollingActor stopped");
        Ok(())
    }
}

impl Message<StreamMessage<Instant, (), ()>> for IbkrSnapshotPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_borrow().await;
                self.poll_fx().await;
                // 过期即致命：kill 自己 → IbkrActor.on_link_died → ManagerActor 整机退出
                let now = now_ms();
                if self.is_stale(now) {
                    tracing::error!(
                        max_staleness_ms = self.cfg.max_staleness_ms,
                        last_borrow_ok_ms = ?self.last_borrow_ok_ms,
                        last_fx_ok_ms = ?self.last_fx_ok_ms,
                        "IBKR 借券费/汇率数据源失效 (超过 max_staleness)，poller 致命退出"
                    );
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Started(_) => {}
            StreamMessage::Finished(_) => {
                tracing::error!("IBKR snapshot polling stream 意外结束，poller 致命退出");
                ctx.actor_ref().kill();
            }
        }
    }
}

/// 过期判定（纯函数，便于单测）：fx 任一超期即致命；借券腿仅在**非冻结**时计入过期。
///
/// fx 优先判定，且冻结豁免只作用于借券腿——故 fx 超期即便借券处于冻结豁免中仍会致命
/// （fx 走独立端点、盘外亦可得，无豁免理由）。
///
/// **冻结豁免依赖一条跨层不变量**：无任何入场逻辑消费冻结期的借券数据（策略入场只在活市发生，
/// 届时 `borrow_frozen=false`、可借量正常返回）。若将来策略把 `shortable_shares` 纳入入场门控，
/// 冻结期沿用的旧值会悄悄喂给决策，此豁免须重估。
fn eval_stale(
    now: u64,
    last_borrow_ok: Option<u64>,
    last_fx_ok: Option<u64>,
    started: u64,
    borrow_frozen: bool,
    max_staleness_ms: u64,
) -> bool {
    let fx_ref = last_fx_ok.unwrap_or(started);
    if now.saturating_sub(fx_ref) > max_staleness_ms {
        return true;
    }
    if borrow_frozen {
        return false;
    }
    let borrow_ref = last_borrow_ok.unwrap_or(started);
    now.saturating_sub(borrow_ref) > max_staleness_ms
}

/// IBKR 行情可用性字段 tag（协议常量，非账户/合约事实）。首字母表征数据类型：
/// R=实时 / D=延迟 / Z=冻结(收盘最后值) / Y=冻结延迟 / N=未订阅。
///
/// `pub(crate)`：ws 订阅的字段并集也要带上它（见 `IbkrPublicWsActorArgs::extra_md_fields`），
/// 否则刷新后该 conid 的字段集里没有 6509，本 poller 就判不出冻结态。
pub(crate) const MD_AVAILABILITY_FIELD: &str = "6509";

/// 判断 snapshot 是否处于冻结态（美股休市，只余最后值，可借量 7636 不返回）。
/// 6509 首字母 Z/Y 视为冻结；字段缺失（无法判定可用性）时保守返回 false → 强制新鲜（fail-safe）。
fn is_market_frozen(obj: &serde_json::Value) -> bool {
    obj.get(MD_AVAILABILITY_FIELD)
        .and_then(|v| v.as_str())
        .and_then(|s| s.chars().next())
        .map(|c| c == 'Z' || c == 'Y')
        .unwrap_or(false)
}

/// 解析 snapshot 字段为 f64：支持数字或字符串 (剥离 %、逗号等非数字字符)
fn parse_field(obj: &serde_json::Value, field: &str) -> Option<f64> {
    let v = obj.get(field)?;
    if let Some(f) = v.as_f64() {
        return Some(f);
    }
    if let Some(s) = v.as_str() {
        let cleaned: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        return cleaned.parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_field_number_and_string() {
        let obj = json!({"7636": 5000000.0, "7637": "0.84%", "31": "1,479.6"});
        assert_eq!(parse_field(&obj, "7636"), Some(5_000_000.0));
        assert!((parse_field(&obj, "7637").unwrap() - 0.84).abs() < 1e-9);
        assert!((parse_field(&obj, "31").unwrap() - 1479.6).abs() < 1e-9);
        assert_eq!(parse_field(&obj, "999"), None);
    }

    #[test]
    fn frozen_detection_by_6509() {
        // 冻结态（Z/Y）→ true；实时/延迟/未订阅 → false
        assert!(is_market_frozen(&json!({"6509": "ZB"})));
        assert!(is_market_frozen(&json!({"6509": "Y"})));
        assert!(!is_market_frozen(&json!({"6509": "RB"})));
        assert!(!is_market_frozen(&json!({"6509": "DP"})));
        assert!(!is_market_frozen(&json!({"6509": "N"})));
        // 字段缺失 → 保守视为非冻结（强制新鲜）
        assert!(!is_market_frozen(&json!({"7637": "0.60%"})));
    }

    // eval_stale 决策覆盖：now/started 用绝对毫秒，max=300s。
    const MAX: u64 = 300_000;
    const T0: u64 = 1_000_000_000;

    #[test]
    fn stale_when_borrow_overdue_and_live() {
        // 活市：借券超期 → 致命
        let now = T0 + MAX + 1;
        assert!(eval_stale(now, Some(T0), Some(now), T0, false, MAX));
    }

    #[test]
    fn not_stale_when_borrow_overdue_but_frozen() {
        // 冻结豁免：借券超期但冻结 → 不致命（沿用最后已知值）
        let now = T0 + MAX + 1;
        assert!(!eval_stale(now, Some(T0), Some(now), T0, true, MAX));
    }

    #[test]
    fn fx_overdue_kills_even_when_frozen() {
        // canary：fx 超期即便借券冻结豁免仍致命（fx 无豁免理由）
        let now = T0 + MAX + 1;
        assert!(eval_stale(now, Some(now), Some(T0), T0, true, MAX));
    }

    #[test]
    fn frozen_then_borrow_endpoint_fails_in_live_still_kills() {
        // Critical 回归：冻结期后借券端点单独故障（Err 分支已置 borrow_frozen=false），
        // fx 仍健康，活市借券真实断源必须致命——否则借券 kill 线被静默绕过。
        let now = T0 + MAX + 1;
        assert!(eval_stale(now, Some(T0), Some(now), T0, /* borrow_frozen */ false, MAX));
    }

    #[test]
    fn fresh_within_window_not_stale() {
        let now = T0 + MAX / 2;
        assert!(!eval_stale(now, Some(T0), Some(T0), T0, false, MAX));
    }
}
