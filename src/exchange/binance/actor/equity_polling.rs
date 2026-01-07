//! BinanceEquityPollingActor - 定时查询 Binance 账户 equity
//!
//! Binance 的 WebSocket 不推送 equity，需要通过 REST API 定时查询

use crate::domain::{now_ms, Exchange};
use crate::engine::IncomePubSub;
use crate::exchange::ExchangeClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// BinanceEquityPollingActor 初始化参数
pub struct BinanceEquityPollingActorArgs {
    /// Binance client (用于查询 equity)
    pub client: Arc<dyn ExchangeClient>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// BinanceEquityPollingActor - 定时查询 equity
pub struct BinanceEquityPollingActor {
    /// Binance client
    client: Arc<dyn ExchangeClient>,
    /// Income PubSub (发布事件)
    income_pubsub: ActorRef<IncomePubSub>,
}

impl BinanceEquityPollingActor {
    /// 执行一次 equity 查询
    async fn poll_equity(&self) {
        let local_ts = now_ms();

        match self.client.fetch_equity().await {
            Ok(equity) => {
                let _ = self
                    .income_pubsub
                    .tell(Publish(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::Equity {
                            exchange: Exchange::Binance,
                            equity,
                        },
                    }))
                    .send()
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
}

impl Actor for BinanceEquityPollingActor {
    type Args = BinanceEquityPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);

        // 使用 attach_stream 管理定时器生命周期
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "Binance",
            interval_ms = interval.as_millis() as u64,
            "BinanceEquityPollingActor started"
        );

        Ok(Self {
            client: args.client,
            income_pubsub: args.income_pubsub,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("BinanceEquityPollingActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for BinanceEquityPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_equity().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Equity polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("Equity polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
