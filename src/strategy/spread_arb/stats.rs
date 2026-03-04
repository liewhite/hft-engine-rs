use crate::db::{fill as db_fill, order as db_order, signal as db_signal};
use crate::domain::{Exchange, OrderStatus, OrderType, Side, Symbol};
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

    /// 从 comment 前缀提取 signal_type
    fn parse_signal_type(comment: &str) -> &'static str {
        if comment.starts_with("spread_open") {
            "open"
        } else if comment.starts_with("spread_close") {
            "close"
        } else if comment.starts_with("spread_rebal") {
            "rebalance"
        } else {
            "unknown"
        }
    }

    /// 从 HL 侧订单推导 direction
    fn derive_direction(orders: &[&crate::domain::Order]) -> &'static str {
        orders
            .iter()
            .find(|o| o.exchange == Exchange::Hyperliquid)
            .map(|o| match o.side {
                Side::Long => "long_perp_short_spot",
                Side::Short => "short_perp_long_spot",
            })
            .unwrap_or("unknown")
    }

    /// 从 OrderType 提取 limit price
    fn order_price(order: &crate::domain::Order) -> f64 {
        match &order.order_type {
            OrderType::Limit { price, .. } => *price,
            OrderType::Market => 0.0,
        }
    }

    /// 格式化 OrderType 为 (type_str, tif_str)
    fn format_order_type(order_type: &OrderType) -> (&'static str, &'static str) {
        match order_type {
            OrderType::Limit { tif, .. } => (
                "limit",
                match tif {
                    crate::domain::TimeInForce::GTC => "GTC",
                    crate::domain::TimeInForce::IOC => "IOC",
                    crate::domain::TimeInForce::FOK => "FOK",
                    crate::domain::TimeInForce::PostOnly => "PostOnly",
                },
            ),
            OrderType::Market => ("market", "IOC"),
        }
    }

    /// 格式化 OrderStatus 为 DB status string
    fn format_status(status: &OrderStatus) -> &'static str {
        match status {
            OrderStatus::Created => "created",
            OrderStatus::Pending => "pending",
            OrderStatus::PartiallyFilled { .. } => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Rejected { .. } => "rejected",
            OrderStatus::Error { .. } => "error",
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

        let symbol = &tracked[0].symbol;

        let (hl_bid, hl_ask) = self
            .prices
            .get(&(Exchange::Hyperliquid, symbol.clone()))
            .copied()
            .unwrap_or((0.0, 0.0));
        let (ibkr_bid, ibkr_ask) = self
            .prices
            .get(&(Exchange::IBKR, symbol.clone()))
            .copied()
            .unwrap_or((0.0, 0.0));

        let signal_type = Self::parse_signal_type(comment);
        let direction = Self::derive_direction(&tracked);

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

        let signal_model = db_signal::ActiveModel {
            created_at: Set(chrono::Utc::now()),
            perp_symbol: Set(symbol.clone()),
            spot_symbol: Set(symbol.clone()),
            signal_type: Set(signal_type.to_string()),
            direction: Set(direction.to_string()),
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
            let (order_type_str, tif_str) = Self::format_order_type(&order.order_type);

            let db_order = db_order::ActiveModel {
                created_at: Set(chrono::Utc::now()),
                opportunity_id: Set(signal_id),
                exchange: Set(order.exchange.to_string()),
                symbol: Set(order.symbol.clone()),
                client_order_id: Set(order.client_order_id.clone()),
                external_order_id: Set(None),
                side: Set(order.side.to_string()),
                quantity: Set(order.quantity),
                price: Set(price),
                order_type: Set(order_type_str.to_string()),
                time_in_force: Set(tif_str.to_string()),
                status: Set("pending".to_string()),
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
            None => return,
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
            external_fill_id: Set(Some(fill.order_id.clone())),
            exchange: Set(fill.exchange.to_string()),
            symbol: Set(fill.symbol.clone()),
            side: Set(fill.side.to_string()),
            price: Set(fill.price),
            quantity: Set(fill.size),
            fee: Set(0.0),
            fill_timestamp: Set(Some(fill.timestamp as i64)),
            ..Default::default()
        };
        if let Err(e) = db_fill.insert(&self.db).await {
            tracing::error!(error = %e, "Failed to insert fill");
        }

        // 更新 order filled_qty
        let new_filled_qty = order.filled_qty + fill.size;
        let mut order_am: db_order::ActiveModel = order.into();
        order_am.filled_qty = Set(new_filled_qty);
        if let Err(e) = order_am.update(&self.db).await {
            tracing::error!(error = %e, "Failed to update order filled_qty");
        }
    }

    /// OrderUpdate (terminal) → 更新 order status
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
            None => return,
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

        let status_str = Self::format_status(&update.status);
        let error_msg = match &update.status {
            OrderStatus::Rejected { reason } | OrderStatus::Error { reason } => {
                Some(reason.clone())
            }
            _ => None,
        };

        let mut order_am: db_order::ActiveModel = order.into();
        order_am.status = Set(status_str.to_string());
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
        }
    }
}
