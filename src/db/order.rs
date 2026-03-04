use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub created_at: DateTimeUtc,
    pub opportunity_id: i32,
    #[sea_orm(column_type = "String(StringLen::N(20))")]
    pub exchange: String,
    #[sea_orm(column_type = "String(StringLen::N(50))")]
    pub symbol: String,
    #[sea_orm(column_type = "String(StringLen::N(100))")]
    pub client_order_id: String,
    #[sea_orm(column_type = "String(StringLen::N(100))", nullable)]
    pub external_order_id: Option<String>,
    pub side: super::enums::DbSide,
    pub quantity: f64,
    pub price: f64,
    pub order_type: super::enums::DbOrderType,
    pub time_in_force: super::enums::DbTimeInForce,
    pub status: super::enums::DbOrderStatus,
    pub filled_qty: f64,
    pub avg_price: f64,
    #[sea_orm(column_type = "Text", nullable)]
    pub error_message: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
