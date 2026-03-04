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
    #[sea_orm(column_type = "String(StringLen::N(10))")]
    pub side: String,
    pub quantity: f64,
    pub price: f64,
    #[sea_orm(column_type = "String(StringLen::N(20))")]
    pub order_type: String,
    #[sea_orm(column_type = "String(StringLen::N(10))")]
    pub time_in_force: String,
    #[sea_orm(column_type = "String(StringLen::N(20))")]
    pub status: String,
    pub filled_qty: f64,
    pub avg_price: f64,
    #[sea_orm(column_type = "Text", nullable)]
    pub error_message: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
