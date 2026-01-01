//! BinanceListenKeyActor - 定时刷新 ListenKey
//!
//! 使用 IntervalStream + attach_stream 实现定时任务
//! 刷新失败时 kill 自己，级联到父 PrivateWsActor

use crate::exchange::client::WsError;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// ListenKey 刷新间隔 (30 分钟)
const LISTEN_KEY_REFRESH_INTERVAL_SECS: u64 = 30 * 60;

/// BinanceListenKeyActor 初始化参数
pub struct BinanceListenKeyActorArgs {
    /// REST API 基础 URL
    pub rest_base_url: String,
    /// API Key
    pub api_key: String,
}

/// BinanceListenKeyActor - 定时刷新 ListenKey
pub struct BinanceListenKeyActor {
    /// REST API 基础 URL
    rest_base_url: String,
    /// API Key
    api_key: String,
    /// HTTP 客户端
    client: reqwest::Client,
}

impl BinanceListenKeyActor {
    /// 创建新的 BinanceListenKeyActor
    pub fn new(args: BinanceListenKeyActorArgs) -> Self {
        Self {
            rest_base_url: args.rest_base_url,
            api_key: args.api_key,
            client: reqwest::Client::new(),
        }
    }

    /// 刷新 ListenKey
    async fn refresh_listen_key(&self) -> Result<(), WsError> {
        let resp = self
            .client
            .put(format!("{}/fapi/v1/listenKey", self.rest_base_url))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| WsError::AuthFailed(e.to_string()))?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(WsError::AuthFailed(format!(
                "ListenKey refresh failed: {}",
                text
            )));
        }

        tracing::debug!("Binance ListenKey refreshed");
        Ok(())
    }
}

impl Actor for BinanceListenKeyActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "BinanceListenKeyActor"
    }

    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        // 创建 IntervalStream，定时触发刷新
        let interval = Duration::from_secs(LISTEN_KEY_REFRESH_INTERVAL_SECS);
        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!("BinanceListenKeyActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("BinanceListenKeyActor stopped");
        Ok(())
    }
}

// ============================================================================
// 消息处理
// ============================================================================

/// IntervalStream 消息处理
impl Message<StreamMessage<Instant, (), ()>> for BinanceListenKeyActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: Context<'_, Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                // 执行 ListenKey 刷新
                if let Err(e) = self.refresh_listen_key().await {
                    tracing::error!(error = %e, "ListenKey refresh failed, killing actor");
                    ctx.actor_ref().kill();
                }
            }
            StreamMessage::Started(_) => {
                tracing::debug!("ListenKey refresh interval stream started");
            }
            StreamMessage::Finished(_) => {
                // IntervalStream 不应该结束，如果结束了就 kill actor
                tracing::error!("ListenKey refresh interval stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
