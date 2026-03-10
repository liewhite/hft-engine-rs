//! IbkrStatusPollingActor - 定时轮询 IBKR 市场状态
//!
//! IBKR 股票交易所有明确的交易时段。
//! 通过查询 AAPL 交易时间表判断当前市场状态 (Liquid/Extending/Closed)。
//! API 失败时 warn 并跳过本轮，等待下次轮询重试。

use crate::domain::{now_ms, Exchange, MarketStatus};
use crate::engine::IncomePubSub;
use crate::exchange::ibkr::client::TradingSchedule;
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use chrono::{Datelike, Timelike};
use chrono_tz::US::Eastern;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// IbkrStatusPollingActor 初始化参数
pub struct IbkrStatusPollingActorArgs {
    /// IBKR client (用于查询交易时间表)
    pub client: Arc<IbkrClient>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// IbkrStatusPollingActor - 定时轮询 IBKR 市场状态
pub struct IbkrStatusPollingActor {
    client: Arc<IbkrClient>,
    income_pubsub: ActorRef<IncomePubSub>,
    last_status: Option<MarketStatus>,
}

impl IbkrStatusPollingActor {
    /// 执行一次状态轮询并发布事件
    async fn poll_status(&mut self) {
        let schedules = match self.client.fetch_trading_schedule().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR trading schedule"
                );
                return;
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
        let _ = self
            .income_pubsub
            .tell(Publish(IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::ExchangeStatus {
                    exchange: Exchange::IBKR,
                    status,
                },
            }))
            .send()
            .await;
    }
}

impl Actor for IbkrStatusPollingActor {
    type Args = IbkrStatusPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);

        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "IBKR",
            interval_ms = interval.as_millis() as u64,
            "IbkrStatusPollingActor started"
        );

        Ok(Self {
            client: args.client,
            income_pubsub: args.income_pubsub,
            last_status: None,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("IbkrStatusPollingActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for IbkrStatusPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_status().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("IBKR status polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!(
                    "IBKR status polling stream unexpectedly finished, killing actor"
                );
                ctx.actor_ref().kill();
            }
        }
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

    // 只使用 ISLAND venue 的时间表
    // ISLAND ECN = NASDAQ 交易所的电子交易系统，有完整的 PRE/LIQUID/POST 时段
    // "NASDAQ Market Making" 只有 LIQUID 时段，不含盘前/盘后
    let schedule = match schedules
        .iter()
        .find(|s| s.description.as_deref().map_or(false, |d| d == "ISLAND"))
        .or_else(|| {
            tracing::warn!(
                "No ISLAND venue found in IBKR schedule, falling back to first venue"
            );
            schedules.first()
        })
    {
        Some(s) => {
            tracing::debug!(
                id = ?s.id,
                desc = ?s.description,
                "IBKR matched venue"
            );
            s
        }
        None => return MarketStatus::Closed,
    };

    // 优先精确日期匹配 (节假日/特殊日期)，再匹配周几模式
    let entry = schedule
        .schedules
        .iter()
        .find(|e| e.trading_schedule_date.as_deref() == Some(&today_str))
        .or_else(|| {
            schedule
                .schedules
                .iter()
                .find(|e| e.trading_schedule_date.as_deref() == Some(dow_str))
        });

    let entry = match entry {
        Some(e) => e,
        None => {
            tracing::debug!(today = %today_str, dow = dow_str, "No matching schedule entry found");
            return MarketStatus::Closed;
        }
    };

    tracing::debug!(
        date = ?entry.trading_schedule_date,
        sessions = ?entry.sessions,
        now_mins = now_mins,
        "IBKR matched schedule entry"
    );

    for session in &entry.sessions {
        let (open_str, close_str) = match (&session.opening_time, &session.closing_time) {
            (Some(o), Some(c)) => (o.as_str(), c.as_str()),
            _ => continue,
        };

        if let (Some(open), Some(close)) = (parse_hhmm(open_str), parse_hhmm(close_str)) {
            // 跳过 0000-0000 的空时段
            if open == 0 && close == 0 {
                continue;
            }
            let in_session = if close > open {
                now_mins >= open && now_mins < close
            } else {
                now_mins >= open || now_mins < close
            };
            if in_session {
                return classify_prop(session.prop.as_deref());
            }
        }
    }

    MarketStatus::Closed
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
            tracing::warn!("IBKR session missing prop field, defaulting to Extending");
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

