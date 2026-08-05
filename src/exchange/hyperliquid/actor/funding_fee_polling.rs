//! HyperliquidFundingFeePollingActor - 定时拉取 Hyperliquid 资费历史
//!
//! 与 `BinanceFundingFeePollingActor` 对位，行为刻意保持一致：
//! - 每隔 `interval_ms` 拉一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重（重叠窗口是有意的：
//!   宁可重复推送靠下游去重，也不要因为窗口对不齐而漏掉一次结算）
//!
//! 为什么走 REST 而不是 WS `userFundings` 订阅：`FundingFee` 需要一个稳定的去重 ID，
//! 而 WS 推送不带任何 ID，且重连时会重推整份 snapshot。REST 这条同样不带可用 ID
//! （`hash` 恒为全零），但至少能由 `(time, coin)` 稳定派生，且拉取窗口可控。

use crate::domain::now_ms;
use crate::engine::IncomePubSub;
use crate::exchange::hyperliquid::HyperliquidClient;
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

/// 资费拉取窗口（24 小时）
const FUNDING_FEE_LOOKBACK_MS: u64 = 24 * 60 * 60 * 1000;

/// 初始化参数
pub struct HyperliquidFundingFeePollingActorArgs {
    pub client: Arc<HyperliquidClient>,
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 轮询间隔 (毫秒)
    pub interval_ms: u64,
}

pub struct HyperliquidFundingFeePollingActor {
    client: Arc<HyperliquidClient>,
    income_pubsub: ActorRef<IncomePubSub>,
}

impl HyperliquidFundingFeePollingActor {
    async fn poll(&self) {
        let now = now_ms();
        let start = now.saturating_sub(FUNDING_FEE_LOOKBACK_MS);

        match self.client.fetch_funding_fees(start, now).await {
            Ok(fees) => {
                let local_ts = now_ms();
                for fee in fees {
                    let exchange_ts = fee.timestamp;
                    if let Err(e) = self
                        .income_pubsub
                        .tell(Publish(IncomeEvent {
                            exchange_ts,
                            local_ts,
                            data: ExchangeEventData::FundingFee(fee),
                        }))
                        .send()
                        .await
                    {
                        tracing::error!(error = %e, "Failed to publish FundingFee to IncomePubSub");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    exchange = "Hyperliquid",
                    error = %e,
                    "Failed to fetch funding fees"
                );
            }
        }
    }
}

impl Actor for HyperliquidFundingFeePollingActor {
    type Args = HyperliquidFundingFeePollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "Hyperliquid",
            interval_ms = interval.as_millis() as u64,
            "HyperliquidFundingFeePollingActor started"
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
        tracing::info!("HyperliquidFundingFeePollingActor stopped");
        Ok(())
    }
}

impl Message<StreamMessage<Instant, (), ()>> for HyperliquidFundingFeePollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Hyperliquid FundingFee polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!(
                    "Hyperliquid FundingFee polling stream unexpectedly finished, killing actor"
                );
                ctx.actor_ref().kill();
            }
        }
    }
}
