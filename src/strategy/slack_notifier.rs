//! SlackNotifierActor - 订阅 Income 事件，当订单完全成交时发送 Slack 通知
//!
//! 职责：
//! - 订阅 OrderUpdate 事件
//! - 当订单状态为 Filled 时发送 Slack 通知（包含多空方向）

use crate::domain::{Exchange, Order, OrderStatus, Side};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use serde::{Deserialize, Serialize};

/// Slack API 响应
#[derive(Deserialize)]
struct SlackResponse {
    ok: bool,
    error: Option<String>,
}

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
                // 检查 Slack API 响应中的 ok 字段
                match resp.json::<SlackResponse>().await {
                    Ok(slack_resp) if slack_resp.ok => {
                        tracing::debug!(channel = %self.channel, "Slack message sent successfully");
                    }
                    Ok(slack_resp) => {
                        tracing::warn!(
                            error = ?slack_resp.error,
                            channel = %self.channel,
                            "Slack API returned error"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to parse Slack response");
                    }
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, body = %body, "Slack API returned non-success HTTP status");
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
        side: Side,
        filled_qty: f64,
    ) -> String {
        let exchange_name = match exchange {
            Exchange::Binance => "Binance",
            Exchange::OKX => "OKX",
            Exchange::Hyperliquid => "Hyperliquid",
            Exchange::IBKR => "IBKR",
        };

        let (side_emoji, side_name) = match side {
            Side::Long => (":chart_with_upwards_trend:", "Long"),
            Side::Short => (":chart_with_downwards_trend:", "Short"),
        };

        format!(
            ":white_check_mark: *Order Filled*\n• Exchange: {}\n• Symbol: {}\n• Side: {} {}\n• Filled: {:.4}",
            exchange_name, symbol, side_emoji, side_name, filled_qty
        )
    }

    /// 格式化批量下单通知
    fn format_place_orders_message(orders: &[Order], comment: &str) -> String {
        let mut lines = vec![format!(":outbox_tray: *Signal: {}*", comment)];

        for order in orders {
            let exchange_name = match order.exchange {
                Exchange::Binance => "Binance",
                Exchange::OKX => "OKX",
                Exchange::Hyperliquid => "Hyperliquid",
                Exchange::IBKR => "IBKR",
            };

            let (side_emoji, side_name) = match order.side {
                Side::Long => (":chart_with_upwards_trend:", "Long"),
                Side::Short => (":chart_with_downwards_trend:", "Short"),
            };

            let price = match &order.order_type {
                crate::domain::OrderType::Market => "Market".to_string(),
                crate::domain::OrderType::Limit { price, .. } => format!("{:.4}", price),
            };

            lines.push(format!(
                "  • {} {} {} {} {:.4} @ {}",
                exchange_name, order.symbol, side_emoji, side_name, order.quantity, price,
            ));
        }

        lines.join("\n")
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
            // 只发送完全成交的通知，忽略部分成交
            if matches!(update.status, OrderStatus::Filled) {
                let message = Self::format_fill_message(
                    update.exchange,
                    &update.symbol,
                    update.side,
                    update.filled_quantity,
                );
                self.send_slack_message(&message).await;
            }
        }
    }
}

/// 处理 Outcome 事件（下单信号）
impl Message<OutcomeEvent> for SlackNotifierActor {
    type Reply = ();

    async fn handle(&mut self, msg: OutcomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match msg {
            OutcomeEvent::PlaceOrders { orders, comment } => {
                let message = Self::format_place_orders_message(&orders, &comment);
                self.send_slack_message(&message).await;
            }
        }
    }
}
