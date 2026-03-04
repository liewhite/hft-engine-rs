use crate::domain::{Exchange, Symbol};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::{HashMap, HashSet};

/// SpreadArbStatsActor 初始化参数
pub struct SpreadArbStatsArgs {
    /// 策略跟踪的 symbols
    pub symbols: HashSet<Symbol>,
}

/// 统计中心 — 订阅 IncomeEvent + OutcomeEvent，维护双边账户状态
///
/// equities/positions/prices 状态当前仅维护，待 DB writer 就绪后用于
/// 定时快照和信号/成交记录的上下文关联。
pub struct SpreadArbStatsActor {
    /// 策略跟踪的 symbols
    symbols: HashSet<Symbol>,
    /// 双边 equity (IBKR, Hyperliquid)
    equities: HashMap<Exchange, f64>,
    /// 双边持仓 (exchange, symbol) → size
    positions: HashMap<(Exchange, Symbol), f64>,
    /// BBO 中间价缓存
    prices: HashMap<(Exchange, Symbol), f64>,
}

impl SpreadArbStatsActor {
    fn is_tracked_exchange(exchange: Exchange) -> bool {
        matches!(exchange, Exchange::IBKR | Exchange::Hyperliquid)
    }
}

impl Actor for SpreadArbStatsActor {
    type Args = SpreadArbStatsArgs;
    type Error = anyhow::Error;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        tracing::info!(
            symbols = ?args.symbols,
            "SpreadArbStatsActor started"
        );
        Ok(Self {
            symbols: args.symbols,
            equities: HashMap::new(),
            positions: HashMap::new(),
            prices: HashMap::new(),
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "SpreadArbStatsActor stopped");
        Ok(())
    }
}

// ============================================================================
// Messages
// ============================================================================

impl Message<IncomeEvent> for SpreadArbStatsActor {
    type Reply = ();

    async fn handle(&mut self, msg: IncomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match &msg.data {
            ExchangeEventData::AccountInfo {
                exchange, equity, ..
            } if Self::is_tracked_exchange(*exchange) => {
                self.equities.insert(*exchange, *equity);
            }

            ExchangeEventData::Position(pos)
                if Self::is_tracked_exchange(pos.exchange)
                    && self.symbols.contains(&pos.symbol) =>
            {
                self.positions
                    .insert((pos.exchange, pos.symbol.clone()), pos.size);
            }

            ExchangeEventData::BBO(bbo)
                if Self::is_tracked_exchange(bbo.exchange)
                    && self.symbols.contains(&bbo.symbol) =>
            {
                let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
                self.prices.insert((bbo.exchange, bbo.symbol.clone()), mid);
            }

            ExchangeEventData::Fill(fill) if self.symbols.contains(&fill.symbol) => {
                tracing::info!(
                    exchange = %fill.exchange,
                    symbol = %fill.symbol,
                    side = ?fill.side,
                    price = fill.price,
                    size = fill.size,
                    "SpreadArbStats: fill"
                );
            }

            ExchangeEventData::OrderUpdate(update)
                if update.status.is_terminal() && self.symbols.contains(&update.symbol) =>
            {
                tracing::info!(
                    exchange = %update.exchange,
                    symbol = %update.symbol,
                    side = ?update.side,
                    status = ?update.status,
                    filled_qty = update.filled_quantity,
                    "SpreadArbStats: order terminal"
                );
            }

            _ => {}
        }
    }
}

impl Message<OutcomeEvent> for SpreadArbStatsActor {
    type Reply = ();

    async fn handle(&mut self, msg: OutcomeEvent, _ctx: &mut Context<Self, Self::Reply>) {
        match msg {
            OutcomeEvent::PlaceOrders { orders, comment } => {
                // 过滤只跟踪的 symbols
                let tracked: Vec<_> = orders
                    .iter()
                    .filter(|o| self.symbols.contains(&o.symbol))
                    .collect();

                if tracked.is_empty() {
                    return;
                }

                tracing::info!(
                    signal = %comment,
                    order_count = tracked.len(),
                    "SpreadArbStats: signal placed"
                );
                for order in &tracked {
                    tracing::info!(
                        exchange = %order.exchange,
                        symbol = %order.symbol,
                        side = ?order.side,
                        qty = order.quantity,
                        "SpreadArbStats: order in signal"
                    );
                }
            }
        }
    }
}
