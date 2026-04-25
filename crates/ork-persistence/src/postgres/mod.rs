pub mod a2a_push_dead_letter_repo;
pub mod a2a_push_repo;
pub mod a2a_signing_key_repo;
pub mod a2a_task_repo;
pub mod tenant_repo;
pub mod workflow_repo;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub async fn create_pool(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
}
