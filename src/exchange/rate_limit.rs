//! 各所 REST 限频的本地闸门：机制在这里，各所的额度数字和每个接口花多少写在各所适配层（#791）。
//!
//! 交易所的限额（按 IP 的权重、按账户的下单数、按品种的下单数……）一旦超了，轻则请求被拒，
//! 重则封 IP（币安连续超限会 418 封禁几分钟到几天）。此前适配层没有任何限流，靠上层（资金费
//! 轮询器、策略）按各所限额手算间隔 —— 换个策略、多加几个币就可能被封。现在每个出站 REST
//! 请求发出前都要先在本所的 [`RateLimiter`] 取到额度（[`crate::net::send`] 只收 [`Admitted`]），
//! 上层不用知道任何所的限额。
//!
//! 两种取额度的方式，由调用的适配层按请求性质选：
//! - **查询**（[`RateLimiter::query`]）：没额度就在本地排队等，等到有了再发。查询只能用到每个
//!   额度的 [`QUERY_SHARE_PCT`]%，剩下的留给下单撤单 —— 否则一轮集中的查询（如启动时拉齐资金费）
//!   会把共用的 IP 权重吃满，这时下单、补腿都会被本地拒掉。
//! - **交易动作**（[`RateLimiter::action`]，下单、撤单）：能用满额度；没额度当场返回
//!   [`Throttled`]，不排队 —— 排队会让下单晚到、价格过时。适配层把它翻成「被拒绝」（下单）或
//!   [`ExchangeError::RateLimited`]（撤单），上层按已有的拒单退避 / 撤单重试处理。
//!
//! 计数用滑动窗口：任意一段 `window` 长的时间内花掉的不超过上限。交易所按固定窗口（如每个
//! 自然分钟）计时，滑动窗口内的量总不小于任一同长固定窗口内的量，所以本地放行的交易所也会放行。
//! 各所额度照官方原值写，网络延迟抖动可能让交易所那边偶尔略超、回限频错误：下单按被拒走拒单退避，
//! 查询下一轮再来，不另加处理。
//!
//! 带签名的请求要先取额度再签名：查询可能排队，签名里的时间戳等久了会过期被拒。
//!
//! 只管本进程：同一 IP 上的其他进程、交易所侧的其他来源（网页端操作）都不计。

use crate::domain::{Exchange, ExchangeError, PlaceError};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

/// 查询最多用到每个额度的百分之几（余下留给下单撤单，见模块文档；至少 1）
pub(crate) const QUERY_SHARE_PCT: u32 = 80;

/// 一项限额：任意 `window` 时长内最多花 `cap`。由各所适配层声明成 `static`（计数按它的地址区分）。
#[derive(Debug)]
pub(crate) struct Quota {
    /// 日志里的名字
    name: &'static str,
    cap: u32,
    window: Duration,
}

impl Quota {
    pub(crate) const fn new(name: &'static str, cap: u32, window: Duration) -> Self {
        Self { name, cap, window }
    }

    /// 在这项额度上花 `cost`（整项共用，如按 IP、按账户的额度）
    pub(crate) fn cost(&'static self, cost: u32) -> Use<'static> {
        Use {
            quota: self,
            scope: "",
            cost,
        }
    }

    /// 在这项额度的 `scope` 分项上花 `cost`（每个分项各自计数，如 OKX 下单按品种分开计）
    pub(crate) fn cost_in<'a>(&'static self, scope: &'a str, cost: u32) -> Use<'a> {
        Use {
            quota: self,
            scope,
            cost,
        }
    }

    /// 计数的键：额度的地址（各所的额度都是 `static`，地址唯一，不靠名字不重复）
    fn key(&'static self) -> usize {
        self as *const Self as usize
    }

    fn limit(&self, kind: Kind) -> u32 {
        match kind {
            // 上限只有 1 的额度（IBKR 部分接口 5 秒 1 次）分不出余量，查询也能用
            Kind::Query => (self.cap * QUERY_SHARE_PCT / 100).max(1),
            Kind::Action => self.cap,
        }
    }
}

/// 一个请求在某项额度上的花费
#[derive(Debug, Clone, Copy)]
pub(crate) struct Use<'a> {
    quota: &'static Quota,
    scope: &'a str,
    cost: u32,
}

/// 已取到额度的凭据：[`crate::net::send`] 只收它，没取额度的交易所请求发不出去。
#[must_use = "交给 crate::net::send 发请求"]
#[derive(Debug)]
pub struct Admitted(());

/// 交易动作此刻没有额度（本地拒绝，请求没发出去）
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{exchange} 本地限流：{quota} 额度已用完，约 {retry_after:?} 后恢复")]
pub(crate) struct Throttled {
    pub(crate) exchange: Exchange,
    pub(crate) quota: String,
    pub(crate) retry_after: Duration,
}

/// 下单没额度 = 请求没发出去，确定没挂上：被拒绝（策略按拒单退避）
impl From<Throttled> for PlaceError {
    fn from(t: Throttled) -> Self {
        PlaceError::unsent(t)
    }
}

/// 撤单没额度：撤单失败（策略按撤单失败的间隔重撤）
impl From<Throttled> for ExchangeError {
    fn from(t: Throttled) -> Self {
        ExchangeError::new(t)
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Query,
    Action,
}

/// 一项额度（的一个分项）在窗口内的花费记录
#[derive(Debug, Default)]
struct Window {
    /// (花费时刻, 花费)，按时间先后
    spent: VecDeque<(Instant, u32)>,
    total: u32,
}

impl Window {
    fn prune(&mut self, now: Instant, window: Duration) {
        while let Some(&(at, cost)) = self.spent.front() {
            if now.duration_since(at) < window {
                break;
            }
            self.spent.pop_front();
            self.total -= cost;
        }
    }

    /// 再花 `cost` 后不超过 `limit` 的最早时刻（`None` = 现在就行）
    fn ready_at(&self, cost: u32, limit: u32, window: Duration) -> Option<Instant> {
        let excess = (self.total + cost).checked_sub(limit).filter(|e| *e > 0)?;
        let mut freed = 0;
        for &(at, c) in &self.spent {
            freed += c;
            if freed >= excess {
                return Some(at + window);
            }
        }
        unreachable!("cost ≤ limit 时全部到期一定腾得出来（调用方已先排除 cost > limit）")
    }
}

/// 一个所的限频闸门。各所一个进程级 `static`（IP、账户额度在进程内共用，同一所的多个 client
/// 和后台任务都要算进同一份）。
#[derive(Debug)]
pub(crate) struct RateLimiter {
    exchange: Exchange,
    /// 键：(额度的地址, 分项)
    windows: Mutex<HashMap<(usize, String), Window>>,
}

impl RateLimiter {
    pub(crate) fn new(exchange: Exchange) -> Self {
        Self {
            exchange,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// 查询取额度：没有就等（最长一个窗口）。
    pub(crate) async fn query(&self, uses: &[Use<'_>]) -> Result<Admitted, ExchangeError> {
        loop {
            let (ready_at, quota) = match self.take(Instant::now(), uses, Kind::Query) {
                Ok(admitted) => return Ok(admitted),
                Err(blocked) => blocked,
            };
            tracing::debug!(exchange = %self.exchange, quota, wait_ms = ready_at.duration_since(Instant::now()).as_millis() as u64, "查询等限频额度");
            tokio::time::sleep_until(ready_at).await;
        }
    }

    /// 交易动作（下单、撤单）取额度：没有就当场拒绝，不排队。
    pub(crate) fn action(&self, uses: &[Use<'_>]) -> Result<Admitted, Throttled> {
        let now = Instant::now();
        self.take(now, uses, Kind::Action)
            .map_err(|(at, quota)| Throttled {
                exchange: self.exchange,
                quota: quota.to_string(),
                retry_after: at.duration_since(now),
            })
    }

    /// 补记已经发生的花费，不等也不拒（花费要看回复大小才知道的接口，如 HL 按返回条数加收的权重）。
    /// 可能记过上限，之后的请求相应多等。
    pub(crate) fn record(&self, uses: &[Use<'_>]) {
        let now = Instant::now();
        let mut windows = self.windows.lock().expect("限频表锁被毒化");
        for u in uses.iter().filter(|u| u.cost > 0) {
            let w = windows
                .entry((u.quota.key(), u.scope.to_string()))
                .or_default();
            w.prune(now, u.quota.window);
            w.spent.push_back((now, u.cost));
            w.total += u.cost;
        }
    }

    /// 各项都够才一起扣；任一项不够就都不扣，返回最晚恢复的时刻和那项的名字。
    /// 单次花费超过上限（永远够不着）是各所把花费常量写错了：当场 panic，不伪装成限流。
    fn take(
        &self,
        now: Instant,
        uses: &[Use<'_>],
        kind: Kind,
    ) -> Result<Admitted, (Instant, &'static str)> {
        // 同一请求在同一项（同一分项）上的几笔花费合起来算
        let mut need: Vec<(&'static Quota, &str, u32)> = Vec::with_capacity(uses.len());
        for u in uses.iter().filter(|u| u.cost > 0) {
            match need
                .iter_mut()
                .find(|(q, s, _)| q.key() == u.quota.key() && *s == u.scope)
            {
                Some((_, _, c)) => *c += u.cost,
                None => need.push((u.quota, u.scope, u.cost)),
            }
        }
        let mut windows = self.windows.lock().expect("限频表锁被毒化");
        let mut blocked: Option<(Instant, &'static str)> = None;
        for &(quota, scope, cost) in &need {
            let limit = quota.limit(kind);
            assert!(
                cost <= limit,
                "{} 单次花费 {cost} 超过 {} 的上限 {limit}，永远发不出去（适配层花费写错了）",
                self.exchange,
                quota.name
            );
            let w = windows.entry((quota.key(), scope.to_string())).or_default();
            w.prune(now, quota.window);
            if let Some(at) = w.ready_at(cost, limit, quota.window) {
                if blocked.is_none_or(|(b, _)| at > b) {
                    blocked = Some((at, quota.name));
                }
            }
        }
        if let Some(blocked) = blocked {
            return Err(blocked);
        }
        for &(quota, scope, cost) in &need {
            let w = windows
                .get_mut(&(quota.key(), scope.to_string()))
                .expect("上面已建");
            w.spent.push_back((now, cost));
            w.total += cost;
        }
        Ok(Admitted(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);
    static IP: Quota = Quota::new("IP", 10, SEC);
    static ORDERS: Quota = Quota::new("ORDERS", 3, SEC);

    fn limiter() -> RateLimiter {
        RateLimiter::new(Exchange::Binance)
    }

    /// 交易动作能用满额度，用完当场拒绝并给出恢复时间；窗口滑过后恢复
    #[tokio::test(start_paused = true)]
    async fn action_uses_full_cap_then_throttles_until_window_slides() {
        let l = limiter();
        for _ in 0..3 {
            let _ = l.action(&[ORDERS.cost(1)]).unwrap();
        }
        let t = l.action(&[ORDERS.cost(1)]).unwrap_err();
        assert_eq!(t.quota, "ORDERS");
        assert_eq!(t.retry_after, SEC);
        tokio::time::advance(SEC / 2).await;
        l.action(&[ORDERS.cost(1)]).unwrap_err();
        tokio::time::advance(SEC / 2).await;
        let _ = l.action(&[ORDERS.cost(1)]).unwrap();
    }

    /// 查询只能用到八成，余下的交易动作还能用
    #[tokio::test(start_paused = true)]
    async fn query_leaves_headroom_for_actions() {
        let l = limiter();
        let _ = l.query(&[IP.cost(8)]).await.unwrap();
        let start = Instant::now();
        // 第 9 个权重要等最早那笔滑出窗口
        let _ = l.query(&[IP.cost(1)]).await.unwrap();
        assert_eq!(Instant::now() - start, SEC);
        // 查询已占 1（新窗口），动作还能花到 10
        let _ = l.action(&[IP.cost(9)]).unwrap();
        l.action(&[IP.cost(1)]).unwrap_err();
    }

    /// 多项一起扣：任一项不够就都不扣
    #[tokio::test(start_paused = true)]
    async fn all_or_nothing_across_quotas() {
        let l = limiter();
        for _ in 0..3 {
            let _ = l.action(&[IP.cost(1), ORDERS.cost(1)]).unwrap();
        }
        let t = l.action(&[IP.cost(1), ORDERS.cost(1)]).unwrap_err();
        assert_eq!(t.quota, "ORDERS");
        // IP 没被扣：还剩 7
        let _ = l.action(&[IP.cost(7)]).unwrap();
        l.action(&[IP.cost(1)]).unwrap_err();
    }

    /// 分项各自计数；同一请求里同一项的几笔合起来算
    #[tokio::test(start_paused = true)]
    async fn scopes_count_separately_and_same_quota_costs_add_up() {
        let l = limiter();
        for _ in 0..3 {
            let _ = l.action(&[ORDERS.cost_in("BTC", 1)]).unwrap();
        }
        l.action(&[ORDERS.cost_in("BTC", 1)]).unwrap_err();
        let _ = l.action(&[ORDERS.cost_in("ETH", 1)]).unwrap();
        // 2 + 2 > 3 - 1（ETH 已花 1）
        l.action(&[ORDERS.cost_in("ETH", 1), ORDERS.cost_in("ETH", 2)])
            .unwrap_err();
        let _ = l.action(&[ORDERS.cost_in("ETH", 2)]).unwrap();
    }

    /// 补记的花费让后面的查询多等
    #[tokio::test(start_paused = true)]
    async fn record_delays_later_queries() {
        let l = limiter();
        l.record(&[IP.cost(12)]);
        let start = Instant::now();
        let _ = l.query(&[IP.cost(1)]).await.unwrap();
        assert_eq!(Instant::now() - start, SEC);
    }

    /// 单次花费超上限是花费常量写错了：当场 panic，不伪装成限流
    #[test]
    #[should_panic(expected = "永远发不出去")]
    fn oversized_cost_panics() {
        let _ = limiter().action(&[IP.cost(11)]);
    }

    /// 同名的两项额度各自计数（按地址区分）
    #[test]
    fn same_name_quotas_count_separately() {
        static A: Quota = Quota::new("X", 1, SEC);
        static B: Quota = Quota::new("X", 1, SEC);
        let l = limiter();
        let _ = l.action(&[A.cost(1)]).unwrap();
        let _ = l.action(&[B.cost(1)]).unwrap();
    }

    /// 上限只有 1 的额度查询也能用（分不出余量），用过之后等满一个窗口
    #[tokio::test(start_paused = true)]
    async fn single_slot_quota_is_usable_by_queries() {
        static ONE: Quota = Quota::new("ONE", 1, SEC);
        let l = limiter();
        let start = Instant::now();
        let _ = l.query(&[ONE.cost(1)]).await.unwrap();
        let _ = l.query(&[ONE.cost(1)]).await.unwrap();
        assert_eq!(Instant::now() - start, SEC);
    }

    /// 排队的查询按各自需要等到有额度为止，不超窗口上限
    #[tokio::test(start_paused = true)]
    async fn concurrent_queries_never_exceed_the_window() {
        let l = std::sync::Arc::new(limiter());
        let start = Instant::now();
        let done: Vec<_> = (0..20)
            .map(|_| {
                let l = l.clone();
                tokio::spawn(async move {
                    let _ = l.query(&[IP.cost(1)]).await.unwrap();
                    Instant::now() - start
                })
            })
            .collect();
        let mut at = Vec::new();
        for d in done {
            at.push(d.await.unwrap());
        }
        at.sort();
        // 每秒最多 8 个
        assert_eq!(at.iter().filter(|t| **t < SEC).count(), 8);
        assert_eq!(at.iter().filter(|t| **t < 2 * SEC).count(), 16);
        assert_eq!(*at.last().unwrap(), 2 * SEC);
    }
}
