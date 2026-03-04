use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "arbitrage_opportunities")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub created_at: DateTimeUtc,
    #[sea_orm(column_type = "String(StringLen::N(50))")]
    pub perp_symbol: String,
    #[sea_orm(column_type = "String(StringLen::N(50))")]
    pub spot_symbol: String,
    pub signal_type: super::enums::SignalType,
    pub direction: super::enums::Direction,
    pub perp_bid: f64,
    pub perp_ask: f64,
    pub spot_bid: f64,
    pub spot_ask: f64,
    pub open_spread_pct: f64,
    pub close_spread_pct: f64,
    pub planned_trade_size: f64,
    pub planned_quantity: f64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
