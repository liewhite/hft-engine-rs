//! EquityPollingActor - 定时用 REST 查账户净值与持仓总价值（`AccountClient::fetch_account_info`）
//!
//! 用于 WebSocket 不推送净值、或推送的净值口径不对的所：Binance 的 WS 不推 equity；
//! Hyperliquid 统一账户的净值在 spot clearinghouse，按 dex 推送的 clearinghouseState
//! 里恒为 0。所别只体现在 `client.exchange()` 与各 client 的 `fetch_account_info` 实现里。

use crate::messaging::AccountPubSub;
use crate::exchange::staleness::{stamped_on_receipt, StalenessGuard, MAX_POLL_STALENESS_MS};
use crate::exchange::AccountClient;
use crate::messaging::{ExchangeAccountData, AccountData, AccountEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// EquityPollingActor 初始化参数
pub struct EquityPollingActorArgs {
    /// 账户 client（用于查询 equity；所别取自 `client.exchange()`）
    pub client: Arc<dyn AccountClient>,
    /// Income PubSub (发布事件)
    pub account_pubsub: ActorRef<AccountPubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// EquityPollingActor - 定时查询 equity
pub struct EquityPollingActor {
    /// 账户 client
    client: Arc<dyn AccountClient>,
    /// Income PubSub (发布事件)
    account_pubsub: ActorRef<AccountPubSub>,
    /// 停摆守卫：净值取不到时，`StateManager` 里的旧值会原样留着且无从分辨新鲜度 ——
    /// 账户杠杆闸门会拿着它一路放行。长期取不到就让本 actor 死掉（见 StalenessGuard）。
    guard: StalenessGuard,
}

impl EquityPollingActor {
    /// 执行一次账户信息查询 (equity + notional)。`Err` = 已停摆过久，调用方应致命退出。
    async fn poll_account_info(&mut self) -> Result<(), String> {
        // 戳在响应到手之后取，理由见 `stamped_on_receipt`
        let (read, local_ts) = stamped_on_receipt(self.client.fetch_account_info()).await;

        match read {
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
                        data: AccountData::Exchange(ExchangeAccountData::AccountInfo {
                            exchange: self.client.exchange(),
                            equity: info.equity,
                            notional: info.notional,
                        }),
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
                    exchange = %self.client.exchange(),
                    error = %e,
                    stale_ms = self.guard.stale_ms(),
                    "Failed to fetch account info, will retry"
                );
            }
        }
        Ok(())
    }
}

impl Actor for EquityPollingActor {
    type Args = EquityPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);

        // 使用 attach_stream 管理定时器生命周期
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = %args.client.exchange(),
            interval_ms = interval.as_millis() as u64,
            "EquityPollingActor started"
        );

        let guard = StalenessGuard::new(format!("{} 账户净值", args.client.exchange()), MAX_POLL_STALENESS_MS);
        Ok(Self {
            client: args.client,
            account_pubsub: args.account_pubsub,
            guard,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("EquityPollingActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for EquityPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                if let Err(reason) = self.poll_account_info().await {
                    tracing::error!(exchange = %self.client.exchange(), %reason, "账户净值长期取不到，退出（陈旧净值会让杠杆闸门失准）");
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
