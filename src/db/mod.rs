pub mod enums;
pub mod fill;
pub mod order;
pub mod signal;

use sea_orm::{Database, DatabaseConnection};

pub async fn init_db(url: &str) -> anyhow::Result<DatabaseConnection> {
    let db = Database::connect(url).await?;

    db.get_schema_registry("fee_arb::db::*").sync(&db).await?;

    tracing::info!("Database initialized, schema synced");
    Ok(db)
}
