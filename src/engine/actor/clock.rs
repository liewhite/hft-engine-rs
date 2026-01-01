//! ClockActor - 定时广播时钟信号
//!
//! 每秒发送 Clock 事件给所有注册的 ExecutorActor
//! 同时负责查询不支持 WebSocket 推送 equity 的交易所 (如 Binance)

use super::executor::ExecutorActor;
use crate::domain::{now_ms, Exchange};
use crate::exchange::{EventSink, ExchangeClient};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// ClockActor 初始化参数
pub struct ClockArgs<S: EventSink> {
    /// 时钟间隔 (毫秒)
    pub interval_ms: u64,
    /// Binance client (用于查询 equity)
    pub binance_client: Option<Arc<dyn ExchangeClient>>,
    /// 事件接收器 (父 Actor)
    pub event_sink: Arc<S>,
}

/// ClockActor - 时钟信号广播器
pub struct ClockActor<S: EventSink> {
    /// 时钟间隔
    interval: Duration,
    /// Binance client
    binance_client: Option<Arc<dyn ExchangeClient>>,
    /// 事件接收器 (父 Actor)
    event_sink: Arc<S>,
    /// 已注册的 ExecutorActor 列表
    executors: Vec<ActorRef<ExecutorActor>>,
}

impl<S: EventSink> ClockActor<S> {
    /// 创建 ClockActor
    pub fn new(args: ClockArgs<S>) -> Self {
        Self {
            interval: Duration::from_millis(args.interval_ms),
            binance_client: args.binance_client,
            event_sink: args.event_sink,
            executors: Vec::new(),
        }
    }

    /// 执行一次 tick
    async fn tick(&mut self) {
        let local_ts = now_ms();

        // 查询 Binance equity (如果有 client)
        if let Some(ref client) = self.binance_client {
            match client.fetch_equity().await {
                Ok(equity) => {
                    self.event_sink
                        .send_event(IncomeEvent {
                            exchange_ts: local_ts, // REST API 没有交易所时间戳，用本地时间
                            local_ts,
                            data: ExchangeEventData::Equity {
                                exchange: Exchange::Binance,
                                equity,
                            },
                        })
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        exchange = %Exchange::Binance,
                        error = %e,
                        "Failed to fetch equity"
                    );
                }
            }
        }

        // 向所有 executor 发送 Clock 事件
        let clock_event = IncomeEvent {
            exchange_ts: local_ts,
            local_ts,
            data: ExchangeEventData::Clock,
        };
        for executor in &self.executors {
            executor
                .tell(clock_event.clone())
                .await
                .expect("Failed to tell ExecutorActor clock event");
        }
    }
}

impl<S: EventSink> Actor for ClockActor<S> {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ClockActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 使用 attach_stream 管理 ticker 生命周期
        let interval_stream = IntervalStream::new(tokio::time::interval(self.interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!("ClockActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("ClockActor stopped");
        Ok(())
    }
}

// === Messages ===

/// 注册 ExecutorActor
pub struct RegisterExecutor {
    pub executor: ActorRef<ExecutorActor>,
}

impl<S: EventSink> Message<RegisterExecutor> for ClockActor<S> {
    type Reply = ();

    async fn handle(&mut self, msg: RegisterExecutor, _ctx: Context<'_, Self, Self::Reply>) {
        self.executors.push(msg.executor);
    }
}

/// Ticker stream 消息处理
impl<S: EventSink> Message<StreamMessage<Instant, (), ()>> for ClockActor<S> {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.tick().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Clock ticker stream started");
            }
            StreamMessage::Finished(_) => {
                // Ticker stream 不应该结束，如果结束了就 kill actor
                tracing::error!("Clock ticker stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
