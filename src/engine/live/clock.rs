//! ClockActor - 定时广播时钟信号
//!
//! 每秒发布 Clock 事件到 IncomePubSub

use crate::domain::now_ms;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

use super::IncomePubSub;

/// ClockActor 初始化参数
pub struct ClockActorArgs {
    /// 时钟间隔 (毫秒)
    pub interval_ms: u64,
    /// Income PubSub (用于发布 Clock 事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
}

/// ClockActor - 时钟信号广播器
pub struct ClockActor {
    /// 时钟间隔
    interval: Duration,
    /// Income PubSub (发布 Clock 事件)
    income_pubsub: ActorRef<IncomePubSub>,
}

impl ClockActor {
    /// 执行一次 tick
    async fn tick(&mut self) {
        let local_ts = now_ms();

        // 发布 Clock 事件到 IncomePubSub
        let _ = self
            .income_pubsub
            .tell(Publish(IncomeEvent {
                exchange_ts: local_ts,
                local_ts,
                data: ExchangeEventData::Clock,
            }))
            .send()
            .await;
    }
}

impl Actor for ClockActor {
    type Args = ClockActorArgs;
    type Error = Infallible;

    async fn on_start(
        args: Self::Args,
        actor_ref: ActorRef<Self>,
    ) -> Result<Self, Self::Error> {
        let actor = Self {
            interval: Duration::from_millis(args.interval_ms),
            income_pubsub: args.income_pubsub,
        };

        // 使用 attach_stream 管理 ticker 生命周期
        let interval_stream = IntervalStream::new(tokio::time::interval(actor.interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!("ClockActor started");
        Ok(actor)
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("ClockActor stopped");
        Ok(())
    }
}

// === Messages ===

/// Ticker stream 消息处理
impl Message<StreamMessage<Instant, (), ()>> for ClockActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
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
