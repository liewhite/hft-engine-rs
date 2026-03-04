pub mod enums;
pub mod fill;
pub mod order;
pub mod signal;

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Schema};

pub async fn init_db(url: &str) -> anyhow::Result<DatabaseConnection> {
    let db = Database::connect(url).await?;

    let backend = db.get_database_backend();
    let schema = Schema::new(backend);

    let stmts = vec![
        schema.create_table_from_entity(signal::Entity),
        schema.create_table_from_entity(order::Entity),
        schema.create_table_from_entity(fill::Entity),
    ];

    for mut stmt in stmts {
        stmt.if_not_exists();
        db.execute(&stmt).await?;
    }

    tracing::info!("Database initialized, tables synced");
    Ok(db)
}
