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
fn determine_status_from_schedule(schedules: &[TradingSchedule]) -> MarketStatus {
    let now_et = chrono::Utc::now().with_timezone(&Eastern);
    let today_str = now_et.format("%Y%m%d").to_string();

    for schedule in schedules {
        for entry in &schedule.schedules {
            // 匹配今天的 schedule
            let date_str = match &entry.trading_schedule_date {
                Some(d) => d,
                None => continue,
            };
            if date_str != &today_str {
                continue;
            }

            // 检查当前时间是否落入某个 session
            for session in &entry.sessions {
                let (open, close) = match (&session.opening_time, &session.closing_time) {
                    (Some(o), Some(c)) => (o, c),
                    _ => continue,
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
                    // 判断是否为 regular trading session (9:30-16:00 ET)
                    let open_hm = (open_dt.hour(), open_dt.minute());
                    let close_hm = (close_dt.hour(), close_dt.minute());

                    if open_hm == (9, 30) && close_hm == (16, 0) {
                        return MarketStatus::Liquid;
                    } else {
                        return MarketStatus::Extending;
                    }
                }
            }
        }
    }

    // 没有匹配任何 session -> Closed
    // 可能是今天没有交易日程或者不在任何 session 内
    fallback_us_market_status()
}

/// 解析 IBKR 日期时间格式 "YYYYMMDD-HH:mm:ss"
fn parse_ibkr_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y%m%d-%H:%M:%S").ok()
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
