//! IbkrPositionPollingActor - 定时轮询 IBKR 持仓和账户信息
//!
//! IBKR WebSocket 仅推送 BBO 行情，不推送持仓数据。
//! 通过 REST 定时轮询 positions + account summary，发布到 IncomePubSub。

use crate::domain::{now_ms, Exchange, Position, Symbol};
use crate::engine::IncomePubSub;
use crate::exchange::client::ExchangeClient;
use crate::exchange::ibkr::IbkrClient;
use crate::messaging::{ExchangeEventData, IncomeEvent};
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, Infallible};
use kameo::message::{Context, Message, StreamMessage};
use kameo::Actor;
use kameo_actors::pubsub::Publish;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_stream::wrappers::IntervalStream;

/// IbkrPositionPollingActor 初始化参数
pub struct IbkrPositionPollingActorArgs {
    /// IBKR client (用于查询持仓和账户信息)
    pub client: Arc<IbkrClient>,
    /// Income PubSub (发布事件)
    pub income_pubsub: ActorRef<IncomePubSub>,
    /// 查询间隔 (毫秒)
    pub interval_ms: u64,
}

/// IbkrPositionPollingActor - 定时轮询 IBKR 持仓和账户信息
pub struct IbkrPositionPollingActor {
    client: Arc<IbkrClient>,
    income_pubsub: ActorRef<IncomePubSub>,
    /// 已知持仓的 symbols（用于检测仓位消失时推送零仓位）
    known_positions: HashSet<Symbol>,
}

impl IbkrPositionPollingActor {
    /// 执行一次持仓查询并发布事件
    ///
    /// 跟踪 known_positions，当 symbol 从持仓列表消失时推送零仓位
    async fn poll_positions(&mut self) {
        let local_ts = now_ms();

        match self.client.fetch_positions().await {
            Ok(positions) => {
                let mut current_symbols = HashSet::new();

                for pos in positions {
                    current_symbols.insert(pos.symbol.clone());
                    let _ = self
                        .income_pubsub
                        .tell(Publish(IncomeEvent {
                            exchange_ts: local_ts,
                            local_ts,
                            data: ExchangeEventData::Position(pos),
                        }))
                        .send()
                        .await;
                }

                // 对消失的 symbol 推送零仓位
                for symbol in self.known_positions.difference(&current_symbols) {
                    tracing::info!(
                        symbol = %symbol,
                        "IBKR position disappeared, setting to zero"
                    );
                    let _ = self
                        .income_pubsub
                        .tell(Publish(IncomeEvent {
                            exchange_ts: local_ts,
                            local_ts,
                            data: ExchangeEventData::Position(Position {
                                exchange: Exchange::IBKR,
                                symbol: symbol.clone(),
                                size: 0.0,
                                entry_price: 0.0,
                                unrealized_pnl: 0.0,
                            }),
                        }))
                        .send()
                        .await;
                }

                self.known_positions = current_symbols;
            }
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR positions"
                );
            }
        }
    }

    /// 执行一次账户信息查询并发布事件
    async fn poll_account_info(&self) {
        let local_ts = now_ms();

        match self.client.fetch_account_info().await {
            Ok(info) => {
                let _ = self
                    .income_pubsub
                    .tell(Publish(IncomeEvent {
                        exchange_ts: local_ts,
                        local_ts,
                        data: ExchangeEventData::AccountInfo {
                            exchange: Exchange::IBKR,
                            equity: info.equity,
                            notional: info.notional,
                        },
                    }))
                    .send()
                    .await;
            }
            Err(e) => {
                tracing::warn!(
                    exchange = %Exchange::IBKR,
                    error = %e,
                    "Failed to fetch IBKR account info"
                );
            }
        }
    }
}

impl Actor for IbkrPositionPollingActor {
    type Args = IbkrPositionPollingActorArgs;
    type Error = Infallible;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let interval = Duration::from_millis(args.interval_ms);

        let interval_stream = IntervalStream::new(tokio::time::interval(interval));
        actor_ref.attach_stream(interval_stream, (), ());

        tracing::info!(
            exchange = "IBKR",
            interval_ms = interval.as_millis() as u64,
            "IbkrPositionPollingActor started"
        );

        Ok(Self {
            client: args.client,
            income_pubsub: args.income_pubsub,
            known_positions: HashSet::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!("IbkrPositionPollingActor stopped");
        Ok(())
    }
}

/// 定时器消息处理
impl Message<StreamMessage<Instant, (), ()>> for IbkrPositionPollingActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: StreamMessage<Instant, (), ()>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        match msg {
            StreamMessage::Next(_) => {
                self.poll_positions().await;
                self.poll_account_info().await;
            }
            StreamMessage::Started(_) => {
                tracing::debug!("IBKR position polling stream started");
            }
            StreamMessage::Finished(_) => {
                tracing::error!("IBKR position polling stream unexpectedly finished, killing actor");
                ctx.actor_ref().kill();
            }
        }
    }
}
