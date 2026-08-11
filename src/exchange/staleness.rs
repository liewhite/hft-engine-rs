//! 周期性读数的**停摆守卫**：单次失败容忍，长期取不到即致命。
//!
//! # 为什么需要它
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
