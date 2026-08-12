//! IbkrAccountPollingActor - 定时轮询 IBKR 账户净值
//!
//! IBKR WebSocket 不推送账户级 equity/notional，需要 REST 周期同步。
//! 持仓不在此处刷新——初始持仓由 ManagerActor 启动期统一 fetch，运行期由 Fill 维护。

use crate::domain::{now_ms, Exchange};
use crate::messaging::AccountPubSub;
use crate::exchange::client::AccountClient;
use crate::exchange::ibkr::IbkrClient;
use crate::exchange::staleness::{StalenessGuard, MAX_POLL_STALENESS_MS};
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

/// IbkrAccountPollingActor 初始化参数
pub struct IbkrAccountPollingActorArgs {
    /// IBKR client (用于查询账户信息)
    pub client: Arc<IbkrClient>,
    /// Income PubSub (发布事件)
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// IbkrAccountPollingActor - 定时轮询 IBKR 账户净值
pub struct IbkrAccountPollingActor {
    client: Arc<IbkrClient>,
    account_pubsub: ActorRef<AccountPubSub>,
    /// 停摆守卫：净值/名义值取不到时，`StateManager` 里的旧值会原样留着且无从分辨
    /// 新鲜度 —— 账户杠杆闸门会拿着它一路放行。长期取不到即退出。
    guard: StalenessGuard,
}

impl IbkrAccountPollingActor {
    /// 执行一次账户信息查询并发布事件。`Err` = 已停摆过久，调用方应致命退出。
    async fn poll_account_info(&mut self) -> Result<(), String> {
        let local_ts = now_ms();

        match self.client.fetch_account_info().await {
            Ok(info) => {
                self.guard.record_success();
                if let Err(e) = self
                    .account_pubsub
                    .tell(Publish(AccountEvent {
                        // 实盘适配层单账户：标签写死 Live（多账户时提升为构造参数）
                        account: crate::domain::AccountId::Live,
                        exchange_ts: local_ts,
                        local_ts,
                        data: AccountData::AccountInfo {
                            exchange: Exchange::IBKR,
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
                    exchange = %Exchange::IBKR,
                    error = %e,
                    stale_ms = self.guard.stale_ms(),
                    "Failed to fetch IBKR account info, will retry"
                );
            }
        }
        Ok(())
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
            account_pubsub: args.account_pubsub,
            guard: StalenessGuard::new("IBKR 账户净值", MAX_POLL_STALENESS_MS),
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
                if let Err(reason) = self.poll_account_info().await {
                    tracing::error!(%reason, "IBKR 账户净值长期取不到，退出（陈旧净值会让杠杆闸门失准）");
                    ctx.actor_ref().kill();
                }
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
