//! PositionPollingActor —— 持仓对账读数的产出者。
//!
//! 周期性调用 [`ExchangeClient::fetch_positions`]，把该所的**完整**持仓快照作为
//! [`ExchangeEventData::PositionReport`] 发到 income 总线，供
//! [`crate::engine::PositionReconcileActor`] 与「基线 + Fill 累加」的本地持仓比对。
//!
//! # 为什么是通用 actor，而不是每个所一个
//!
//! `fetch_positions` 已经在 [`ExchangeClient`] trait 上，本 actor 只需要
//! `Arc<dyn ExchangeClient>`，因此四个所共用这一份实现，`ManagerActor` 那里是个循环。
//! 交易所模块**一行都不用改** —— 这也是把对账源定在 REST 而非各所私有 WS 推送的附带好处：
//! 后者每个所的推送时机与完整性语义都不同，得在四个适配层各写一遍。
//!
//! # 拉取失败的处理：单次容忍，长期停摆即致命
//!
//! REST 抖动（超时、限流、5xx）是常态，单次失败只 warn。但**对账通道停摆等于风控失效**，
//! 而失效必须可见：距上次成功超过 [`MAX_POLL_STALENESS_MS`] 即 kill 自己，经父 actor 的
//! `on_link_died` 整机受控退出。判据用"距上次成功的时长"而非"连续失败次数"，这样改轮询
//! 间隔不会连带改变容忍窗口（与 `IbkrSnapshotPollingActor` 的 kill 线同口径）。

use crate::domain::{now_ms, Timestamp};
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

use super::IncomePubSub;

/// 持仓对账的轮询间隔。
///
/// 与 [`crate::engine::PositionReconcileActor`] 的"连续 N 次不一致才致命"配合决定反应速度：
/// 3s × 3 次 = 持续约 9 秒不一致才停机。间隔不能太短——它同时是"飞行窗口"的宽限期：
/// REST 快照时点与本地 Fill 流之间必然有间隙，间隔太短会让正常成交被误判成漂移。
pub const DEFAULT_POSITION_POLL_INTERVAL_MS: u64 = 3_000;

/// 允许对账通道停摆的最长时间，超过即致命退出。
const MAX_POLL_STALENESS_MS: u64 = 60_000;

/// PositionPollingActor 初始化参数
pub struct PositionPollingActorArgs {
    /// 目标交易所的 REST client（交易所标识取自 `client.exchange()`，不另传一份）
    pub client: Arc<dyn ExchangeClient>,
    /// Income PubSub（发布 PositionReport）
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 轮询间隔 (毫秒)
    pub interval_ms: u64,
}

/// 持仓对账读数轮询器（每个交易所一个）
pub struct PositionPollingActor {
    client: Arc<dyn ExchangeClient>,
    income_pubsub: ActorRef<IncomePubSub>,
    /// 上次**成功**拉取的时刻，用于判定对账通道是否已停摆
    last_success_ms: Timestamp,
}

impl PositionPollingActor {
    /// 拉一次并发布。`Err` = 对账通道已停摆过久，调用方应致命退出。
    async fn poll(&mut self) -> Result<(), String> {
        let exchange = self.client.exchange();
        match self.client.fetch_positions().await {
            Ok(positions) => {
                self.last_success_ms = now_ms();
                let local_ts = self.last_success_ms;
                tracing::debug!(%exchange, count = positions.len(), "Position report polled");
                if let Err(e) = self
                    .income_pubsub
                    .tell(Publish(IncomeEvent {
                        // REST 响应不带交易所时点，两个时间戳同取本地接收时刻
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::PositionReport {
                            exchange,
                            positions,
                        },
                    }))
                    .send()
                    .await
                {
                    tracing::error!(%exchange, error = %e, "Failed to publish PositionReport");
                }
                Ok(())
            }
            Err(e) => {
                let stale_ms = now_ms().saturating_sub(self.last_success_ms);
                if stale_ms > MAX_POLL_STALENESS_MS {
                    return Err(format!(
                        "{exchange} 持仓对账已停摆 {stale_ms}ms（上限 {MAX_POLL_STALENESS_MS}ms），\
                         最后一次失败: {e}"
                    ));
                }
                tracing::warn!(
                    %exchange,
                    error = %e,
                    stale_ms,
                    "Failed to fetch positions for reconciliation, will retry"
                );
                Ok(())
            }
        }
    }
}

impl Actor for PositionPollingActor {
    type Args = PositionPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = %args.client.exchange(),
            interval_ms = args.interval_ms,
            "PositionPollingActor started"
        );

        Ok(Self {
            client: args.client,
            income_pubsub: args.income_pubsub,
            // 从启动时刻起算，避免首次拉取前就被判成停摆
            last_success_ms: now_ms(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(exchange = %self.client.exchange(), "PositionPollingActor stopped");
        Ok(())
    }
}

impl Message<StreamMessage<Instant, (), ()>> for PositionPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                if let Err(reason) = self.poll().await {
                    tracing::error!(
                        exchange = %self.client.exchange(),
                        %reason,
                        "持仓对账通道停摆过久，退出（对账停摆等于风控失效，不静默续跑）"
                    );
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Started(_) => {
                tracing::debug!("Position polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!(
                    exchange = %self.client.exchange(),
                    "Position polling stream unexpectedly finished, killing actor"
                );
                ctx.actor_ref().kill();
            }
        }
    }
}
