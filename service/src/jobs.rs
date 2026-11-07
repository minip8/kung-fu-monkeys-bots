//! The job queue: the `jobs` table (architecture v0.63 §11: "the jobs table
//! is the queue"; no broker).

use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// What a job does; the worker roles claim by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A submission's source into a module (`{"submission": id}`).
    Build,
    /// A built module through the checks and the smoke match (`{"submission": id}`).
    Validate,
    /// One two-sided `kfm_arena` run (step 4).
    Match,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Build, Kind::Validate, Kind::Match];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Build => "build",
            Kind::Validate => "validate",
            Kind::Match => "match",
        }
    }

    pub fn parse(text: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// Queues a job inside `tx` (committed with the row it is about); returns its id.
pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    kind: Kind,
    payload: Value,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO jobs (id, kind, payload) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(kind.as_str())
        .bind(payload)
        .execute(&mut **tx)
        .await?;
    Ok(id)
}
