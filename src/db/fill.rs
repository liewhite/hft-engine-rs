use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "fills")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub created_at: DateTimeUtc,
    pub order_id: i32,
    #[sea_orm(column_type = "String(StringLen::N(100))", nullable)]
    pub external_fill_id: Option<String>,
    #[sea_orm(column_type = "String(StringLen::N(20))")]
    pub exchange: String,
    #[sea_orm(column_type = "String(StringLen::N(50))")]
    pub symbol: String,
    #[sea_orm(column_type = "String(StringLen::N(10))")]
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub fee: f64,
    pub fill_timestamp: Option<i64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
