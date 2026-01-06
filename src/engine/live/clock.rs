//! ClockActor - 定时广播时钟信号
//!
//! 每秒通过 EventSink 发送 Clock 事件到 ProcessorActor，由其统一分发
//! 同时负责查询需要 REST API 获取 equity 的交易所 (Binance, Hyperliquid)

use crate::domain::{now_ms, Exchange};
use crate::exchange::{EventSink, ExchangeClient};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// ClockActor 初始化参数
pub struct ClockArgs<S: EventSink> {
    /// 时钟间隔 (毫秒)
    pub interval_ms: u64,
    /// 需要通过 REST API 查询 equity 的交易所 clients
    pub equity_clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 事件接收器 (父 Actor)
    pub event_sink: Arc<S>,
}

/// ClockActor - 时钟信号广播器
pub struct ClockActor<S: EventSink> {
    /// 时钟间隔
    interval: Duration,
    /// 需要查询 equity 的交易所 clients
    equity_clients: HashMap<Exchange, Arc<dyn ExchangeClient>>,
    /// 事件接收器 (发送到 ProcessorActor)
    event_sink: Arc<S>,
}

impl<S: EventSink> ClockActor<S> {
    /// 创建 ClockActor
    pub fn new(args: ClockArgs<S>) -> Self {
        Self {
            interval: Duration::from_millis(args.interval_ms),
            equity_clients: args.equity_clients,
            event_sink: args.event_sink,
        }
    }

    /// 执行一次 tick
    async fn tick(&mut self) {
        let local_ts = now_ms();

        // 查询所有配置的交易所 equity
        for (exchange, client) in &self.equity_clients {
            match client.fetch_equity().await {
                Ok(equity) => {
                    self.event_sink
                        .send_event(IncomeEvent {
                            exchange_ts: local_ts,
                            local_ts,
                            data: ExchangeEventData::Equity {
                                exchange: *exchange,
                                equity,
                            },
                        })
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        exchange = %exchange,
                        error = %e,
                        "Failed to fetch equity"
                    );
                }
            }
        }

        // 通过 EventSink 发送 Clock 事件到 ProcessorActor，由其统一广播
        self.event_sink
            .send_event(IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::Clock,
            })
            .await;
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
