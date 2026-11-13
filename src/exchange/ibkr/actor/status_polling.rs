//! IbkrStatusPoller - 定时轮询 IBKR 市场状态
//!
//! IBKR 股票交易所有明确的交易时段。
//! 通过查询 AAPL 交易时间表判断当前市场状态 (Liquid/Extending/Closed)。
//! API 失败时 warn 并跳过本轮，等待下次轮询重试。

use crate::domain::{now_ms, Exchange, MarketStatus};
use crate::exchange::ibkr::client::TradingSchedule;
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::ExchangeMarketData;
use crate::messaging::{MarketBus, MarketData, MarketEvent};
use crate::tasks::{ChildHandle, Group, GroupClosed, Periodic};
use chrono::{Datelike, Timelike};
use chrono_tz::US::Eastern;
use std::sync::Arc;
use std::time::Duration;

/// 定时轮询 IBKR 市场状态
pub(super) struct IbkrStatusPoller {
    client: Arc<IbkrClient>,
    market_bus: MarketBus,
    last_status: Option<MarketStatus>,
}

impl IbkrStatusPoller {
    /// 在 `group` 下起轮询，每 `interval_ms` 毫秒一次（首次立即）
    pub(super) fn spawn(
        group: &Group,
        client: Arc<IbkrClient>,
        market_bus: MarketBus,
        interval_ms: u64,
    ) -> Result<ChildHandle, GroupClosed> {
        tracing::info!(exchange = "IBKR", interval_ms, "IbkrStatusPoller started");
        group.spawn_periodic(
            "IbkrStatusPoller",
            Duration::from_millis(interval_ms),
            Self {
                client,
                market_bus,
                last_status: None,
            },
        )
    }
}

impl Periodic for IbkrStatusPoller {
    /// 执行一次状态轮询并发布事件（失败只告警，见下）
    async fn tick(&mut self) -> anyhow::Result<()> {
        let schedules = match self.client.fetch_trading_schedule().await {
            Ok(s) => s,
            Err(e) => {
                // 不接 StalenessGuard：取不到交易时段表时，`StateManager::market_status`
                // 的缺省与本组件的判定都落在 `Closed`（不交易）—— 失败方向本身就是
                // 安全侧，再加一道致命守卫只会把"该歇着"变成"整机退出"。
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR trading schedule"
                );
                return Ok(());
            }
        };
        let status = determine_status_from_schedule(&schedules);

        // 仅在状态变化时打日志
        if self.last_status != Some(status) {
            tracing::info!(
                exchange = %Exchange::IBKR,
                status = %status,
                prev = ?self.last_status,
                "IBKR market status changed"
            );
            self.last_status = Some(status);
        }

        let local_ts = now_ms();
        self.market_bus.publish(MarketEvent {
            exchange_ts: local_ts,
            local_ts,
            data: MarketData::Exchange(ExchangeMarketData::ExchangeStatus {
                exchange: Exchange::IBKR,
                status,
            }),
        });
        Ok(())
    }
}

// ============================================================================
// 状态判定逻辑
// ============================================================================

/// 从 IBKR 交易时间表判定当前市场状态
///
/// 从 NASDAQ venue 判定当前市场状态。
/// NASDAQ 对盘前/正常/盘后有准确的 prop 标注，其他 venue 可能把全天标为 LIQUID。
///
/// IBKR schedule API 的日期格式 (已在解析时去除连字符标准化为 YYYYMMDD):
/// - 周几模式: "20000101"=Sat, "20000103"=Mon, ..., "20000107"=Fri
/// - 精确日期: 节假日用实际日期 (如 "20260403" = Good Friday)
///
/// 时间格式: "HHmm" (如 "0930"=09:30, "1600"=16:00)
fn determine_status_from_schedule(schedules: &[TradingSchedule]) -> MarketStatus {
    let now_et = chrono::Utc::now().with_timezone(&Eastern);
    let today_str = now_et.format("%Y%m%d").to_string();
    let now_mins = now_et.hour() * 60 + now_et.minute();

    // 2000-01-01 是周六，所以周几映射:
    // Sat="20000101" Sun="20000102" Mon="20000103" ... Fri="20000107"
    let dow_str = match now_et.weekday() {
        chrono::Weekday::Mon => "20000103",
        chrono::Weekday::Tue => "20000104",
        chrono::Weekday::Wed => "20000105",
        chrono::Weekday::Thu => "20000106",
        chrono::Weekday::Fri => "20000107",
        chrono::Weekday::Sat => "20000101",
        chrono::Weekday::Sun => "20000102",
    };

    // 查找策略：
    // 1. 优先用 "NASDAQ Market Making" — 有精确的 prop 标注 (LIQUID 09:30-16:00)
    // 2. 不在 NASDAQ 时段内，用 "ISLAND" — 覆盖完整交易窗口 (04:00-20:00)，命中即为盘前/盘后
    // 两个 venue 都找不到则 fallback 到第一个 venue
    let nasdaq = find_venue(schedules, "NASDAQ Market Making");
    let island = find_venue(schedules, "ISLAND");

    // 先尝试 NASDAQ
    if let Some(status) = try_match_venue(nasdaq, &today_str, dow_str, now_mins) {
        return status;
    }
    // NASDAQ 未命中，尝试 ISLAND（在交易窗口内 = 盘前/盘后）
    if let Some(status) = try_match_venue(island, &today_str, dow_str, now_mins) {
        return status;
    }

    // 两个 venue 都未匹配（周末/节假日无时段）
    MarketStatus::Closed
}

/// 按 description 查找 venue
fn find_venue<'a>(schedules: &'a [TradingSchedule], desc: &str) -> Option<&'a TradingSchedule> {
    schedules
        .iter()
        .find(|s| s.description.as_deref() == Some(desc))
}

/// 尝试在指定 venue 中匹配当前时间，返回 Some(status) 或 None（未在该 venue 时段内）
fn try_match_venue(
    schedule: Option<&TradingSchedule>,
    today_str: &str,
    dow_str: &str,
    now_mins: u32,
) -> Option<MarketStatus> {
    let schedule = schedule?;

    // 优先精确日期匹配 (节假日/特殊日期)，再匹配周几模式
    let entry = schedule
        .schedules
        .iter()
        .find(|e| e.trading_schedule_date.as_deref() == Some(today_str))
        .or_else(|| {
            schedule
                .schedules
                .iter()
                .find(|e| e.trading_schedule_date.as_deref() == Some(dow_str))
        })?;

    for session in &entry.sessions {
        let (open_str, close_str) = match (&session.opening_time, &session.closing_time) {
            (Some(o), Some(c)) => (o.as_str(), c.as_str()),
            _ => continue,
        };

        if let (Some(open), Some(close)) = (parse_hhmm(open_str), parse_hhmm(close_str)) {
            if open == 0 && close == 0 {
                continue;
            }
            let in_session = if close > open {
                now_mins >= open && now_mins < close
            } else {
                now_mins >= open || now_mins < close
            };
            if in_session {
                return Some(classify_prop(session.prop.as_deref()));
            }
        }
    }

    None
}

/// 根据 prop 字段判断市场状态
fn classify_prop(prop: Option<&str>) -> MarketStatus {
    match prop {
        Some(p) => {
            let upper = p.to_uppercase();
            if upper.contains("LIQUID") || upper.contains("REGULAR") {
                MarketStatus::Liquid
            } else if upper.contains("PRE") || upper.contains("POST") || upper.contains("AFTER") {
                MarketStatus::Extending
            } else {
                tracing::warn!(prop = %p, "Unknown IBKR session prop, treating as Extending");
                MarketStatus::Extending
            }
        }
        None => {
            // ISLAND venue 不提供 prop，命中即为盘前/盘后
            MarketStatus::Extending
        }
    }
}

/// 解析 "HHmm" 格式时间字符串为分钟数
/// "0930" → 570, "1600" → 960, "0000" → 0
fn parse_hhmm(s: &str) -> Option<u32> {
    if s.len() != 4 || !s.is_ascii() {
        tracing::warn!(input = %s, "Invalid HHmm format");
        return None;
    }
    let hh: u32 = s[..2].parse().ok().or_else(|| {
        tracing::warn!(input = %s, "Invalid HHmm format: bad hour");
        None
    })?;
    let mm: u32 = s[2..].parse().ok().or_else(|| {
        tracing::warn!(input = %s, "Invalid HHmm format: bad minute");
        None
    })?;
    Some(hh * 60 + mm)
}
