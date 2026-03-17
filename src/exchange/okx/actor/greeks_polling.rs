//! OkxGreeksPollingActor - 定时通过 REST 查询 OKX 账户希腊值
//!
//! OKX account-greeks WebSocket 推送频率太低，改用 REST 轮询。
//! 官方限速 10/2s，配置为每秒 3 次。

use crate::domain::{now_ms, Exchange};
use crate::engine::IncomePubSub;
use crate::exchange::okx::OkxClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// OkxGreeksPollingActor 初始化参数
pub struct OkxGreeksPollingActorArgs {
    /// OKX REST client
    pub client: Arc<OkxClient>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// OkxGreeksPollingActor - 定时查询希腊值
pub struct OkxGreeksPollingActor {
    client: Arc<OkxClient>,
    income_pubsub: ActorRef<IncomePubSub>,
    /// 缓存每个 ccy 的上次 timestamp，用于去重
    last_ts: HashMap<String, u64>,
}

impl OkxGreeksPollingActor {
    async fn poll_greeks(&mut self) {
        let local_ts = now_ms();

        match self.client.fetch_greeks().await {
            Ok(greeks_list) => {
                for greeks in greeks_list {
                    // 跳过未变化的数据 (OKX 数据变化时 ts 会更新)
                    let prev_ts = self.last_ts.get(&greeks.ccy).copied().unwrap_or(0);
                    if greeks.timestamp == prev_ts {
                        continue;
                    }
                    self.last_ts.insert(greeks.ccy.clone(), greeks.timestamp);

                    let exchange_ts = greeks.timestamp;
                    if let Err(e) = self
                        .income_pubsub
                        .tell(Publish(IncomeEvent {
                            exchange_ts,
                            local_ts,
                            data: ExchangeEventData::Greeks(greeks),
                        }))
                        .send()
                        .await
                    {
                        tracing::error!(error = %e, "Failed to publish Greeks to IncomePubSub");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::OKX,
                    error = %e,
                    "Failed to fetch greeks"
                );
            }
        }
    }
}

impl Actor for OkxGreeksPollingActor {
    type Args = OkxGreeksPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "OKX",
            interval_ms = args.interval_ms,
            "OkxGreeksPollingActor started"
        );

        Ok(Self {
            client: args.client,
            income_pubsub: args.income_pubsub,
            last_ts: HashMap::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("OkxGreeksPollingActor stopped");
        Ok(())
    }
}

impl Message<StreamMessage<Instant, (), ()>> for OkxGreeksPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_greeks().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Greeks polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("Greeks polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
