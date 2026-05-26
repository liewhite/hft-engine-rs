//! BinanceFundingFeePollingActor - 定时拉取 Binance 资费历史
//!
//! 全仓模式下 WS 推送的 ACCOUNT_UPDATE.bc 是聚合值无法定位 symbol，
//! 通过 REST `GET /fapi/v1/income?incomeType=FUNDING_FEE` 获取 per-symbol 明细。
//!
//! 设计：
//! - 每隔 `interval_ms` 拉取一次，窗口固定为最近 24h
//! - 不做去重，下游凭 `FundingFee::tran_id` 自行去重

use crate::domain::now_ms;
use crate::engine::IncomePubSub;
use crate::exchange::binance::BinanceClient;
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
pub struct BinanceFundingFeePollingActorArgs {
    pub client: Arc<BinanceClient>,
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 轮询间隔 (毫秒)
    pub interval_ms: u64,
}

pub struct BinanceFundingFeePollingActor {
    client: Arc<BinanceClient>,
    income_pubsub: ActorRef<IncomePubSub>,
}

impl BinanceFundingFeePollingActor {
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
                    exchange = "Binance",
                    error = %e,
                    "Failed to fetch funding fees"
                );
            }
        }
    }
}

impl Actor for BinanceFundingFeePollingActor {
    type Args = BinanceFundingFeePollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "Binance",
            interval_ms = interval.as_millis() as u64,
            "BinanceFundingFeePollingActor started"
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
        tracing::info!("BinanceFundingFeePollingActor stopped");
        Ok(())
    }
}

impl Message<StreamMessage<Instant, (), ()>> for BinanceFundingFeePollingActor {
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
                tracing::debug!("FundingFee polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("FundingFee polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
