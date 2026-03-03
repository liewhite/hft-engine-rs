//! IbkrStatusPollingActor - 定时轮询 IBKR 市场状态
//!
//! IBKR 股票交易所有明确的交易时段。
//! 通过查询 AAPL 交易时间表判断当前市场状态 (Liquid/Extending/Closed)。
//! API 失败时 fallback 到硬编码的 US 市场时间。

use crate::domain::{now_ms, Exchange, MarketStatus};
use crate::engine::IncomePubSub;
use crate::exchange::ibkr::client::TradingSchedule;
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use chrono::{NaiveDateTime, Timelike};
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
        let status = match self.client.fetch_trading_schedule().await {
            Ok(schedules) => determine_status_from_schedule(&schedules),
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR trading schedule, using fallback"
                );
                fallback_us_market_status()
            }
        };

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
/// API 成功返回时，严格根据返回的 schedule 数据判断。
/// 未匹配任何 session 直接返回 Closed（API 成功意味着数据可信）。
fn determine_status_from_schedule(schedules: &[TradingSchedule]) -> MarketStatus {
    let now_et = chrono::Utc::now().with_timezone(&Eastern);
    let today_str = now_et.format("%Y%m%d").to_string();

    for schedule in schedules {
        for entry in &schedule.schedules {
            let date_str = match &entry.trading_schedule_date {
                Some(d) => d,
                None => {
                    tracing::debug!("IBKR schedule entry missing tradingScheduleDate, skipping");
                    continue;
                }
            };
            if date_str != &today_str {
                continue;
            }

            // 找到今天的 schedule，遍历 sessions
            for session in &entry.sessions {
                let (open, close) = match (&session.opening_time, &session.closing_time) {
                    (Some(o), Some(c)) => (o, c),
                    _ => {
                        tracing::debug!(prop = ?session.prop, "IBKR session missing open/close time, skipping");
                        continue;
                    }
                };

                let open_dt = match parse_ibkr_datetime(open) {
                    Some(dt) => dt,
                    None => continue,
                };
                let close_dt = match parse_ibkr_datetime(close) {
                    Some(dt) => dt,
                    None => continue,
                };

                if now_et.naive_local() >= open_dt && now_et.naive_local() < close_dt {
                    return classify_session(session);
                }
            }
        }
    }

    // API 成功但未匹配任何 session → 确实不在交易时段内
    MarketStatus::Closed
}

/// 根据 session 的 prop 字段判断市场状态
///
/// IBKR API 的 prop 字段标识 session 类型。
/// 如果 prop 不可用，fallback 到时间范围判断。
fn classify_session(session: &crate::exchange::ibkr::client::TradingSession) -> MarketStatus {
    // 优先使用 prop 字段
    if let Some(ref prop) = session.prop {
        let prop_upper = prop.to_uppercase();
        if prop_upper.contains("REGULAR") {
            return MarketStatus::Liquid;
        }
        if prop_upper.contains("PRE") || prop_upper.contains("POST") || prop_upper.contains("AFTER") {
            return MarketStatus::Extending;
        }
        // prop 存在但未识别，记录日志后 fallback 到时间判断
        tracing::debug!(prop = %prop, "Unknown IBKR session prop, falling back to time-based classification");
    }

    // Fallback: 通过时间范围判断
    let open_dt = session.opening_time.as_ref().and_then(|s| parse_ibkr_datetime(s));
    let close_dt = session.closing_time.as_ref().and_then(|s| parse_ibkr_datetime(s));

    if let (Some(open), Some(close)) = (open_dt, close_dt) {
        let open_mins = open.hour() as u32 * 60 + open.minute() as u32;
        let close_mins = close.hour() as u32 * 60 + close.minute() as u32;
        // Regular session: 大致 9:00-10:00 开盘，15:00-17:00 收盘
        if (540..=600).contains(&open_mins) && (900..=1020).contains(&close_mins) {
            return MarketStatus::Liquid;
        }
    }

    // 默认视为 Extending（在某个 session 内但无法确定类型）
    MarketStatus::Extending
}

/// 解析 IBKR 日期时间格式 "YYYYMMDD-HH:mm:ss"
fn parse_ibkr_datetime(s: &str) -> Option<NaiveDateTime> {
    match NaiveDateTime::parse_from_str(s, "%Y%m%d-%H:%M:%S") {
        Ok(dt) => Some(dt),
        Err(e) => {
            tracing::warn!(input = %s, error = %e, "Failed to parse IBKR datetime");
            None
        }
    }
}

/// 硬编码的 US 市场时间 fallback
///
/// - 9:30-16:00 ET → Liquid
/// - 4:00-9:30 / 16:00-20:00 ET → Extending
/// - 其他 → Closed
fn fallback_us_market_status() -> MarketStatus {
    let now_et = chrono::Utc::now().with_timezone(&Eastern);
    let hour = now_et.hour();
    let minute = now_et.minute();
    let time_mins = hour * 60 + minute;

    match time_mins {
        // 9:30 (570) - 16:00 (960)
        570..960 => MarketStatus::Liquid,
        // 4:00 (240) - 9:30 (570)
        240..570 => MarketStatus::Extending,
        // 16:00 (960) - 20:00 (1200)
        960..1200 => MarketStatus::Extending,
        // 其他时间 → Closed
        _ => MarketStatus::Closed,
    }
}
