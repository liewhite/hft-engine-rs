//! BinanceEquityPollingActor - 定时查询 Binance 账户 equity
//!
//! Binance 的 WebSocket 不推送 equity，需要通过 REST API 定时查询

use crate::domain::{now_ms, Exchange};
use crate::engine::AccountPubSub;
use crate::exchange::staleness::{StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::exchange::ExchangeClient;
use crate::messaging::{AccountData, AccountEvent};
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
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// BinanceEquityPollingActor - 定时查询 equity
pub struct BinanceEquityPollingActor {
    /// Binance client
    client: Arc<dyn ExchangeClient>,
    /// Income PubSub (发布事件)
    account_pubsub: ActorRef<AccountPubSub>,
    /// 停摆守卫：净值取不到时，`StateManager` 里的旧值会原样留着且无从分辨新鲜度 ——
    /// 账户杠杆闸门会拿着它一路放行。长期取不到就让本 actor 死掉（见 StalenessGuard）。
    guard: StalenessGuard,
}

impl BinanceEquityPollingActor {
    /// 执行一次账户信息查询 (equity + notional)。`Err` = 已停摆过久，调用方应致命退出。
    async fn poll_account_info(&mut self) -> Result<(), String> {
        let local_ts = now_ms();

        match self.client.fetch_account_info().await {
            Ok(info) => {
                self.guard.record_success();
                // 发布 AccountInfo 事件
                if let Err(e) = self
                    .account_pubsub
                    .tell(Publish(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts: local_ts,
                        local_ts,
                        data: AccountData::AccountInfo {
                            exchange: Exchange::Binance,
                            equity: info.equity,
                            notional: info.notional,
                        },
                    }))
                    .send()
                    .await
                {
                    tracing::error!(error = %e, "Failed to publish to AccountPubSub");
                }
            }
            Err(e) => {
                self.guard.check_failure(&e)?;
                tracing::warn!(
                    exchange = %Exchange::Binance,
                    error = %e,
                    stale_ms = self.guard.stale_ms(),
                    "Failed to fetch account info, will retry"
                );
            }
        }
        Ok(())
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
            account_pubsub: args.account_pubsub,
            guard: StalenessGuard::new("Binance 账户净值", MAX_POLL_STALENESS_MS),
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
                if let Err(reason) = self.poll_account_info().await {
                    tracing::error!(%reason, "Binance 账户净值长期取不到，退出（陈旧净值会让杠杆闸门失准）");
                    ctx.actor_ref().kill();
                }
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
