//! 周期性读数的两条新鲜度规矩：**停摆守卫**（[`StalenessGuard`]，长期取不到即致命）与
//! **收到才盖戳**（[`stamped_on_receipt`]，时间戳不得早于响应）。
//!
//! 两者管的是同一件事的两面：前者防"陈旧读数冒充当前值"，后者防"读数的年龄被算错"。
//!
//! # 为什么需要停摆守卫
//!
//! REST 轮询失败时，消费方（`StateManager`）里的上一份读数**原样留着**：
//! `equity()` 依然返回 `Some(旧值)`，调用方无从分辨它是 3 秒前还是 3 小时前的。
//! 于是"取不到"被静默表达成了"还是老样子" —— 账户杠杆闸门可以拿着几小时前的净值
//! 一路放行。这与"某个字段可以没有，但不能是假的"直接冲突。
//!
//! 修法不是给每个读数附时效戳、也不是让每个消费点去判断新鲜度（那是层层设防），
//! 而是让**数据源自己**在停摆过久时死掉：读数流断了比读数陈旧更容易发现，且 actor
//! 的 `on_link_died` 会把它变成受控的整机退出。
//!
//! # 判据用时长而非连续失败次数
//!
//! 容忍窗口应当与轮询间隔解耦：改间隔不该连带改变"能瞎多久"。

use crate::domain::{now_ms, Timestamp};
use std::fmt::Display;
use std::future::Future;

/// 取一次读数，并在**响应到手之后**盖时间戳，返回 `(读数, 戳)`。
///
/// # 为什么顺序必须由这个函数决定
///
/// `local_ts` 的语义是"适配层收到这条读数的时刻"，下游拿它算决策依据的陈旧度 ——
/// [`crate::engine::live::ExecutorActor`] 的管线滞后自查超过 1 秒就告警"本实例跟不上
/// 事件流"。戳若盖在**发起请求之前**，REST 往返就整段算进了那个数：IBKR 账户接口一次
/// 1.2 秒的往返，会被报成"策略正在消费积压事件"，把**对端慢**误诊成**本机跟不上**，
/// 而真正的积压反而淹没在这条噪声里。
///
/// 三个轮询器（IBKR 净值 / Binance 净值 / OKX 希腊值）都曾把戳盖在请求之前 —— 同一条
/// 规矩、同一个写法、错了三遍，所以它不该继续依赖下一个改这段代码的人的记性，而是由
/// 类型交付：拿不到读数就拿不到戳，戳也就不可能早于读数。
pub(crate) async fn stamped_on_receipt<T>(fetch: impl Future<Output = T>) -> (T, Timestamp) {
    let read = fetch.await;
    (read, now_ms())
}

/// 周期性读数的停摆守卫。
///
/// 用法：成功时 [`Self::record_success`]，失败时 [`Self::check_failure`] —— 后者返回
/// `Err` 即表示已停摆过久，调用方应当 kill 自己（而不是继续重试）。
pub struct StalenessGuard {
    /// 数据源标识（进错误信息与日志，如 "Binance 账户净值"）
    label: String,
    /// 上次**成功**取到读数的时刻
    last_success_ms: Timestamp,
    /// 允许连续取不到的最长时间
    max_staleness_ms: u64,
}

impl StalenessGuard {
    /// `label` 用于诊断信息；构造时视作"刚成功过"，容忍窗口从此刻起算。
    pub fn new(label: impl Into<String>, max_staleness_ms: u64) -> Self {
        Self {
            label: label.into(),
            last_success_ms: now_ms(),
            max_staleness_ms,
        }
    }

    /// 记一次成功：容忍窗口重新开始
    pub fn record_success(&mut self) {
        self.last_success_ms = now_ms();
    }

    /// 记一次失败。`Ok` = 仍在容忍窗口内（调用方 warn 后重试即可）；
    /// `Err(诊断信息)` = 已停摆过久，调用方应致命退出。
    pub fn check_failure(&self, error: impl Display) -> Result<(), String> {
        let stale_ms = self.stale_ms();
        if stale_ms > self.max_staleness_ms {
            return Err(format!(
                "{} 已停摆 {stale_ms}ms（上限 {}ms），读数不再可信；最后一次失败: {error}",
                self.label, self.max_staleness_ms
            ));
        }
        Ok(())
    }

    /// 距上次成功的时长
    pub fn stale_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_success_ms)
    }

    /// 测试专用：把上次成功时刻回拨 `ms` 毫秒，模拟长期失败。
    ///
    /// `pub(crate)`：守卫的消费者（如 `PositionLedgerActor` 的在途停摆判据）也要能测
    /// "超窗即致命"这条路径，而窗口是全局常量、时钟又直接读墙钟。
    #[cfg(test)]
    pub(crate) fn rewind(&mut self, ms: u64) {
        self.last_success_ms = self.last_success_ms.saturating_sub(ms);
    }
}

/// 周期性读数允许停摆的最长时间（账户净值 / 希腊值 / 持仓对账共用）。
///
/// 判据是"距上次成功的时长"而非"连续失败次数"：这样调整轮询间隔不会连带改变容忍
/// 窗口。取 60s 是因为这些读数一旦陈旧，策略与风控就在拿旧数做决策，而 REST 抖动
/// （超时、限流、5xx）通常在秒级内自愈。
pub const MAX_POLL_STALENESS_MS: u64 = 60_000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// **回归防线**：戳必须晚于响应。
    ///
    /// 此前三个轮询器把 `now_ms()` 写在 `fetch().await` **之前**，于是 REST 往返整段进了
    /// `local_ts` 的年龄里 —— 生产上表现为每隔几秒一条"策略正在消费积压事件"，而真实原因
    /// 是 IBKR 账户接口往返 1.2 秒，本机一条事件都没积压。
    #[tokio::test]
    async fn stamp_is_taken_after_the_response_arrives() {
        /// 模拟的 REST 往返时长
        const RTT_MS: u64 = 40;

        // 让"响应"在自己到手的那一刻自报一个墙钟读数，断言只比较两个墙钟读数 ——
        // 不去比"戳比调用前晚了至少 RTT"：那要跨两个时钟域（`sleep` 走单调钟、
        // `now_ms` 走 `SystemTime`），NTP 回拨落在窗口内就是一次假失败。
        // 区分度不受影响：戳若盖在请求之前，它会比 `resolved_at` 早整整一个往返。
        let ((read, resolved_at), stamp) = stamped_on_receipt(async {
            tokio::time::sleep(Duration::from_millis(RTT_MS)).await;
            ("账户读数", now_ms())
        })
        .await;

        assert_eq!(read, "账户读数", "读数应原样透传");
        assert!(
            stamp >= resolved_at,
            "戳 ({stamp}) 早于响应到手 ({resolved_at})：REST 往返会整段算进管线滞后，把对端慢误报成本机积压"
        );
    }

    /// 窗口内的失败只是重试信号，不该致命 —— REST 抖动（超时、限流、5xx）是常态
    #[test]
    fn transient_failure_is_tolerated() {
        let guard = StalenessGuard::new("测试读数", 60_000);
        assert!(guard.check_failure("timeout").is_ok());
    }

    /// **核心防线**：超过容忍窗口即报错，让调用方去死 —— 而不是继续用陈旧读数
    #[test]
    fn prolonged_failure_becomes_fatal() {
        let mut guard = StalenessGuard::new("Binance 账户净值", 60_000);
        guard.rewind(61_000);
        let err = guard
            .check_failure("connection refused")
            .expect_err("停摆超过上限仍被当成可重试 —— 消费方会一直用旧读数决策");
        assert!(err.contains("Binance 账户净值"), "错误应指明是哪个数据源: {err}");
        assert!(err.contains("connection refused"), "应保留最后一次失败原因: {err}");
    }

    /// 一次成功即重置窗口：间歇性失败不会累积成假的"长期停摆"
    #[test]
    fn success_resets_the_window() {
        let mut guard = StalenessGuard::new("测试读数", 60_000);
        guard.rewind(61_000);
        assert!(guard.check_failure("x").is_err());
        guard.record_success();
        assert!(guard.check_failure("x").is_ok(), "成功后窗口应重新开始");
    }
}
