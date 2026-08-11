//! CryptoStatusActor - 加密货币交易所市场状态广播
//!
//! 加密货币交易所 7x24 运行，当前始终为 Liquid 状态。
//! 定时发布 ExchangeStatus 事件，供策略层统一感知市场状态。
//!
//! 保留独立 Actor 的理由：
//! - StateManager 默认 Closed（安全侧），需要主动"喂"状态以避免策略误判
//! - 未来加密交易所可能有维护停机等非 Liquid 状态，届时可在此扩展判定逻辑

use crate::domain::{now_ms, Exchange, MarketStatus};
use crate::messaging::{MarketData, MarketEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

use super::MarketPubSub;

/// CryptoStatusActor 初始化参数
pub struct CryptoStatusActorArgs {
    /// 交易所标识
    pub exchange: Exchange,
    /// Income PubSub (用于发布 ExchangeStatus 事件)
    pub market_pubsub: ActorRef<MarketPubSub>,
    /// 发布间隔 (毫秒)
    pub interval_ms: u64,
}

/// CryptoStatusActor - 加密货币市场状态广播器
pub struct CryptoStatusActor {
    exchange: Exchange,
    market_pubsub: ActorRef<MarketPubSub>,
}

impl Actor for CryptoStatusActor {
    type Args = CryptoStatusActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = %args.exchange,
            "CryptoStatusActor started"
        );

        Ok(Self {
            exchange: args.exchange,
            market_pubsub: args.market_pubsub,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(exchange = %self.exchange, "CryptoStatusActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for CryptoStatusActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                let local_ts = now_ms();
                if let Err(e) = self
                    .market_pubsub
                    .tell(Publish(MarketEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: MarketData::ExchangeStatus {
                            exchange: self.exchange,
                            status: MarketStatus::Liquid,
                        },
                    }))
                    .send()
                    .await
                {
                    tracing::error!(error = %e, "Failed to publish to MarketPubSub");
                }
            }
            StreamMessage::Started(_) => {
                tracing::debug!(exchange = %self.exchange, "Crypto status stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!(exchange = %self.exchange, "Crypto status stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
