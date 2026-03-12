//! IbkrTickleActor - IBKR tickle 保活 Actor
//!
//! 定时发送 POST /tickle 维持 session，纳入 link 体系实现级联退出。
//! 连续失败 3 次则 kill 自身，父 Actor 通过 on_link_died 感知并退出。

use crate::exchange::ibkr::auth::{self, IbkrAuth};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// 连续失败阈值
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Tickle 间隔 (秒)
const TICKLE_INTERVAL_SECS: u64 = 60;

/// IbkrTickleActor 初始化参数
pub struct IbkrTickleActorArgs {
    /// 认证器
    pub auth: Arc<dyn IbkrAuth>,
    /// HTTP 客户端
    pub http: reqwest::Client,
}

/// IbkrTickleActor - tickle 保活
pub struct IbkrTickleActor {
    auth: Arc<dyn IbkrAuth>,
    http: reqwest::Client,
    consecutive_failures: u32,
}

impl Actor for IbkrTickleActor {
    type Args = IbkrTickleActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_secs(TICKLE_INTERVAL_SECS);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!("IbkrTickleActor started");
        Ok(Self {
            auth: args.auth,
            http: args.http,
            consecutive_failures: 0,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("IbkrTickleActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for IbkrTickleActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                if auth::tickle(&*self.auth, &self.http).await.is_ok() {
                    self.consecutive_failures = 0;
                    tracing::trace!("IBKR tickle sent");
                } else {
                    self.consecutive_failures += 1;
                    tracing::warn!(
                        consecutive_failures = self.consecutive_failures,
                        max = MAX_CONSECUTIVE_FAILURES,
                        "IBKR tickle failed"
                    );
                    if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        tracing::error!(
                            "IBKR tickle: {} consecutive failures, killing actor",
                            MAX_CONSECUTIVE_FAILURES
                        );
                        ctx.actor_ref().kill();
                    }
                }
            }
            StreamMessage::Started(_) => {
                tracing::debug!("IBKR tickle stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("IBKR tickle stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
