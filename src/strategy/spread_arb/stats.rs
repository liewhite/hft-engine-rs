use crate::db::enums::{
    DbOrderStatus, DbOrderType, DbSide, DbTimeInForce, Direction, SignalType,
};
use crate::db::{fill as db_fill, order as db_order, signal as db_signal};
use crate::domain::{Exchange, OrderStatus, OrderType, Symbol};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::strategy::OutcomeEvent;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::message::{Context, Message};
use kameo::Actor;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use std::collections::{HashMap, HashSet};

/// SpreadArbStatsActor 初始化参数
pub struct SpreadArbStatsArgs {
    /// 策略跟踪的 symbols
    pub symbols: HashSet<Symbol>,
    /// 数据库连接
    pub db: DatabaseConnection,
}

/// 统计中心 — 订阅 IncomeEvent + OutcomeEvent，持久化信号/订单/成交
///
/// 单 actor 顺序处理所有事件，DB 读写不存在并发竞态。
pub struct SpreadArbStatsActor {
    /// 策略跟踪的 symbols
    symbols: HashSet<Symbol>,
    /// 双边 equity (IBKR, Hyperliquid)
    equities: HashMap<Exchange, f64>,
    /// 双边持仓 (exchange, symbol) → size
    positions: HashMap<(Exchange, Symbol), f64>,
    /// BBO 缓存 (exchange, symbol) → (bid, ask)
    prices: HashMap<(Exchange, Symbol), (f64, f64)>,
    /// 数据库连接
    db: DatabaseConnection,
}

impl SpreadArbStatsActor {
    fn is_tracked_exchange(exchange: Exchange) -> bool {
        matches!(exchange, Exchange::IBKR | Exchange::Hyperliquid)
    }

    /// 从 OrderType 提取 limit price
    fn order_price(order: &crate::domain::Order) -> f64 {
        match &order.order_type {
            OrderType::Limit { price, .. } => *price,
            OrderType::Market => 0.0,
        }
    }

    /// PlaceOrders → 插入 signal + orders
    async fn on_place_orders(&self, orders: &[crate::domain::Order], comment: &str) {
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

        // 从各交易所订单提取 symbol (IBKR: "AAPL", HL: "xyz:AAPL")
        let ibkr_symbol = tracked
            .iter()
            .find(|o| o.exchange == Exchange::IBKR)
            .map(|o| &o.symbol);
        let hl_symbol = tracked
            .iter()
            .find(|o| o.exchange == Exchange::Hyperliquid)
            .map(|o| &o.symbol);

        // BBO 缓存：用各自交易所的 symbol 查询
        let (hl_bid, hl_ask) = hl_symbol
            .and_then(|s| self.prices.get(&(Exchange::Hyperliquid, s.clone())))
            .copied()
            .unwrap_or_else(|| {
                tracing::warn!("SpreadArbStats: Hyperliquid BBO missing at signal time");
                (0.0, 0.0)
            });
        let (ibkr_bid, ibkr_ask) = ibkr_symbol
            .and_then(|s| self.prices.get(&(Exchange::IBKR, s.clone())))
            .copied()
            .unwrap_or_else(|| {
                tracing::warn!("SpreadArbStats: IBKR BBO missing at signal time");
                (0.0, 0.0)
            });

        let signal_type = SignalType::from_comment(comment);

        // 从 HL 侧订单推导 direction
        let direction = tracked
            .iter()
            .find(|o| o.exchange == Exchange::Hyperliquid)
            .map(|o| Direction::from(o.side))
            .unwrap_or_else(|| {
                tracing::warn!("SpreadArbStats: no Hyperliquid order in signal, direction unknown");
                Direction::Unknown
            });

        let open_spread_pct = if ibkr_ask > 0.0 {
            (hl_bid - ibkr_ask) / ibkr_ask * 100.0
        } else {
            0.0
        };
        let close_spread_pct = if ibkr_bid > 0.0 {
            (hl_ask - ibkr_bid) / ibkr_bid * 100.0
        } else {
            0.0
        };

        // 从 HL 侧订单计算 planned_quantity / planned_trade_size
        let (planned_qty, planned_size) = tracked
            .iter()
            .find(|o| o.exchange == Exchange::Hyperliquid)
            .map(|o| {
                let price = Self::order_price(o);
                (o.quantity, o.quantity * price)
            })
            .unwrap_or((0.0, 0.0));

        // perp_symbol = HL 侧, spot_symbol = IBKR 侧
        let perp_sym = hl_symbol
            .cloned()
            .unwrap_or_else(|| tracked[0].symbol.clone());
        let spot_sym = ibkr_symbol
            .cloned()
            .unwrap_or_else(|| tracked[0].symbol.clone());

        let signal_model = db_signal::ActiveModel {
            created_at: Set(chrono::Utc::now()),
            perp_symbol: Set(perp_sym),
            spot_symbol: Set(spot_sym),
            signal_type: Set(signal_type),
            direction: Set(direction),
            perp_bid: Set(hl_bid),
            perp_ask: Set(hl_ask),
            spot_bid: Set(ibkr_bid),
            spot_ask: Set(ibkr_ask),
            open_spread_pct: Set(open_spread_pct),
            close_spread_pct: Set(close_spread_pct),
            planned_trade_size: Set(planned_size),
            planned_quantity: Set(planned_qty),
            ..Default::default()
        };

        let signal_id = match signal_model.insert(&self.db).await {
            Ok(inserted) => inserted.id,
            Err(e) => {
                tracing::error!(error = %e, "Failed to insert signal");
                return;
            }
        };

        for order in &tracked {
            let price = Self::order_price(order);

            let db_order = db_order::ActiveModel {
                created_at: Set(chrono::Utc::now()),
                opportunity_id: Set(signal_id),
                exchange: Set(order.exchange.to_string()),
                symbol: Set(order.symbol.clone()),
                client_order_id: Set(order.client_order_id.clone()),
                external_order_id: Set(None),
                side: Set(DbSide::from(order.side)),
                quantity: Set(order.quantity),
                price: Set(price),
                order_type: Set(DbOrderType::from(&order.order_type)),
                time_in_force: Set(DbTimeInForce::from_order_type(&order.order_type)),
                status: Set(DbOrderStatus::Pending),
                filled_qty: Set(0.0),
                avg_price: Set(0.0),
                error_message: Set(None),
                ..Default::default()
            };
            if let Err(e) = db_order.insert(&self.db).await {
                tracing::error!(error = %e, "Failed to insert order");
            }
        }
    }

    /// Fill → 查询 order + 插入 fill + 更新 filled_qty
    ///
    /// filled_qty 采用 read-then-write 累加（单 actor 无并发竞态）。
    /// 终态 OrderUpdate 会以交易所报告的 filled_quantity 覆写，自动修正中间态。
    async fn on_fill(&self, fill: &crate::domain::Fill) {
        if !self.symbols.contains(&fill.symbol) {
            return;
        }

        tracing::info!(
            exchange = %fill.exchange,
            symbol = %fill.symbol,
            side = ?fill.side,
            price = fill.price,
            size = fill.size,
            "SpreadArbStats: fill"
        );

        let client_oid = match &fill.client_order_id {
            Some(id) => id,
            None => {
                tracing::warn!(
                    exchange = %fill.exchange,
                    symbol = %fill.symbol,
                    "Fill without client_order_id, skipping DB persistence"
                );
                return;
            }
        };

        let order = match db_order::Entity::find()
            .filter(db_order::Column::ClientOrderId.eq(client_oid.as_str()))
            .one(&self.db)
            .await
        {
            Ok(Some(order)) => order,
            Ok(None) => {
                tracing::warn!(client_order_id = %client_oid, "Fill: order not found in DB");
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to query order for fill");
                return;
            }
        };

        let db_fill = db_fill::ActiveModel {
            created_at: Set(chrono::Utc::now()),
            order_id: Set(order.id),
            external_order_id: Set(Some(fill.order_id.clone())),
            exchange: Set(fill.exchange.to_string()),
            symbol: Set(fill.symbol.clone()),
            side: Set(DbSide::from(fill.side)),
            price: Set(fill.price),
            quantity: Set(fill.size),
            fee: Set(None),
            fill_timestamp: Set(Some(fill.timestamp as i64)),
            ..Default::default()
        };
        if let Err(e) = db_fill.insert(&self.db).await {
            tracing::error!(error = %e, "Failed to insert fill");
        }

        // 更新 order filled_qty（单 actor 串行，无并发问题）
        let new_filled_qty = order.filled_qty + fill.size;
        let mut order_am: db_order::ActiveModel = order.into();
        order_am.filled_qty = Set(new_filled_qty);
        if let Err(e) = order_am.update(&self.db).await {
            tracing::error!(error = %e, "Failed to update order filled_qty");
        }
    }

    /// OrderUpdate (terminal) → 更新 order status
    ///
    /// 终态以交易所报告的 filled_quantity 为准，覆写 fill 累加的中间值。
    async fn on_order_update(&self, update: &crate::domain::OrderUpdate) {
        if !update.status.is_terminal() || !self.symbols.contains(&update.symbol) {
            return;
        }

        tracing::info!(
            exchange = %update.exchange,
            symbol = %update.symbol,
            side = ?update.side,
            status = ?update.status,
            filled_qty = update.filled_quantity,
            "SpreadArbStats: order terminal"
        );

        let client_oid = match &update.client_order_id {
            Some(id) => id,
            None => {
                tracing::warn!(
                    exchange = %update.exchange,
                    symbol = %update.symbol,
                    "OrderUpdate without client_order_id, skipping DB update"
                );
                return;
            }
        };

        let order = match db_order::Entity::find()
            .filter(db_order::Column::ClientOrderId.eq(client_oid.as_str()))
            .one(&self.db)
            .await
        {
            Ok(Some(order)) => order,
            Ok(None) => {
                tracing::warn!(client_order_id = %client_oid, "OrderUpdate: order not found in DB");
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to query order for update");
                return;
            }
        };

        let error_msg = match &update.status {
            OrderStatus::Rejected { reason } | OrderStatus::Error { reason } => {
                Some(reason.clone())
            }
            _ => None,
        };

        let mut order_am: db_order::ActiveModel = order.into();
        order_am.status = Set(DbOrderStatus::from(&update.status));
        order_am.filled_qty = Set(update.filled_quantity);
        order_am.external_order_id = Set(Some(update.order_id.clone()));
        order_am.error_message = Set(error_msg);
        if let Err(e) = order_am.update(&self.db).await {
            tracing::error!(error = %e, "Failed to update order status");
        }
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
            db: args.db,
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
                self.prices.insert(
                    (bbo.exchange, bbo.symbol.clone()),
                    (bbo.bid_price, bbo.ask_price),
                );
            }

            ExchangeEventData::Fill(fill) => {
                self.on_fill(fill).await;
            }

            ExchangeEventData::OrderUpdate(update) => {
                self.on_order_update(update).await;
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
                self.on_place_orders(&orders, &comment).await;
            }
            OutcomeEvent::CancelOrder { .. } => {}
        }
    }
}
