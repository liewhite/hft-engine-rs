//! SlackNotifierActor - 订阅 Income 事件，当订单成交时发送 Slack 通知
//!
//! 职责：
//! - 订阅 OrderUpdate 事件
//! - 当订单状态为 Filled 或 PartiallyFilled 时发送 Slack 通知

use crate::domain::{Exchange, OrderStatus};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde::Serialize;

/// SlackNotifierActor 初始化参数
pub struct SlackNotifierArgs {
    /// Slack channel
    pub channel: String,
    /// Slack token
    pub token: String,
}

/// SlackNotifierActor - 订阅 OrderUpdate 事件，发送 Slack 通知
pub struct SlackNotifierActor {
    /// Slack channel
    channel: String,
    /// Slack token
    token: String,
    /// HTTP Client
    http_client: reqwest::Client,
}

/// Slack 消息请求体
#[derive(Serialize)]
struct SlackMessage {
    channel: String,
    text: String,
}

impl SlackNotifierActor {
    /// 发送 Slack 消息
    async fn send_slack_message(&self, text: &str) {
        let message = SlackMessage {
            channel: self.channel.clone(),
            text: text.to_string(),
        };

        match self
            .http_client
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&message)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(channel = %self.channel, "Slack message sent successfully");
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, body = %body, "Slack API returned non-success status");
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to send Slack message");
            }
        }
    }

    /// 格式化订单成交通知
    fn format_fill_message(
        exchange: Exchange,
        symbol: &str,
        status: &OrderStatus,
        filled_qty: f64,
        avg_price: Option<f64>,
    ) -> String {
        let exchange_name = match exchange {
            Exchange::Binance => "Binance",
            Exchange::OKX => "OKX",
            Exchange::Hyperliquid => "Hyperliquid",
        };

        let status_emoji = match status {
            OrderStatus::Filled => ":white_check_mark:",
            OrderStatus::PartiallyFilled { .. } => ":hourglass:",
            _ => ":question:",
        };

        let price_info = avg_price
            .map(|p| format!(" @ {:.4}", p))
            .unwrap_or_default();

        format!(
            "{} *Order Filled*\n• Exchange: {}\n• Symbol: {}\n• Filled: {:.4}{}\n• Status: {:?}",
            status_emoji, exchange_name, symbol, filled_qty, price_info, status
        )
    }
}

impl Actor for SlackNotifierActor {
    type Args = SlackNotifierArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!(
            channel = %args.channel,
            "SlackNotifierActor started"
        );

        Ok(Self {
            channel: args.channel,
            token: args.token,
            http_client: reqwest::Client::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "SlackNotifierActor stopped");
        Ok(())
    }
}

// ============================================================================
// Messages
// ============================================================================

/// 处理 Income 事件
impl Message<IncomeEvent> for SlackNotifierActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        if let ExchangeEventData::OrderUpdate(update) = &msg.data {
            // 只处理成交事件
            match &update.status {
                OrderStatus::Filled | OrderStatus::PartiallyFilled { .. } => {
                    let message = Self::format_fill_message(
                        update.exchange,
                        &update.symbol.canonical(),
                        &update.status,
                        update.filled_quantity,
                        update.avg_price,
                    );
                    self.send_slack_message(&message).await;
                }
                _ => {
                    // 忽略其他状态
                }
            }
        }
    }
}
