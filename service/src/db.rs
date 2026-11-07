//! The pool and the migrations (`service/migrations/`, embedded at build time).

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Connects to `url`, retrying for up to `patience` (Postgres may still be
/// starting when the service's unit starts).
pub async fn connect(url: &str, patience: Duration) -> Result<PgPool, sqlx::Error> {
    let deadline = tokio::time::Instant::now() + patience;
    let mut delay = Duration::from_millis(250);
    loop {
        let attempt = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await;
        match attempt {
            Ok(pool) => return Ok(pool),
            Err(err) if tokio::time::Instant::now() + delay < deadline => {
                tracing::warn!(error = %err, "database not reachable yet; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Applies every pending migration. Concurrent callers (the API and the
/// workers starting together) serialise on sqlx's advisory lock.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}
