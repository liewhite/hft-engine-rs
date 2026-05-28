//! IbkrAccountPollingActor - 定时轮询 IBKR 账户净值
//!
//! IBKR WebSocket 不推送账户级 equity/notional，需要 REST 周期同步。
//! 持仓不在此处刷新——初始持仓由 ManagerActor 启动期统一 fetch，运行期由 Fill 维护。

use crate::domain::{now_ms, Exchange};
use crate::engine::IncomePubSub;
use crate::exchange::client::ExchangeClient;
use crate::exchange::ibkr::IbkrClient;
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

/// IbkrAccountPollingActor 初始化参数
pub struct IbkrAccountPollingActorArgs {
    /// IBKR client (用于查询账户信息)
    pub client: Arc<IbkrClient>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// IbkrAccountPollingActor - 定时轮询 IBKR 账户净值
pub struct IbkrAccountPollingActor {
    client: Arc<IbkrClient>,
    income_pubsub: ActorRef<IncomePubSub>,
}

impl IbkrAccountPollingActor {
    /// 执行一次账户信息查询并发布事件
    async fn poll_account_info(&self) {
        let local_ts = now_ms();

        match self.client.fetch_account_info().await {
            Ok(info) => {
                if let Err(e) = self
                    .income_pubsub
                    .tell(Publish(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::AccountInfo {
                            exchange: Exchange::IBKR,
                            equity: info.equity,
                            notional: info.notional,
                        },
                    }))
                    .send()
                    .await
                {
                    tracing::error!(error = %e, "Failed to publish to IncomePubSub");
                }
            }
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR account info"
                );
            }
        }
    }
}

impl Actor for IbkrAccountPollingActor {
    type Args = IbkrAccountPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);

        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "IBKR",
            interval_ms = interval.as_millis() as u64,
            "IbkrAccountPollingActor started"
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
        tracing::info!("IbkrAccountPollingActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for IbkrAccountPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_account_info().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("IBKR account polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("IBKR account polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
